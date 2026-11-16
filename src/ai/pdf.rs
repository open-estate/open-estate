// Pure-Rust PDF & On-Demand Local OCR Pipeline powered strictly by permissive open-source
// components (`lopdf` MIT + `pdf-extract` MIT + local `tesseract-ocr` Apache-2.0 — zero AGPL!).
// - Tier 1 (`lopdf`) & Tier 2 (`pdf_extract`): <2ms pure-Rust digital text extraction when pages
//   already contain >= 25 selectable characters (skips OCR completely!).
// - Tier 3 (`lopdf+tesseract_ocr` / `pdftoppm+tesseract_ocr`): Triggered ONLY when a PDF page yields
//   < 25 selectable characters AND contains embedded `/Subtype /Image` XObjects (or when an entire
//   vector/form-flattened PDF yields < 25 selectable chars and `pdftoppm` is available on `PATH`).
//   Extracts JPEG/JP2 streams directly or synthesizes 20-byte Netpbm (`P4` PBM / `P5` PGM / `P6` PPM)
//   headers in pure Rust with zero extra crates, piping bytes to `tesseract stdin stdout -l eng --psm 6`
//   under strict process concurrency & timeout limits.
// - Retains exact `[Page N]` citations per page so local prompt-injection guardrails AND local Title
//   PII redaction (`redact_title_pii`) execute BEFORE any text ever leaves the server to an LLM API.

use crate::ai::llm::{
    extract_response_text, extract_usage, resolve_llm_spec_for_call, ModelSpec,
};
use crate::guardrails::{
    scrub_sensitive_text, scrub_untrusted_document_lines, validate_and_scrub_property_document,
    DocumentKind, GuardrailError, IMMUTABLE_SYSTEM_PREAMBLE,
};
use crate::shared::{EnvConfig, HttpClient};
use anyhow::{anyhow, Result};
use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
    Engine as _,
};
// [feat/hoa-packet-ai]
use flate2::read::DeflateDecoder;
// [/feat/hoa-packet-ai]
use lopdf::{
    content::{Content, Operation},
    Dictionary, Document, Object, ObjectId, Stream,
};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const MAX_SINGLE_PDF_BYTES: usize = 12 * 1024 * 1024; // 12 MB per uploaded PDF
// [feat/hoa-packet-ai]
pub const MAX_ZIP_ARCHIVE_BYTES: usize = 24 * 1024 * 1024; // 24 MB per uploaded .zip packet
pub const MAX_ZIP_CENTRAL_ENTRIES: usize = 64;
pub const MAX_ZIP_TOTAL_UNCOMPRESSED_BYTES: usize = 48 * 1024 * 1024; // 48 MB total decompressed cap
pub const MAX_ZIP_COMPRESSION_RATIO: usize = 40; // Reject >40:1 compression ratio above exempt floor
pub const ZIP_RATIO_MIN_EXEMPT_BYTES: usize = 64 * 1024; // 64 KB floor before ratio enforcement
pub const MAX_PACKET_DOCUMENTS: usize = 20; // Up to 20 PDFs in a multi-doc disclosure packet
// [/feat/hoa-packet-ai]
pub const MAX_PDF_PAGES: usize = 150;
pub const MAX_PAGE_DECOMPRESSED_BYTES: usize = 8 * 1024 * 1024;

/// Minimum selectable characters on a PDF page before on-demand OCR is skipped for that page.
pub const MIN_SELECTABLE_PAGE_CHARS: usize = 25;
/// Bomb-safe cap per decompressed image XObject stream (36 MB fits a 300-DPI 24-bit US Letter scan).
pub const MAX_IMAGE_DECOMPRESSED_BYTES: usize = 36 * 1024 * 1024;
/// Maximum scanned pages OCR'd in a single uploaded PDF to bound worst-case latency.
pub const MAX_OCR_PAGES_PER_DOC: usize = 40;
/// Maximum embedded image XObjects OCR'd on a single PDF page.
pub const MAX_OCR_IMAGES_PER_PAGE: usize = 4;
/// Default wall-clock timeout per `tesseract` / `pdftoppm` subprocess invocation.
pub const DEFAULT_OCR_PAGE_TIMEOUT_MS: u64 = 12_000;
/// Default wall-clock timeout per multimodal LLM Vision OCR `/responses` request.
pub const DEFAULT_LLM_VISION_OCR_TIMEOUT_MS: u64 = 90_000;
/// Standard multimodal Vision OCR prompt for `POST {OPENAI_BASE_URL}/responses`.
pub const LLM_VISION_OCR_PROMPT: &str = "Perform accurate page-by-page OCR of this residential property document. Prefix each page with [Page N] and transcribe all text, tables, checkboxes, and inspector photo captions faithfully.";
/// Process-wide cap on simultaneous OCR subprocesses so concurrent uploads never starve CPU cores.
pub const MAX_CONCURRENT_OCR_JOBS: usize = 2;
/// Maximum stdout bytes read from a single OCR page invocation (512 KB).
const MAX_OCR_STDOUT_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UploadedDocInput {
    #[serde(default = "default_doc_name")]
    pub name: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub base64: Option<String>,
}

fn default_doc_name() -> String {
    "uploaded-document.pdf".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PageExtract {
    pub page_number: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractedDocument {
    pub name: String,
    pub pages: Vec<PageExtract>,
    pub formatted_text: String,
    pub chars: usize,
    pub method: String,
}

#[derive(Debug, Clone)]
pub struct NormalizedUploadBatch {
    pub files: Vec<Value>,
    pub documents: Vec<ExtractedDocument>,
    pub sanitized_text: String,
    pub scrubbed_lines: usize,
    pub pii_redacted_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PdfImageFormat {
    /// Raw JPEG stream (`/Filter /DCTDecode`)
    Jpeg,
    /// Raw JPEG 2000 stream (`/Filter /JPXDecode`)
    Jpeg2000,
    /// Netpbm `P4` binary 1-bit monochrome (`1` = black, `0` = white)
    Pbm,
    /// Netpbm `P5` binary 8-bit grayscale (`0` = black, `255` = white)
    Pgm,
    /// Netpbm `P6` binary 24-bit RGB
    Ppm,
}

impl PdfImageFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Jpeg2000 => "jp2",
            Self::Pbm => "pbm",
            Self::Pgm => "pgm",
            Self::Ppm => "ppm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPdfImage {
    pub page_number: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub bits_per_component: u8,
    pub format: PdfImageFormat,
    /// Complete image payload (raw JPEG/JP2 bytes or Netpbm `P4`/`P5`/`P6` header + pixel raster)
    /// directly readable by `tesseract stdin stdout -l eng --psm 6`.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct OcrConfig {
    pub tesseract_bin: String,
    pub pdftoppm_bin: String,
    pub timeout: Duration,
    pub max_pages: usize,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl OcrConfig {
    pub fn from_env() -> Self {
        let tesseract_bin = std::env::var("TESSERACT_BIN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "tesseract".to_string());
        let pdftoppm_bin = std::env::var("PDFTOPPM_BIN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "pdftoppm".to_string());
        let timeout_ms = std::env::var("OPEN_ESTATE_OCR_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&ms| (50..=120_000).contains(&ms))
            .unwrap_or(DEFAULT_OCR_PAGE_TIMEOUT_MS);
        Self {
            tesseract_bin,
            pdftoppm_bin,
            timeout: Duration::from_millis(timeout_ms),
            max_pages: MAX_OCR_PAGES_PER_DOC,
        }
    }

    pub fn from_env_config(env: &EnvConfig) -> Self {
        let base = Self::from_env();
        let tesseract_bin = env
            .get("TESSERACT_BIN")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or(base.tesseract_bin);
        let pdftoppm_bin = env
            .get("PDFTOPPM_BIN")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or(base.pdftoppm_bin);
        let timeout = env
            .get("OPEN_ESTATE_OCR_TIMEOUT_MS")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&ms| (50..=120_000).contains(&ms))
            .map(Duration::from_millis)
            .unwrap_or(base.timeout);
        Self {
            tesseract_bin,
            pdftoppm_bin,
            timeout,
            max_pages: base.max_pages,
        }
    }
}

static DATA_URL_PREFIX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)^data:[^;,]+(?:;[^;,]+)*;base64,").expect("data url regex"));
static MULTI_HSPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t]{2,}").unwrap());
static MULTI_BLANK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());
static INLINE_PAGE_MARKER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?im)^\s*(?:#{1,4}\s*|\*\*\s*)?\[Page\s+(\d+)\](?:\s*\*\*)?[ \t:—\-]*").unwrap()
});

static DEFAULT_TESSERACT_AVAILABLE: Lazy<bool> = Lazy::new(|| probe_binary("tesseract", "--version"));
static DEFAULT_PDFTOPPM_AVAILABLE: Lazy<bool> = Lazy::new(|| probe_binary("pdftoppm", "-v"));

struct OcrSemaphore {
    active: Mutex<usize>,
    cvar: Condvar,
    limit: usize,
}

struct OcrPermit<'a> {
    sem: &'a OcrSemaphore,
}

impl OcrSemaphore {
    fn new(limit: usize) -> Self {
        Self {
            active: Mutex::new(0),
            cvar: Condvar::new(),
            limit: limit.max(1),
        }
    }

    fn acquire(&self, wait_timeout: Duration) -> Result<OcrPermit<'_>> {
        let deadline = Instant::now() + wait_timeout;
        let mut guard = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *guard >= self.limit {
            let now = Instant::now();
            if now >= deadline {
                return Err(anyhow!(
                    "OCR worker pool busy (exceeded {} concurrent jobs)",
                    self.limit
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_guard, timed_out) = self
                .cvar
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard = next_guard;
            if timed_out.timed_out() && *guard >= self.limit {
                return Err(anyhow!(
                    "OCR worker pool busy (exceeded {} concurrent jobs)",
                    self.limit
                ));
            }
        }
        *guard += 1;
        Ok(OcrPermit { sem: self })
    }
}

impl Drop for OcrPermit<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .sem
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = guard.saturating_sub(1);
        self.sem.cvar.notify_one();
    }
}

static OCR_SEMAPHORE: Lazy<OcrSemaphore> = Lazy::new(|| OcrSemaphore::new(MAX_CONCURRENT_OCR_JOBS));

fn probe_binary(bin: &str, flag: &str) -> bool {
    Command::new(bin)
        .arg(flag)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn is_tesseract_available(cfg: &OcrConfig) -> bool {
    if cfg.tesseract_bin == "tesseract" {
        *DEFAULT_TESSERACT_AVAILABLE
    } else {
        let p = Path::new(&cfg.tesseract_bin);
        if p.components().count() > 1 {
            p.is_file()
        } else {
            probe_binary(&cfg.tesseract_bin, "--version")
        }
    }
}

fn is_pdftoppm_available(cfg: &OcrConfig) -> bool {
    if cfg.pdftoppm_bin == "pdftoppm" {
        *DEFAULT_PDFTOPPM_AVAILABLE
    } else {
        let p = Path::new(&cfg.pdftoppm_bin);
        if p.components().count() > 1 {
            p.is_file()
        } else {
            probe_binary(&cfg.pdftoppm_bin, "-v")
        }
    }
}

fn normalize_page_text(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c == '\r' {
                '\n'
            } else if c == '\n' || c == '\t' || (c >= ' ' && c != '\u{007f}') {
                c
            } else {
                ' '
            }
        })
        .collect();
    let lines: Vec<String> = cleaned
        .lines()
        .map(|l| MULTI_HSPACE_RE.replace_all(l.trim(), " ").into_owned())
        .collect();
    let joined = lines.join("\n");
    MULTI_BLANK_RE
        .replace_all(joined.trim(), "\n\n")
        .into_owned()
}

/// Decodes a `data:application/pdf;base64,...` data URL or raw base64 payload with strict byte-size enforcement.
pub fn decode_base64_payload(raw_b64: &str) -> std::result::Result<Vec<u8>, GuardrailError> {
    decode_base64_payload_with_limit(
        raw_b64,
        MAX_SINGLE_PDF_BYTES,
        "Uploaded PDF exceeds the 12 MB per-document size limit.",
    )
}

// [feat/hoa-packet-ai]
/// Decodes a base64 / data-URL payload up to `max_bytes`.
pub fn decode_base64_payload_with_limit(
    raw_b64: &str,
    max_bytes: usize,
    limit_message: &str,
) -> std::result::Result<Vec<u8>, GuardrailError> {
    let stripped_prefix = DATA_URL_PREFIX_RE.replace(raw_b64.trim(), "");
    let compact: String = stripped_prefix
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    if compact.is_empty() {
        return Err(GuardrailError::new(
            "EMPTY_DOCUMENT",
            "Uploaded base64 document payload is empty.",
            400,
        ));
    }
    if compact.len() > (max_bytes * 4 / 3) + 1024 {
        return Err(GuardrailError::new("DOCUMENT_TOO_LARGE", limit_message, 413));
    }
    let bytes = STANDARD
        .decode(&compact)
        .or_else(|_| STANDARD_NO_PAD.decode(&compact))
        .or_else(|_| URL_SAFE.decode(&compact))
        .or_else(|_| URL_SAFE_NO_PAD.decode(&compact))
        .map_err(|_| {
            GuardrailError::new(
                "INVALID_BASE64",
                "Uploaded document is not valid base64 data.",
                400,
            )
        })?;
    if bytes.len() > max_bytes {
        return Err(GuardrailError::new("DOCUMENT_TOO_LARGE", limit_message, 413));
    }
    Ok(bytes)
}

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Pure-Rust IEEE 802.3 CRC-32 checksum used to verify extracted ZIP archive entries.
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        let idx = ((crc ^ (b as u32)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    !crc
}

#[derive(Debug, Clone)]
struct ZipCentralEntry {
    raw_name: String,
    method: u16,
    crc32: u32,
    comp_size: usize,
    uncomp_size: usize,
    local_offset: usize,
    data_start: usize,
    data_end: usize,
    is_ignorable_meta: bool,
    is_supported_doc: bool,
}

fn validate_safe_zip_entry_path(raw_name: &str) -> std::result::Result<(bool, bool, String), GuardrailError> {
    if raw_name.is_empty() || raw_name.contains('\0') {
        return Err(GuardrailError::new(
            "ZIP_PATH_TRAVERSAL",
            "ZIP archive contains an empty or null-byte filename.",
            400,
        ));
    }
    if raw_name.starts_with('/') || raw_name.starts_with('\\') || raw_name.contains(':') {
        return Err(GuardrailError::new(
            "ZIP_PATH_TRAVERSAL",
            "ZIP archive contains an unsafe absolute or drive-prefixed path.",
            400,
        ));
    }
    let normalized = raw_name.replace('\\', "/");
    for seg in normalized.split('/') {
        if seg == ".." {
            return Err(GuardrailError::new(
                "ZIP_PATH_TRAVERSAL",
                "ZIP archive contains path traversal (`..`) segments.",
                400,
            ));
        }
    }

    if normalized.ends_with('/') {
        return Ok((true, false, String::new()));
    }

    let basename = normalized
        .rsplit('/')
        .next()
        .unwrap_or(normalized.as_str())
        .trim();
    let lower_full = normalized.to_ascii_lowercase();
    let lower_base = basename.to_ascii_lowercase();

    // Reject nested archives (recursive zip-bomb defense)
    for nested_ext in [
        ".zip", ".tar", ".gz", ".tgz", ".bz2", ".xz", ".7z", ".rar", ".zst",
    ] {
        if lower_base.ends_with(nested_ext) {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                "Nested archives inside uploaded ZIP disclosure packets are prohibited.",
                400,
            ));
        }
    }

    // Skip macOS / Windows directory clutter
    if lower_full.starts_with("__macosx/")
        || lower_full.contains("/__macosx/")
        || lower_base == ".ds_store"
        || lower_base == "thumbs.db"
        || lower_base == "desktop.ini"
        || lower_base.starts_with("._")
        || basename.is_empty()
    {
        return Ok((true, false, basename.to_string()));
    }

    let is_doc = lower_base.ends_with(".pdf")
        || lower_base.ends_with(".txt")
        || lower_base.ends_with(".md");
    Ok((false, is_doc, basename.to_string()))
}

fn find_zip_eocd(bytes: &[u8]) -> std::result::Result<usize, GuardrailError> {
    if bytes.len() < 22 {
        return Err(GuardrailError::new(
            "INVALID_ZIP_ARCHIVE",
            "Uploaded ZIP archive is truncated or corrupt.",
            400,
        ));
    }
    let min_pos = bytes.len().saturating_sub(65_557);
    let mut pos = bytes.len() - 22;
    loop {
        if bytes[pos..pos + 4] == [0x50, 0x4B, 0x05, 0x06] {
            let comment_len = u16::from_le_bytes([bytes[pos + 20], bytes[pos + 21]]) as usize;
            if pos + 22 + comment_len == bytes.len() {
                return Ok(pos);
            }
        }
        if pos == min_pos {
            break;
        }
        pos -= 1;
    }
    Err(GuardrailError::new(
        "INVALID_ZIP_ARCHIVE",
        "Could not locate a valid End-of-Central-Directory record in uploaded ZIP archive.",
        400,
    ))
}

/// Pure-Rust ZIP central-directory unpacker (`Stored` + `Deflate` via `flate2`) with strict
/// zip-bomb defenses: entry-count cap, path-traversal block, nested-archive block, non-overlapping
/// local-header verification (Fifield `zbsm` defense), declared & streaming 40:1 compression-ratio
/// enforcement, 12 MB per-file & 48 MB total uncompressed caps, and IEEE CRC-32 verification.
pub fn unpack_zip_archive_bytes(
    zip_name: &str,
    bytes: &[u8],
    max_docs: usize,
) -> std::result::Result<Vec<UploadedDocInput>, GuardrailError> {
    if bytes.len() > MAX_ZIP_ARCHIVE_BYTES {
        return Err(GuardrailError::new(
            "DOCUMENT_TOO_LARGE",
            "Uploaded ZIP packet exceeds the 24 MB archive size limit.",
            413,
        ));
    }

    let eocd_pos = find_zip_eocd(bytes)?;
    let disk_no = u16::from_le_bytes([bytes[eocd_pos + 4], bytes[eocd_pos + 5]]);
    let cd_disk = u16::from_le_bytes([bytes[eocd_pos + 6], bytes[eocd_pos + 7]]);
    let disk_entries = u16::from_le_bytes([bytes[eocd_pos + 8], bytes[eocd_pos + 9]]);
    let total_entries = u16::from_le_bytes([bytes[eocd_pos + 10], bytes[eocd_pos + 11]]) as usize;
    let cd_size = u32::from_le_bytes([
        bytes[eocd_pos + 12],
        bytes[eocd_pos + 13],
        bytes[eocd_pos + 14],
        bytes[eocd_pos + 15],
    ]) as usize;
    let cd_offset = u32::from_le_bytes([
        bytes[eocd_pos + 16],
        bytes[eocd_pos + 17],
        bytes[eocd_pos + 18],
        bytes[eocd_pos + 19],
    ]) as usize;

    if disk_no != 0 || cd_disk != 0 || disk_entries as usize != total_entries {
        return Err(GuardrailError::new(
            "INVALID_ZIP_ARCHIVE",
            "Multi-disk ZIP archives are not supported.",
            400,
        ));
    }
    if total_entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
        return Err(GuardrailError::new(
            "ZIP_BOMB_BLOCKED",
            "ZIP64 archives are blocked by disclosure packet safety limits.",
            400,
        ));
    }
    if total_entries == 0 {
        return Err(GuardrailError::new(
            "EMPTY_ZIP_ARCHIVE",
            format!("Uploaded archive \"{zip_name}\" contains no files."),
            400,
        ));
    }
    if total_entries > MAX_ZIP_CENTRAL_ENTRIES {
        return Err(GuardrailError::new(
            "ZIP_BOMB_BLOCKED",
            format!(
                "ZIP archive contains {total_entries} entries (maximum allowed is {MAX_ZIP_CENTRAL_ENTRIES})."
            ),
            400,
        ));
    }
    let Some(cd_end) = cd_offset.checked_add(cd_size) else {
        return Err(GuardrailError::new(
            "INVALID_ZIP_ARCHIVE",
            "Corrupt central directory bounds in ZIP archive.",
            400,
        ));
    };
    if cd_end > eocd_pos {
        return Err(GuardrailError::new(
            "INVALID_ZIP_ARCHIVE",
            "Central directory extends past End-of-Central-Directory record.",
            400,
        ));
    }

    let mut cursor = cd_offset;
    let mut parsed_entries = Vec::with_capacity(total_entries);
    let mut declared_total_uncompressed: usize = 0;

    for _ in 0..total_entries {
        if cursor + 46 > cd_end || bytes[cursor..cursor + 4] != [0x50, 0x4B, 0x01, 0x02] {
            return Err(GuardrailError::new(
                "INVALID_ZIP_ARCHIVE",
                "Corrupt Central Directory entry header in ZIP archive.",
                400,
            ));
        }
        let gp_flags = u16::from_le_bytes([bytes[cursor + 8], bytes[cursor + 9]]);
        if (gp_flags & 0x0001) != 0 {
            return Err(GuardrailError::new(
                "ZIP_ENCRYPTED_ENTRY",
                "Password-protected or encrypted ZIP archives cannot be analyzed.",
                400,
            ));
        }
        let method = u16::from_le_bytes([bytes[cursor + 10], bytes[cursor + 11]]);
        if method != 0 && method != 8 {
            return Err(GuardrailError::new(
                "ZIP_UNSUPPORTED_COMPRESSION",
                format!("Unsupported ZIP compression method ({method}); only Stored and Deflate are supported."),
                400,
            ));
        }
        let crc32 = u32::from_le_bytes([
            bytes[cursor + 16],
            bytes[cursor + 17],
            bytes[cursor + 18],
            bytes[cursor + 19],
        ]);
        let comp_u32 = u32::from_le_bytes([
            bytes[cursor + 20],
            bytes[cursor + 21],
            bytes[cursor + 22],
            bytes[cursor + 23],
        ]);
        let uncomp_u32 = u32::from_le_bytes([
            bytes[cursor + 24],
            bytes[cursor + 25],
            bytes[cursor + 26],
            bytes[cursor + 27],
        ]);
        if comp_u32 == 0xFFFF_FFFF || uncomp_u32 == 0xFFFF_FFFF {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                "ZIP64 entry sizes are blocked by packet zip-bomb guardrails.",
                400,
            ));
        }
        let comp_size = comp_u32 as usize;
        let uncomp_size = uncomp_u32 as usize;
        let name_len = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[cursor + 30], bytes[cursor + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[cursor + 32], bytes[cursor + 33]]) as usize;
        let local_offset = u32::from_le_bytes([
            bytes[cursor + 42],
            bytes[cursor + 43],
            bytes[cursor + 44],
            bytes[cursor + 45],
        ]) as usize;

        let header_end = cursor
            .checked_add(46)
            .and_then(|v| v.checked_add(name_len))
            .and_then(|v| v.checked_add(extra_len))
            .and_then(|v| v.checked_add(comment_len))
            .ok_or_else(|| {
                GuardrailError::new("INVALID_ZIP_ARCHIVE", "Corrupt ZIP header lengths.", 400)
            })?;
        if header_end > cd_end {
            return Err(GuardrailError::new(
                "INVALID_ZIP_ARCHIVE",
                "Central directory filename/extra fields exceed directory bounds.",
                400,
            ));
        }

        let raw_name = std::str::from_utf8(&bytes[cursor + 46..cursor + 46 + name_len])
            .map(|s| s.to_string())
            .unwrap_or_else(|_| {
                String::from_utf8_lossy(&bytes[cursor + 46..cursor + 46 + name_len]).into_owned()
            });
        cursor = header_end;

        let (is_ignorable_meta, is_supported_doc, display_base) =
            validate_safe_zip_entry_path(&raw_name)?;

        // Enforce zip-bomb size & compression ratio invariants on EVERY entry (even skipped metadata)
        if uncomp_size > MAX_SINGLE_PDF_BYTES {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                format!(
                    "ZIP entry \"{}\" exceeds the 12 MB per-document uncompressed limit.",
                    display_base
                ),
                413,
            ));
        }
        declared_total_uncompressed = declared_total_uncompressed.saturating_add(uncomp_size);
        if declared_total_uncompressed > MAX_ZIP_TOTAL_UNCOMPRESSED_BYTES {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                "ZIP archive exceeds the 48 MB total uncompressed budget (zip-bomb defense).",
                413,
            ));
        }
        if uncomp_size > ZIP_RATIO_MIN_EXEMPT_BYTES
            && (comp_size == 0
                || uncomp_size > comp_size.saturating_mul(MAX_ZIP_COMPRESSION_RATIO))
        {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                format!(
                    "ZIP entry \"{}\" compression ratio exceeds {MAX_ZIP_COMPRESSION_RATIO}:1 safety threshold.",
                    display_base
                ),
                400,
            ));
        }

        // Validate corresponding Local File Header (`0x04034b50`)
        if local_offset + 30 > cd_offset
            || bytes[local_offset..local_offset + 4] != [0x50, 0x4B, 0x03, 0x04]
        {
            return Err(GuardrailError::new(
                "INVALID_ZIP_ARCHIVE",
                "Invalid or out-of-bounds Local File Header in ZIP archive.",
                400,
            ));
        }
        let lfh_name_len =
            u16::from_le_bytes([bytes[local_offset + 26], bytes[local_offset + 27]]) as usize;
        let lfh_extra_len =
            u16::from_le_bytes([bytes[local_offset + 28], bytes[local_offset + 29]]) as usize;
        let data_start = local_offset
            .checked_add(30)
            .and_then(|v| v.checked_add(lfh_name_len))
            .and_then(|v| v.checked_add(lfh_extra_len))
            .ok_or_else(|| {
                GuardrailError::new("INVALID_ZIP_ARCHIVE", "Corrupt Local File Header.", 400)
            })?;
        let data_end = data_start.checked_add(comp_size).ok_or_else(|| {
            GuardrailError::new("INVALID_ZIP_ARCHIVE", "Corrupt compressed entry size.", 400)
        })?;
        if data_end > cd_offset {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                "ZIP entry compressed stream overlaps Central Directory (zip-bomb defense).",
                400,
            ));
        }

        parsed_entries.push(ZipCentralEntry {
            raw_name: if display_base.is_empty() {
                raw_name
            } else {
                display_base
            },
            method,
            crc32,
            comp_size,
            uncomp_size,
            local_offset,
            data_start,
            data_end,
            is_ignorable_meta,
            is_supported_doc,
        });
    }

    // Verify zero overlapping `[local_offset, data_end)` extents (blocks non-recursive Fifield `zbsm` zip bombs)
    let mut extents: Vec<(usize, usize)> = parsed_entries
        .iter()
        .map(|e| (e.local_offset, e.data_end.max(e.local_offset + 30)))
        .collect();
    extents.sort_by_key(|&(start, _)| start);
    for pair in extents.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                "ZIP archive contains overlapping local file headers (shared-kernel zip-bomb defense).",
                400,
            ));
        }
    }

    let doc_entries: Vec<&ZipCentralEntry> = parsed_entries
        .iter()
        .filter(|e| !e.is_ignorable_meta && e.is_supported_doc && e.uncomp_size > 0)
        .collect();

    if doc_entries.is_empty() {
        return Err(GuardrailError::new(
            "EMPTY_ZIP_ARCHIVE",
            format!("ZIP archive \"{zip_name}\" contains no readable PDF or text disclosure files."),
            400,
        ));
    }
    if doc_entries.len() > max_docs {
        return Err(GuardrailError::new(
            "TOO_MANY_DOCUMENTS",
            format!(
                "ZIP archive contains {} documents; up to {max_docs} documents may be analyzed per packet.",
                doc_entries.len()
            ),
            400,
        ));
    }

    let mut total_actual_bytes: usize = 0;
    let mut used_names: HashSet<String> = HashSet::new();
    let mut unpacked_docs = Vec::with_capacity(doc_entries.len());

    for entry in doc_entries {
        let comp_slice = &bytes[entry.data_start..entry.data_end];
        let mut out_buf = Vec::with_capacity(entry.uncomp_size.min(256 * 1024));

        if entry.method == 0 {
            if entry.comp_size != entry.uncomp_size {
                return Err(GuardrailError::new(
                    "INVALID_ZIP_ARCHIVE",
                    "Stored ZIP entry has mismatched compressed and uncompressed byte lengths.",
                    400,
                ));
            }
            total_actual_bytes = total_actual_bytes.saturating_add(comp_slice.len());
            if total_actual_bytes > MAX_ZIP_TOTAL_UNCOMPRESSED_BYTES {
                return Err(GuardrailError::new(
                    "ZIP_BOMB_BLOCKED",
                    "ZIP decompression exceeded 48 MB total safety ceiling.",
                    413,
                ));
            }
            out_buf.extend_from_slice(comp_slice);
        } else {
            let mut decoder = DeflateDecoder::new(comp_slice);
            let mut chunk = [0u8; 16 * 1024];
            loop {
                let n = decoder.read(&mut chunk).map_err(|_| {
                    GuardrailError::new(
                        "INVALID_ZIP_ARCHIVE",
                        format!("Failed to inflate Deflate stream for \"{}\".", entry.raw_name),
                        400,
                    )
                })?;
                if n == 0 {
                    break;
                }
                if out_buf.len().saturating_add(n) > MAX_SINGLE_PDF_BYTES {
                    return Err(GuardrailError::new(
                        "ZIP_BOMB_BLOCKED",
                        format!(
                            "Inflated stream for \"{}\" exceeded 12 MB per-file safety limit.",
                            entry.raw_name
                        ),
                        413,
                    ));
                }
                total_actual_bytes = total_actual_bytes.saturating_add(n);
                if total_actual_bytes > MAX_ZIP_TOTAL_UNCOMPRESSED_BYTES {
                    return Err(GuardrailError::new(
                        "ZIP_BOMB_BLOCKED",
                        "Inflated ZIP packet exceeded 48 MB total uncompressed safety limit.",
                        413,
                    ));
                }
                out_buf.extend_from_slice(&chunk[..n]);
                if out_buf.len() > ZIP_RATIO_MIN_EXEMPT_BYTES
                    && (entry.comp_size == 0
                        || out_buf.len()
                            > entry.comp_size.saturating_mul(MAX_ZIP_COMPRESSION_RATIO))
                {
                    return Err(GuardrailError::new(
                        "ZIP_BOMB_BLOCKED",
                        format!(
                            "Streaming inflation of \"{}\" exceeded {MAX_ZIP_COMPRESSION_RATIO}:1 compression ratio limit.",
                            entry.raw_name
                        ),
                        400,
                    ));
                }
            }
        }

        if out_buf.len() != entry.uncomp_size {
            return Err(GuardrailError::new(
                "ZIP_BOMB_BLOCKED",
                format!(
                    "ZIP entry \"{}\" actual inflated length ({}) does not match declared header ({}).",
                    entry.raw_name,
                    out_buf.len(),
                    entry.uncomp_size
                ),
                400,
            ));
        }
        let actual_crc = crc32_ieee(&out_buf);
        if actual_crc != entry.crc32 {
            return Err(GuardrailError::new(
                "INVALID_ZIP_ARCHIVE",
                format!("ZIP entry \"{}\" failed CRC-32 integrity check.", entry.raw_name),
                400,
            ));
        }

        let mut unique_name = entry.raw_name.clone();
        if !used_names.insert(unique_name.to_ascii_lowercase()) {
            let mut idx = 2usize;
            loop {
                let candidate = match unique_name.rfind('.') {
                    Some(dot) => format!("{}-{idx}{}", &unique_name[..dot], &unique_name[dot..]),
                    None => format!("{unique_name}-{idx}"),
                };
                if used_names.insert(candidate.to_ascii_lowercase()) {
                    unique_name = candidate;
                    break;
                }
                idx += 1;
            }
        }

        let looks_pdf = out_buf.starts_with(b"%PDF")
            || unique_name.to_ascii_lowercase().ends_with(".pdf");
        if looks_pdf {
            unpacked_docs.push(UploadedDocInput {
                name: unique_name,
                text: None,
                base64: Some(STANDARD.encode(&out_buf)),
            });
        } else {
            unpacked_docs.push(UploadedDocInput {
                name: unique_name,
                text: Some(String::from_utf8_lossy(&out_buf).into_owned()),
                base64: None,
            });
        }
    }

    Ok(unpacked_docs)
}

fn is_zip_upload(doc: &UploadedDocInput) -> bool {
    if doc.name.trim().to_ascii_lowercase().ends_with(".zip") {
        return doc.base64.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false);
    }
    let Some(ref b64) = doc.base64 else {
        return false;
    };
    let trimmed = b64.trim_start();
    let lower_prefix: String = trimmed.chars().take(48).collect::<String>().to_ascii_lowercase();
    if lower_prefix.starts_with("data:application/zip")
        || lower_prefix.starts_with("data:application/x-zip")
    {
        return true;
    }
    let stripped = DATA_URL_PREFIX_RE.replace(trimmed, "");
    let head: String = stripped
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .take(8)
        .collect();
    head.starts_with("UEsDB") || head.starts_with("UEsFB")
}

/// Expands any `.zip` archive uploads inside `documents` into individual validated `UploadedDocInput`
/// entries while enforcing `max_docs` and all pure-Rust zip-bomb guardrails.
pub fn expand_uploaded_documents_with_zip(
    documents: &[UploadedDocInput],
    max_docs: usize,
) -> std::result::Result<Vec<UploadedDocInput>, GuardrailError> {
    if documents.is_empty() {
        return Err(GuardrailError::new(
            "MISSING_DOCUMENTS",
            "At least one residential property document is required.",
            400,
        ));
    }
    let mut expanded = Vec::with_capacity(documents.len());
    for doc in documents {
        if is_zip_upload(doc) {
            let b64 = doc.base64.as_deref().unwrap_or("");
            let bytes = decode_base64_payload_with_limit(
                b64,
                MAX_ZIP_ARCHIVE_BYTES,
                "Uploaded ZIP packet exceeds the 24 MB archive size limit.",
            )?;
            let unpacked = unpack_zip_archive_bytes(&doc.name, &bytes, max_docs)?;
            expanded.extend(unpacked);
        } else {
            expanded.push(doc.clone());
        }
        if expanded.len() > max_docs {
            return Err(GuardrailError::new(
                "TOO_MANY_DOCUMENTS",
                format!("Up to {max_docs} documents may be analyzed in a single request."),
                400,
            ));
        }
    }
    if expanded.is_empty() {
        return Err(GuardrailError::new(
            "MISSING_DOCUMENTS",
            "At least one residential property document is required.",
            400,
        ));
    }
    Ok(expanded)
}
// [/feat/hoa-packet-ai]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PdfColorKind {
    Gray,
    Rgb,
    Cmyk,
    Unknown,
}

fn dict_int(dict: &Dictionary, primary: &[u8], alt: &[u8]) -> Option<i64> {
    dict.get(primary)
        .or_else(|_| dict.get(alt))
        .ok()
        .and_then(|o| match o {
            Object::Integer(i) => Some(*i),
            Object::Real(f) => Some(*f as i64),
            _ => None,
        })
}

fn dict_bool(dict: &Dictionary, primary: &[u8], alt: &[u8]) -> bool {
    dict.get(primary)
        .or_else(|_| dict.get(alt))
        .ok()
        .and_then(|o| o.as_bool().ok())
        .unwrap_or(false)
}

fn is_decode_inverted(dict: &Dictionary, doc: &Document) -> bool {
    let Ok(obj) = dict
        .get_deref(b"Decode", doc)
        .or_else(|_| dict.get_deref(b"D", doc))
    else {
        return false;
    };
    let Ok(arr) = obj.as_array() else {
        return false;
    };
    if arr.len() < 2 {
        return false;
    }
    let d0 = arr[0]
        .as_f32()
        .or_else(|_| arr[0].as_i64().map(|v| v as f32));
    let d1 = arr[1]
        .as_f32()
        .or_else(|_| arr[1].as_i64().map(|v| v as f32));
    matches!((d0, d1), (Ok(a), Ok(b)) if a > b)
}

fn resolve_color_kind(dict: &Dictionary, doc: &Document) -> PdfColorKind {
    let Ok(cs_obj) = dict
        .get_deref(b"ColorSpace", doc)
        .or_else(|_| dict.get_deref(b"CS", doc))
    else {
        return if dict_bool(dict, b"ImageMask", b"IM") {
            PdfColorKind::Gray
        } else {
            PdfColorKind::Unknown
        };
    };

    match cs_obj {
        Object::Name(name) => match name.as_slice() {
            b"DeviceGray" | b"CalGray" | b"G" => PdfColorKind::Gray,
            b"DeviceRGB" | b"CalRGB" | b"RGB" => PdfColorKind::Rgb,
            b"DeviceCMYK" | b"CMYK" => PdfColorKind::Cmyk,
            _ => PdfColorKind::Unknown,
        },
        Object::Array(arr) => {
            let Some(first_name) = arr.first().and_then(|o| o.as_name().ok()) else {
                return PdfColorKind::Unknown;
            };
            match first_name {
                b"DeviceGray" | b"CalGray" | b"G" | b"Separation" => PdfColorKind::Gray,
                b"DeviceRGB" | b"CalRGB" | b"RGB" => PdfColorKind::Rgb,
                b"DeviceCMYK" | b"CMYK" => PdfColorKind::Cmyk,
                b"ICCBased" => {
                    if let Some(icc_stream) = arr
                        .get(1)
                        .and_then(|o| doc.dereference(o).ok())
                        .and_then(|(_, deref)| deref.as_stream().ok())
                    {
                        match dict_int(&icc_stream.dict, b"N", b"N") {
                            Some(1) => PdfColorKind::Gray,
                            Some(3) => PdfColorKind::Rgb,
                            Some(4) => PdfColorKind::Cmyk,
                            _ => PdfColorKind::Unknown,
                        }
                    } else {
                        PdfColorKind::Unknown
                    }
                }
                _ => PdfColorKind::Unknown,
            }
        }
        _ => PdfColorKind::Unknown,
    }
}

fn extract_filter_names(dict: &Dictionary, doc: &Document) -> Vec<String> {
    let Ok(filter_obj) = dict
        .get_deref(b"Filter", doc)
        .or_else(|_| dict.get_deref(b"F", doc))
    else {
        return Vec::new();
    };
    match filter_obj {
        Object::Name(n) => vec![String::from_utf8_lossy(n).into_owned()],
        Object::Array(arr) => arr
            .iter()
            .filter_map(|item| {
                doc.dereference(item)
                    .ok()
                    .and_then(|(_, o)| o.as_name().ok())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Decodes outer transport filters (e.g. `[/FlateDecode /DCTDecode]`) while keeping the inner JPEG/JP2 stream intact.
fn decode_passthrough_jpeg_or_jp2(stream: &Stream, filter_len: usize) -> Option<Vec<u8>> {
    if filter_len <= 1 {
        if stream.content.is_empty() || stream.content.len() > MAX_IMAGE_DECOMPRESSED_BYTES {
            return None;
        }
        return Some(stream.content.clone());
    }
    let mut wrapper = stream.clone();
    if let Ok(Object::Array(arr)) = wrapper.dict.get_mut(b"Filter") {
        if !arr.is_empty() {
            arr.pop();
        }
    }
    if let Ok(Object::Array(parms)) = wrapper.dict.get_mut(b"DecodeParms") {
        if parms.len() >= filter_len {
            parms.pop();
        }
    }
    let bytes = wrapper
        .decompressed_content_with_limit(MAX_IMAGE_DECOMPRESSED_BYTES)
        .ok()?;
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

/// Converts a single `lopdf::Stream` (`/Subtype /Image`) into raw JPEG/JP2 or pure-Rust Netpbm (`P4`/`P5`/`P6`).
fn decode_pdf_image_stream(
    doc: &Document,
    page_number: u32,
    name: String,
    stream: &Stream,
) -> Option<ExtractedPdfImage> {
    let dict = &stream.dict;
    let subtype = dict.get_deref(b"Subtype", doc).ok()?.as_name().ok()?;
    if subtype != b"Image" {
        return None;
    }

    let w_i64 = dict_int(dict, b"Width", b"W")?;
    let h_i64 = dict_int(dict, b"Height", b"H")?;
    if !(1..=20_000).contains(&w_i64) || !(1..=20_000).contains(&h_i64) {
        return None;
    }
    let w = w_i64 as u32;
    let h = h_i64 as u32;

    let is_mask = dict_bool(dict, b"ImageMask", b"IM");
    let bpc_i64 = dict_int(dict, b"BitsPerComponent", b"BPC").unwrap_or(if is_mask { 1 } else { 8 });
    let bpc = u8::try_from(bpc_i64).ok()?;

    let filters = extract_filter_names(dict, doc);
    if let Some(last_filter) = filters.last().map(|s| s.as_str()) {
        match last_filter {
            "DCTDecode" | "DCT" => {
                let jpeg_bytes = decode_passthrough_jpeg_or_jp2(stream, filters.len())?;
                return Some(ExtractedPdfImage {
                    page_number,
                    name,
                    width: w,
                    height: h,
                    bits_per_component: bpc,
                    format: PdfImageFormat::Jpeg,
                    data: jpeg_bytes,
                });
            }
            "JPXDecode" => {
                let jp2_bytes = decode_passthrough_jpeg_or_jp2(stream, filters.len())?;
                return Some(ExtractedPdfImage {
                    page_number,
                    name,
                    width: w,
                    height: h,
                    bits_per_component: bpc,
                    format: PdfImageFormat::Jpeg2000,
                    data: jp2_bytes,
                });
            }
            _ => {}
        }
    }

    let raw = stream
        .decompressed_content_with_limit(MAX_IMAGE_DECOMPRESSED_BYTES)
        .ok()?;
    let color_kind = resolve_color_kind(dict, doc);
    let decode_inverted = is_decode_inverted(dict, doc);

    let wu = w as usize;
    let hu = h as usize;
    let pixels = wu.checked_mul(hu)?;

    // 1) 1-bit monochrome (`/DeviceGray` or `/ImageMask` with `BitsPerComponent = 1`) -> Netpbm `P4` PBM
    let row_bytes_1bit = (wu + 7) / 8;
    let expected_1bit = row_bytes_1bit.checked_mul(hu)?;
    if bpc == 1
        && matches!(color_kind, PdfColorKind::Gray | PdfColorKind::Unknown)
        && raw.len() >= expected_1bit
    {
        let header = format!("P4\n{w} {h}\n");
        let mut out = Vec::with_capacity(header.len() + expected_1bit);
        out.extend_from_slice(header.as_bytes());
        let rem_bits = wu % 8;
        let tail_mask: u8 = if rem_bits == 0 {
            0xFF
        } else {
            0xFFu8 << (8 - rem_bits)
        };
        for row in 0..hu {
            let row_start = row * row_bytes_1bit;
            let row_slice = &raw[row_start..row_start + row_bytes_1bit];
            for (col_idx, &b) in row_slice.iter().enumerate() {
                // PDF default `/Decode [0 1]` maps 0 -> black and 1 -> white, whereas Netpbm `P4`
                // maps 1 -> black and 0 -> white. Invert unless `/Decode [1 0]` was specified.
                let mut pbm_byte = if decode_inverted { b } else { !b };
                if col_idx + 1 == row_bytes_1bit {
                    pbm_byte &= tail_mask;
                }
                out.push(pbm_byte);
            }
        }
        return Some(ExtractedPdfImage {
            page_number,
            name,
            width: w,
            height: h,
            bits_per_component: 1,
            format: PdfImageFormat::Pbm,
            data: out,
        });
    }

    // 2) 8-bit RGB -> Netpbm `P6` PPM
    let expected_rgb8 = pixels.checked_mul(3)?;
    if bpc == 8
        && (color_kind == PdfColorKind::Rgb
            || (color_kind == PdfColorKind::Unknown && raw.len() == expected_rgb8))
        && raw.len() >= expected_rgb8
    {
        let header = format!("P6\n{w} {h}\n255\n");
        let mut out = Vec::with_capacity(header.len() + expected_rgb8);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&raw[..expected_rgb8]);
        return Some(ExtractedPdfImage {
            page_number,
            name,
            width: w,
            height: h,
            bits_per_component: 8,
            format: PdfImageFormat::Ppm,
            data: out,
        });
    }

    // 3) 8-bit Grayscale -> Netpbm `P5` PGM
    if bpc == 8
        && (color_kind == PdfColorKind::Gray
            || (color_kind == PdfColorKind::Unknown && raw.len() == pixels))
        && raw.len() >= pixels
    {
        let header = format!("P5\n{w} {h}\n255\n");
        let mut out = Vec::with_capacity(header.len() + pixels);
        out.extend_from_slice(header.as_bytes());
        if decode_inverted {
            out.extend(raw[..pixels].iter().map(|&b| 255u8.wrapping_sub(b)));
        } else {
            out.extend_from_slice(&raw[..pixels]);
        }
        return Some(ExtractedPdfImage {
            page_number,
            name,
            width: w,
            height: h,
            bits_per_component: 8,
            format: PdfImageFormat::Pgm,
            data: out,
        });
    }

    // 4) 16-bit RGB / Grayscale -> downsample MSB to 8-bit `P6` / `P5`
    if bpc == 16 {
        let expected_rgb16 = expected_rgb8.checked_mul(2)?;
        let expected_gray16 = pixels.checked_mul(2)?;
        if color_kind == PdfColorKind::Rgb && raw.len() >= expected_rgb16 {
            let header = format!("P6\n{w} {h}\n255\n");
            let mut out = Vec::with_capacity(header.len() + expected_rgb8);
            out.extend_from_slice(header.as_bytes());
            out.extend(raw[..expected_rgb16].chunks_exact(2).map(|c| c[0]));
            return Some(ExtractedPdfImage {
                page_number,
                name,
                width: w,
                height: h,
                bits_per_component: 8,
                format: PdfImageFormat::Ppm,
                data: out,
            });
        }
        if color_kind == PdfColorKind::Gray && raw.len() >= expected_gray16 {
            let header = format!("P5\n{w} {h}\n255\n");
            let mut out = Vec::with_capacity(header.len() + pixels);
            out.extend_from_slice(header.as_bytes());
            out.extend(raw[..expected_gray16].chunks_exact(2).map(|c| c[0]));
            return Some(ExtractedPdfImage {
                page_number,
                name,
                width: w,
                height: h,
                bits_per_component: 8,
                format: PdfImageFormat::Pgm,
                data: out,
            });
        }
    }

    // 5) 8-bit CMYK -> convert to 24-bit RGB Netpbm `P6` PPM
    let expected_cmyk8 = pixels.checked_mul(4)?;
    if bpc == 8 && color_kind == PdfColorKind::Cmyk && raw.len() >= expected_cmyk8 {
        let header = format!("P6\n{w} {h}\n255\n");
        let mut out = Vec::with_capacity(header.len() + expected_rgb8);
        out.extend_from_slice(header.as_bytes());
        for px in raw[..expected_cmyk8].chunks_exact(4) {
            let (c, m, y, k) = (px[0] as u16, px[1] as u16, px[2] as u16, px[3] as u16);
            let inv_k = 255u16.saturating_sub(k);
            let r = ((255u16.saturating_sub(c) * inv_k) / 255) as u8;
            let g = ((255u16.saturating_sub(m) * inv_k) / 255) as u8;
            let b = ((255u16.saturating_sub(y) * inv_k) / 255) as u8;
            out.extend_from_slice(&[r, g, b]);
        }
        return Some(ExtractedPdfImage {
            page_number,
            name,
            width: w,
            height: h,
            bits_per_component: 8,
            format: PdfImageFormat::Ppm,
            data: out,
        });
    }

    None
}

fn collect_xobject_images_from_resources(
    doc: &Document,
    resources: &Dictionary,
    page_number: u32,
    depth: usize,
    visited_streams: &mut HashSet<ObjectId>,
    out: &mut Vec<ExtractedPdfImage>,
) {
    if depth > 6 {
        return;
    }
    let Ok(xobj_entry) = resources.get_deref(b"XObject", doc) else {
        return;
    };
    let Ok(xobj_dict) = xobj_entry.as_dict() else {
        return;
    };

    // Sort keys deterministically (`/Im0`, `/Im1`, ...) so multi-strip page scans stay in order.
    let mut entries: Vec<(&Vec<u8>, &Object)> = xobj_dict.iter().collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    for (raw_name, val) in entries {
        let (maybe_id, resolved) = match doc.dereference(val) {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        if let Some(oid) = maybe_id {
            if !visited_streams.insert(oid) {
                continue;
            }
        }
        let Ok(stream) = resolved.as_stream() else {
            continue;
        };
        let Some(subtype) = stream
            .dict
            .get_deref(b"Subtype", doc)
            .ok()
            .and_then(|o| o.as_name().ok())
        else {
            continue;
        };

        if subtype == b"Image" {
            let xname = String::from_utf8_lossy(raw_name).into_owned();
            if let Some(extracted) = decode_pdf_image_stream(doc, page_number, xname, stream) {
                out.push(extracted);
            }
        } else if subtype == b"Form" {
            if let Ok(form_res_obj) = stream.dict.get_deref(b"Resources", doc) {
                if let Ok(form_res_dict) = form_res_obj.as_dict() {
                    collect_xobject_images_from_resources(
                        doc,
                        form_res_dict,
                        page_number,
                        depth + 1,
                        visited_streams,
                        out,
                    );
                }
            }
        }
    }
}

/// Extracts embedded `/Subtype /Image` XObjects from an already-loaded `lopdf::Document` page in pure Rust.
pub fn extract_embedded_images_from_lopdf_page(
    doc: &Document,
    page_number: u32,
    page_id: ObjectId,
) -> Result<Vec<ExtractedPdfImage>> {
    let (direct_res, res_ids) = doc.get_page_resources(page_id)?;
    let mut visited = HashSet::new();
    let mut images = Vec::new();

    if let Some(res_dict) = direct_res {
        collect_xobject_images_from_resources(
            doc,
            res_dict,
            page_number,
            0,
            &mut visited,
            &mut images,
        );
    }
    for res_id in res_ids {
        if let Ok(res_dict) = doc.get_dictionary(res_id) {
            collect_xobject_images_from_resources(
                doc,
                res_dict,
                page_number,
                0,
                &mut visited,
                &mut images,
            );
        }
    }
    Ok(images)
}

/// Extracts embedded `/Subtype /Image` XObjects from 1-based `page_number` of raw PDF `bytes` in pure Rust:
/// - `/DCTDecode` -> raw JPEG bytes (`PdfImageFormat::Jpeg`)
/// - `/JPXDecode` -> raw JPEG 2000 bytes (`PdfImageFormat::Jpeg2000`)
/// - `/FlateDecode` / uncompressed 1-bit Gray -> Netpbm `P4` PBM (`PdfImageFormat::Pbm`)
/// - `/FlateDecode` / uncompressed 8-bit Gray -> Netpbm `P5` PGM (`PdfImageFormat::Pgm`)
/// - `/FlateDecode` / uncompressed 8-bit RGB/CMYK -> Netpbm `P6` PPM (`PdfImageFormat::Ppm`)
pub fn extract_embedded_images_from_pdf_page(
    bytes: &[u8],
    page_number: u32,
) -> Result<Vec<ExtractedPdfImage>> {
    if bytes.len() > MAX_SINGLE_PDF_BYTES {
        return Err(GuardrailError::new(
            "DOCUMENT_TOO_LARGE",
            "Uploaded PDF exceeds the 12 MB per-document size limit.",
            413,
        )
        .into());
    }
    let doc = Document::load_mem(bytes).map_err(|e| anyhow!("Invalid PDF structure: {e}"))?;
    let pages = doc.get_pages();
    let Some(&page_id) = pages.get(&page_number) else {
        return Err(anyhow!(
            "Page {page_number} does not exist in PDF ({} total pages)",
            pages.len()
        ));
    };
    extract_embedded_images_from_lopdf_page(&doc, page_number, page_id)
}

/// Runs a bounded subprocess with concurrent `stdin` streaming, capped `stdout`/`stderr` readers, and strict kill-on-timeout.
fn run_piped_command_with_timeout(
    bin: &str,
    args: &[&str],
    stdin_payload: Option<&[u8]>,
    timeout: Duration,
    max_stdout_bytes: u64,
) -> Result<Vec<u8>> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .env("OMP_THREAD_LIMIT", "1")
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn OCR helper '{bin}': {e}"))?;

    let writer_handle = if let Some(payload) = stdin_payload {
        let mut child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdin pipe for '{bin}'"))?;
        let owned = payload.to_vec();
        Some(std::thread::spawn(move || {
            let _ = child_stdin.write_all(&owned);
            let _ = child_stdin.flush();
        }))
    } else {
        None
    };

    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to open stdout pipe for '{bin}'"))?;
    let stdout_handle = std::thread::spawn(move || -> Vec<u8> {
        let mut buf = Vec::new();
        let _ = child_stdout.take(max_stdout_bytes).read_to_end(&mut buf);
        buf
    });

    let child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to open stderr pipe for '{bin}'"))?;
    let stderr_handle = std::thread::spawn(move || -> String {
        let mut buf = Vec::new();
        let _ = child_stderr.take(16 * 1024).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let deadline = Instant::now() + timeout;
    let mut poll_sleep = Duration::from_millis(4);
    let status = loop {
        match child.try_wait() {
            Ok(Some(exit_status)) => break exit_status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow!(
                        "OCR process '{bin}' timed out after {}ms",
                        timeout.as_millis()
                    ));
                }
                std::thread::sleep(poll_sleep);
                poll_sleep = (poll_sleep * 2).min(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("Failed waiting on OCR process '{bin}': {e}"));
            }
        }
    };

    if let Some(wh) = writer_handle {
        let _ = wh.join();
    }
    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_msg = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        return Err(anyhow!(
            "OCR process '{bin}' exited with {status}: {stderr_msg}"
        ));
    }
    Ok(stdout_bytes)
}

/// Pipes raw JPEG/JP2/PNG/Netpbm (`P4`/`P5`/`P6`) bytes to `tesseract stdin stdout -l eng --psm 6`
/// under the global concurrency semaphore and per-page timeout.
pub fn run_tesseract_ocr(image_bytes: &[u8], cfg: &OcrConfig) -> Result<String> {
    if image_bytes.is_empty() {
        return Ok(String::new());
    }
    let _permit = OCR_SEMAPHORE.acquire(cfg.timeout.max(Duration::from_secs(5)))?;
    let out_bytes = run_piped_command_with_timeout(
        &cfg.tesseract_bin,
        &["stdin", "stdout", "-l", "eng", "--psm", "6"],
        Some(image_bytes),
        cfg.timeout,
        MAX_OCR_STDOUT_BYTES,
    )?;
    Ok(normalize_page_text(&String::from_utf8_lossy(&out_bytes)))
}

struct TempPdfFileGuard {
    path: PathBuf,
}

impl TempPdfFileGuard {
    fn write_restricted(pdf_bytes: &[u8]) -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "openestate-ocr-{}-{}.pdf",
            std::process::id(),
            Uuid::new_v4()
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?;
            f.write_all(pdf_bytes)?;
            f.flush()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, pdf_bytes)?;
        }
        Ok(Self { path })
    }
}

impl Drop for TempPdfFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn render_page_png_via_pdftoppm(
    pdf_path: &Path,
    page_number: u32,
    cfg: &OcrConfig,
) -> Result<Vec<u8>> {
    let page_str = page_number.to_string();
    let path_str = pdf_path
        .to_str()
        .ok_or_else(|| anyhow!("Non-UTF8 temp path for pdftoppm"))?;
    let _permit = OCR_SEMAPHORE.acquire(cfg.timeout.max(Duration::from_secs(5)))?;
    run_piped_command_with_timeout(
        &cfg.pdftoppm_bin,
        &[
            "-png",
            "-r",
            "200",
            "-singlefile",
            "-f",
            &page_str,
            "-l",
            &page_str,
            path_str,
        ],
        None,
        cfg.timeout,
        MAX_IMAGE_DECOMPRESSED_BYTES as u64,
    )
}

fn select_page_images_for_ocr(mut images: Vec<ExtractedPdfImage>) -> Vec<ExtractedPdfImage> {
    if images.len() > 1 {
        let has_substantive = images
            .iter()
            .any(|img| (img.width as u64) * (img.height as u64) >= 64);
        if has_substantive {
            images.retain(|img| (img.width as u64) * (img.height as u64) >= 16);
        }
    }
    if images.len() > MAX_OCR_IMAGES_PER_PAGE {
        // Keep the top N images by pixel area while restoring original XObject stream order.
        let mut indexed: Vec<(usize, ExtractedPdfImage)> = images.into_iter().enumerate().collect();
        indexed.sort_by(|(_, a), (_, b)| {
            let area_a = (a.width as u64) * (a.height as u64);
            let area_b = (b.width as u64) * (b.height as u64);
            area_b.cmp(&area_a)
        });
        indexed.truncate(MAX_OCR_IMAGES_PER_PAGE);
        indexed.sort_by_key(|(idx, _)| *idx);
        images = indexed.into_iter().map(|(_, img)| img).collect();
    }
    images
}

fn merge_digital_and_ocr_text(digital: &str, ocr: &str) -> String {
    if ocr.is_empty() {
        return digital.to_string();
    }
    if digital.is_empty() || ocr.contains(digital) {
        return ocr.to_string();
    }
    normalize_page_text(&format!("{digital}\n{ocr}"))
}

struct DigitalPdfState {
    doc: Option<Document>,
    page_map: BTreeMap<u32, ObjectId>,
    page_texts: BTreeMap<u32, String>,
    all_pages_rich: bool,
    used_pdf_extract: bool,
}

/// Shared pure-Rust digital PDF loader & per-page text extractor (`lopdf` `Td`/`TD` newline
/// synthesis + `pdf_extract` CMap/ToUnicode fallback on sparse pages).
fn load_and_extract_digital_pdf(bytes: &[u8]) -> Result<DigitalPdfState> {
    if bytes.len() > MAX_SINGLE_PDF_BYTES {
        return Err(GuardrailError::new(
            "DOCUMENT_TOO_LARGE",
            "Uploaded PDF exceeds the 12 MB per-document size limit.",
            413,
        )
        .into());
    }

    if let Ok(mut doc) = Document::load_mem(bytes) {
        let page_map = doc.get_pages();
        if page_map.len() > MAX_PDF_PAGES {
            return Err(GuardrailError::new(
                "TOO_MANY_PDF_PAGES",
                format!("PDF exceeds maximum page limit ({MAX_PDF_PAGES} pages)."),
                400,
            )
            .into());
        }

        for &page_id in page_map.values() {
            if let Ok(raw_content) =
                doc.get_page_content_with_limit(page_id, MAX_PAGE_DECOMPRESSED_BYTES)
            {
                if let Ok(decoded) = Content::decode(&raw_content) {
                    let mut new_ops = Vec::with_capacity(decoded.operations.len() + 16);
                    let mut changed = false;
                    let mut emitted_text = false;
                    for op in decoded.operations {
                        match op.operator.as_str() {
                            "Tj" | "TJ" => {
                                emitted_text = true;
                                new_ops.push(op);
                            }
                            "T*" | "'" | "\"" | "BT" | "ET" => {
                                emitted_text = false;
                                new_ops.push(op);
                            }
                            "Td" | "TD" => {
                                let dy_nonzero = op
                                    .operands
                                    .get(1)
                                    .map(|o| match o {
                                        Object::Real(f) => f.abs() > 0.5,
                                        Object::Integer(i) => *i != 0,
                                        _ => false,
                                    })
                                    .unwrap_or(false);
                                if dy_nonzero && emitted_text {
                                    new_ops.push(Operation::new("T*", vec![]));
                                    emitted_text = false;
                                    changed = true;
                                }
                                new_ops.push(op);
                            }
                            _ => new_ops.push(op),
                        }
                    }
                    if changed {
                        if let Ok(reencoded) = (Content { operations: new_ops }).encode() {
                            let _ = doc.change_page_content(page_id, reencoded);
                        }
                    }
                }
            }
        }

        let mut page_texts: BTreeMap<u32, String> = BTreeMap::new();
        let mut all_pages_rich = !page_map.is_empty();
        for &page_num in page_map.keys() {
            let txt = doc
                .extract_text_with_limit(&[page_num], MAX_PAGE_DECOMPRESSED_BYTES)
                .map(|s| normalize_page_text(&s))
                .unwrap_or_default();
            if txt.chars().count() < MIN_SELECTABLE_PAGE_CHARS {
                all_pages_rich = false;
            }
            page_texts.insert(page_num, txt);
        }

        let mut used_pdf_extract = false;
        if !all_pages_rich {
            if let Ok(pe_vec) = pdf_extract::extract_text_from_mem_by_pages(bytes) {
                for (idx, raw_pe) in pe_vec.into_iter().enumerate() {
                    let pnum = (idx + 1) as u32;
                    if let Some(slot) = page_texts.get_mut(&pnum) {
                        if slot.chars().count() < MIN_SELECTABLE_PAGE_CHARS {
                            let norm_pe = normalize_page_text(&raw_pe);
                            if norm_pe.chars().count() > slot.chars().count() {
                                *slot = norm_pe;
                                used_pdf_extract = true;
                            }
                        }
                    }
                }
            }
        }

        return Ok(DigitalPdfState {
            doc: Some(doc),
            page_map,
            page_texts,
            all_pages_rich,
            used_pdf_extract,
        });
    }

    let mut page_texts: BTreeMap<u32, String> = BTreeMap::new();
    let mut used_pdf_extract = false;
    if let Ok(by_pages) = pdf_extract::extract_text_from_mem_by_pages(bytes) {
        if by_pages.len() > MAX_PDF_PAGES {
            return Err(GuardrailError::new(
                "TOO_MANY_PDF_PAGES",
                format!("PDF exceeds maximum page limit ({MAX_PDF_PAGES} pages)."),
                400,
            )
            .into());
        }
        used_pdf_extract = true;
        for (idx, raw_p) in by_pages.into_iter().enumerate() {
            page_texts.insert((idx + 1) as u32, normalize_page_text(&raw_p));
        }
    }

    Ok(DigitalPdfState {
        doc: None,
        page_map: BTreeMap::new(),
        page_texts,
        all_pages_rich: false,
        used_pdf_extract,
    })
}

fn extract_pdf_pages_with_preloaded_state(
    bytes: &[u8],
    state: DigitalPdfState,
    ocr_cfg: &OcrConfig,
) -> Result<(Vec<PageExtract>, &'static str)> {
    let mut ocr_attempted = false;
    let mut last_ocr_err: Option<String> = None;

    let DigitalPdfState {
        doc,
        page_map,
        mut page_texts,
        all_pages_rich,
        used_pdf_extract,
    } = state;

    if let Some(doc) = doc {
        if all_pages_rich {
            let pages = page_texts
                .into_iter()
                .filter(|(_, t)| !t.is_empty())
                .map(|(page_number, text)| PageExtract { page_number, text })
                .collect();
            return Ok((pages, "lopdf"));
        }

        let total_digital_chars: usize = page_texts.values().map(|t| t.chars().count()).sum();
        let sparse_pages: Vec<(u32, ObjectId)> = page_map
            .iter()
            .filter_map(|(&pnum, &pid)| {
                let len = page_texts
                    .get(&pnum)
                    .map(|s| s.chars().count())
                    .unwrap_or(0);
                if len < MIN_SELECTABLE_PAGE_CHARS {
                    Some((pnum, pid))
                } else {
                    None
                }
            })
            .collect();

        if sparse_pages.is_empty() && total_digital_chars >= 20 {
            let pages = page_texts
                .into_iter()
                .filter(|(_, t)| !t.is_empty())
                .map(|(page_number, text)| PageExtract { page_number, text })
                .collect();
            return Ok((pages, if used_pdf_extract { "pdf_extract" } else { "lopdf" }));
        }

        let mut used_lopdf_ocr = false;
        let mut used_pdftoppm_ocr = false;
        let mut ocr_pages_processed = 0usize;
        let mut sparse_without_images: Vec<u32> = Vec::new();

        for &(pnum, pid) in &sparse_pages {
            if ocr_pages_processed >= ocr_cfg.max_pages {
                break;
            }
            let page_images =
                extract_embedded_images_from_lopdf_page(&doc, pnum, pid).unwrap_or_default();
            if page_images.is_empty() {
                sparse_without_images.push(pnum);
                continue;
            }

            if !is_tesseract_available(ocr_cfg) {
                continue;
            }

            ocr_attempted = true;
            ocr_pages_processed += 1;
            let chosen_images = select_page_images_for_ocr(page_images);
            let mut ocr_chunks = Vec::with_capacity(chosen_images.len());
            for img in chosen_images {
                match run_tesseract_ocr(&img.data, ocr_cfg) {
                    Ok(txt) if !txt.is_empty() => ocr_chunks.push(txt),
                    Ok(_) => {}
                    Err(e) => last_ocr_err = Some(e.to_string()),
                }
            }
            if !ocr_chunks.is_empty() {
                let page_ocr = normalize_page_text(&ocr_chunks.join("\n"));
                if !page_ocr.is_empty() {
                    let existing = page_texts.get(&pnum).cloned().unwrap_or_default();
                    page_texts.insert(pnum, merge_digital_and_ocr_text(&existing, &page_ocr));
                    used_lopdf_ocr = true;
                }
            }
        }

        let current_total_chars: usize = page_texts.values().map(|t| t.chars().count()).sum();
        if current_total_chars < MIN_SELECTABLE_PAGE_CHARS
            && !sparse_without_images.is_empty()
            && is_pdftoppm_available(ocr_cfg)
            && is_tesseract_available(ocr_cfg)
        {
            if let Ok(temp_pdf) = TempPdfFileGuard::write_restricted(bytes) {
                for pnum in sparse_without_images {
                    if ocr_pages_processed >= ocr_cfg.max_pages {
                        break;
                    }
                    ocr_attempted = true;
                    ocr_pages_processed += 1;
                    match render_page_png_via_pdftoppm(&temp_pdf.path, pnum, ocr_cfg)
                        .and_then(|png| run_tesseract_ocr(&png, ocr_cfg))
                    {
                        Ok(ocr_txt) if !ocr_txt.is_empty() => {
                            let existing = page_texts.get(&pnum).cloned().unwrap_or_default();
                            page_texts
                                .insert(pnum, merge_digital_and_ocr_text(&existing, &ocr_txt));
                            used_pdftoppm_ocr = true;
                        }
                        Ok(_) => {}
                        Err(e) => last_ocr_err = Some(e.to_string()),
                    }
                }
            }
        }

        let final_pages: Vec<PageExtract> = page_texts
            .into_iter()
            .filter(|(_, t)| !t.is_empty())
            .map(|(page_number, text)| PageExtract { page_number, text })
            .collect();
        let final_chars: usize = final_pages.iter().map(|p| p.text.chars().count()).sum();

        if final_chars >= 20 && !final_pages.is_empty() {
            let method = if used_lopdf_ocr {
                "lopdf+tesseract_ocr"
            } else if used_pdftoppm_ocr {
                "pdftoppm+tesseract_ocr"
            } else if used_pdf_extract {
                "pdf_extract"
            } else {
                "lopdf"
            };
            return Ok((final_pages, method));
        }
    } else if used_pdf_extract {
        let pe_pages: Vec<PageExtract> = page_texts
            .into_iter()
            .filter(|(_, t)| !t.is_empty())
            .map(|(page_number, text)| PageExtract { page_number, text })
            .collect();
        let total_chars: usize = pe_pages.iter().map(|p| p.text.chars().count()).sum();
        if total_chars >= 20 && !pe_pages.is_empty() {
            return Ok((pe_pages, "pdf_extract"));
        }
    }

    if let Ok(whole) = pdf_extract::extract_text_from_mem(bytes) {
        let norm = normalize_page_text(&whole);
        if norm.chars().count() >= 20 {
            return Ok((
                vec![PageExtract {
                    page_number: 1,
                    text: norm,
                }],
                "pdf_extract",
            ));
        }
    }

    if let Some(err_detail) = last_ocr_err {
        return Err(anyhow!(
            "Could not extract readable text from scanned PDF (OCR error: {err_detail})."
        ));
    }
    if ocr_attempted {
        return Err(anyhow!(
            "Scanned PDF contained no legible OCR text (>= 20 characters required)."
        ));
    }

    Err(anyhow!(
        "Could not extract selectable text from PDF. If this file is a scanned image without OCR text, ensure tesseract-ocr is installed or paste summary pages."
    ))
}

/// Extracts per-page text from raw PDF bytes with an explicit [`OcrConfig`]:
/// 1) `lopdf::Document::load_mem` + `doc.extract_text_with_limit(&[page_num], ...)` (<2ms fast path when >= 25 chars/page)
/// 2) `pdf_extract::extract_text_from_mem_by_pages` / `extract_text_from_mem` (CMap / ToUnicode / CID / TrueType / Type1 font decoder)
/// 3) On-demand OCR (`lopdf+tesseract_ocr` or `pdftoppm+tesseract_ocr`) ONLY on sparse pages (< 25 chars) with embedded images
///    or completely non-selectable vector-flattened documents.
pub fn extract_pdf_pages_from_bytes_with_ocr(
    bytes: &[u8],
    ocr_cfg: &OcrConfig,
) -> Result<(Vec<PageExtract>, &'static str)> {
    let state = load_and_extract_digital_pdf(bytes)?;
    extract_pdf_pages_with_preloaded_state(bytes, state, ocr_cfg)
}

/// Extracts per-page text from raw PDF bytes using pure-Rust `lopdf` / `pdf_extract` and automatic
/// on-demand local `tesseract` OCR when pages contain scanned `/Subtype /Image` XObjects (`< 25` digital chars).
pub fn extract_pdf_pages_from_bytes(bytes: &[u8]) -> Result<(Vec<PageExtract>, &'static str)> {
    extract_pdf_pages_from_bytes_with_ocr(bytes, &OcrConfig::from_env())
}

fn split_plain_text_into_pages(raw: &str) -> Vec<PageExtract> {
    let mut pages = Vec::new();
    let mut last_end = 0usize;
    let mut current_page_num: Option<u32> = None;

    for caps in INLINE_PAGE_MARKER_RE.captures_iter(raw) {
        let m = caps.get(0).unwrap();
        if let Some(pnum) = current_page_num {
            let chunk = normalize_page_text(&raw[last_end..m.start()]);
            if !chunk.is_empty() {
                pages.push(PageExtract {
                    page_number: pnum,
                    text: chunk,
                });
            }
        } else {
            let leading = normalize_page_text(&raw[..m.start()]);
            if !leading.is_empty() {
                pages.push(PageExtract {
                    page_number: 1,
                    text: leading,
                });
            }
        }
        current_page_num = caps.get(1).and_then(|c| c.as_str().parse::<u32>().ok());
        last_end = m.end();
    }

    if let Some(pnum) = current_page_num {
        let tail = normalize_page_text(&raw[last_end..]);
        if !tail.is_empty() {
            pages.push(PageExtract {
                page_number: pnum,
                text: tail,
            });
        }
    } else {
        let whole = normalize_page_text(raw);
        if !whole.is_empty() {
            pages.push(PageExtract {
                page_number: 1,
                text: whole,
            });
        }
    }
    pages
}

fn format_pages_with_citations(pages: &[PageExtract]) -> String {
    pages
        .iter()
        .map(|p| format!("[Page {}]\n{}", p.page_number, p.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Extracts per-page text + `[Page N]` cited text from an [`UploadedDocInput`] (base64 data URL, raw base64 PDF, or plain text).
pub fn extract_document_pages(doc: &UploadedDocInput) -> Result<ExtractedDocument> {
    let safe_name: String = if doc.name.trim().is_empty() {
        "uploaded-document.pdf".to_string()
    } else {
        doc.name.trim().chars().take(140).collect()
    };

    if let Some(ref txt) = doc.text {
        if txt.trim().chars().count() >= 20 {
            let pages = split_plain_text_into_pages(txt);
            let formatted = format_pages_with_citations(&pages);
            let chars = formatted.chars().count();
            return Ok(ExtractedDocument {
                name: safe_name,
                pages,
                formatted_text: formatted,
                chars,
                method: "plain_text".to_string(),
            });
        }
    }

    if let Some(ref b64) = doc.base64 {
        let bytes = decode_base64_payload(b64)?;
        let looks_like_pdf = bytes.starts_with(b"%PDF")
            || bytes.windows(4).take(1024).any(|w| w == b"%PDF")
            || safe_name.to_lowercase().ends_with(".pdf")
            || b64.trim_start().to_lowercase().starts_with("data:application/pdf");

        if looks_like_pdf {
            let (pages, method) = extract_pdf_pages_from_bytes(&bytes)?;
            let formatted = format_pages_with_citations(&pages);
            let chars = formatted.chars().count();
            return Ok(ExtractedDocument {
                name: safe_name,
                pages,
                formatted_text: formatted,
                chars,
                method: method.to_string(),
            });
        }

        let utf8 = String::from_utf8_lossy(&bytes);
        let pages = split_plain_text_into_pages(&utf8);
        let formatted = format_pages_with_citations(&pages);
        let chars = formatted.chars().count();
        if chars >= 20 {
            return Ok(ExtractedDocument {
                name: safe_name,
                pages,
                formatted_text: formatted,
                chars,
                method: "base64_text".to_string(),
            });
        }
    }

    Err(GuardrailError::new(
        "UNREADABLE_DOCUMENT",
        format!(
            "Could not extract readable text from \"{safe_name}\". If this PDF is a scanned image without selectable text, run OCR or paste key summary pages."
        ),
        400,
    )
    .into())
}

fn finalize_upload_batch(
    mut extracted_docs: Vec<ExtractedDocument>,
    kind: DocumentKind,
    max_total_chars: usize,
) -> Result<NormalizedUploadBatch> {
    let per_doc_cap = (max_total_chars / extracted_docs.len().max(1)).max(4_000);
    let mut blocks = Vec::with_capacity(extracted_docs.len());
    let mut files = Vec::with_capacity(extracted_docs.len());

    for d in &extracted_docs {
        files.push(json!({
            "name": d.name,
            "chars": d.chars,
            "pages": d.pages.len(),
            "method": d.method,
        }));
        let clipped = if d.formatted_text.chars().count() > per_doc_cap {
            let head_len = (per_doc_cap * 72) / 100;
            let tail_len = (per_doc_cap * 25) / 100;
            let chars_vec: Vec<char> = d.formatted_text.chars().collect();
            let head: String = chars_vec[..head_len.min(chars_vec.len())].iter().collect();
            let tail: String =
                chars_vec[chars_vec.len().saturating_sub(tail_len)..].iter().collect();
            format!("{head}\n\n[... middle pages omitted for length ...]\n\n{tail}")
        } else {
            d.formatted_text.clone()
        };
        blocks.push(format!(
            "=== DOCUMENT: {} ({} chars, {} pages) ===\n{}",
            d.name,
            d.chars,
            d.pages.len(),
            clipped
        ));
    }

    let combined = blocks.join("\n\n");
    let scrubbed = validate_and_scrub_property_document(&combined, kind)?;

    let (sanitized_text, pii_redacted_count) = match kind {
        DocumentKind::Title | DocumentKind::Inspection => {
            let pii = crate::ai::title_review::redact_title_pii(&scrubbed.sanitized_text);
            (pii.masked_text, pii.pii_redacted_count)
        }
        DocumentKind::Cre => {
            let pii = crate::cre::redact_cre_document_pii(&scrubbed.sanitized_text);
            (pii.masked_text, pii.pii_redacted_count)
        }
        DocumentKind::Hoa | DocumentKind::Packet => {
            let mut page_pii_total = 0usize;
            for doc in &mut extracted_docs {
                for page in &mut doc.pages {
                    let (safe_page, _) = scrub_untrusted_document_lines(&page.text);
                    page.text = safe_page;
                }
                let formatted_safe = format_pages_with_citations(&doc.pages);
                let doc_pii = crate::ai::title_review::redact_title_pii(&formatted_safe);
                page_pii_total += doc_pii.pii_redacted_count;
                let masked_pages = split_plain_text_into_pages(&doc_pii.masked_text);
                if !masked_pages.is_empty() {
                    doc.pages = masked_pages;
                }
                doc.formatted_text = format_pages_with_citations(&doc.pages);
                doc.chars = doc.formatted_text.chars().count();
            }
            let combined_pii = crate::ai::title_review::redact_title_pii(&scrubbed.sanitized_text);
            (
                combined_pii.masked_text,
                combined_pii.pii_redacted_count.max(page_pii_total),
            )
        }
    };

    Ok(NormalizedUploadBatch {
        files,
        documents: extracted_docs,
        sanitized_text,
        scrubbed_lines: scrubbed.scrubbed_lines,
        pii_redacted_count,
    })
}

/// Validates document count, extracts per-page cited text (including `.zip` packet unpacking and on-demand local OCR),
/// clips to character budget, and enforces residential property relevance + prompt-injection scrubbing (`guardrails`).
pub fn normalize_and_validate_uploads(
    documents: &[UploadedDocInput],
    kind: DocumentKind,
    max_docs: usize,
    max_total_chars: usize,
) -> Result<NormalizedUploadBatch> {
    let expanded = expand_uploaded_documents_with_zip(documents, max_docs)?;

    let mut extracted_docs = Vec::with_capacity(expanded.len());
    for doc in &expanded {
        extracted_docs.push(extract_document_pages(doc)?);
    }

    finalize_upload_batch(extracted_docs, kind, max_total_chars)
}

// =================================================================================================
// Tier 4: Multimodal LLM Vision OCR (`POST {OPENAI_BASE_URL}/responses`)
// =================================================================================================

#[derive(Debug)]
struct VisionHttpError {
    status: Option<u16>,
    message: String,
    unsupported_effort: bool,
    rejected_input_file: bool,
}

fn resolve_vision_ocr_models(env: &EnvConfig) -> Vec<ModelSpec> {
    resolve_llm_spec_for_call(env, None, Some("pdf_vision_ocr"), Some("extraction_fast")).models
}

fn extract_responses_output_text(j: &Value) -> std::result::Result<String, String> {
    extract_response_text(j).map_err(|e| e.message)
}

async fn send_vision_ocr_once(
    http: &HttpClient,
    env: &EnvConfig,
    endpoint: &str,
    api_key: &str,
    model: &str,
    effort: Option<&str>,
    user_content: &Value,
    sent_input_file: bool,
    timeout_ms: u64,
) -> std::result::Result<String, VisionHttpError> {
    let instructions = format!(
        "{IMMUTABLE_SYSTEM_PREAMBLE}\n\nYou are an accurate document OCR transcriber for residential real-estate inspection, seller disclosure, and preliminary title documents. Transcribe every page faithfully in plain text prefixed with [Page N] markers without commentary."
    );
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": [
            {
                "role": "user",
                "content": user_content,
            }
        ],
        "stream": false,
        "store": false,
        "max_output_tokens": 16_384,
    });
    if let Some(eff) = effort {
        body["reasoning"] = json!({ "effort": eff });
    }

    let resp = http
        .raw()
        .post(endpoint)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(Duration::from_millis(timeout_ms))
        .json(&body)
        .send()
        .await
        .map_err(|e| VisionHttpError {
            status: None,
            message: scrub_sensitive_text(&format!("Vision OCR network error: {e}")),
            unsupported_effort: false,
            rejected_input_file: false,
        })?;

    let status = resp.status();
    let raw_text = resp.text().await.map_err(|e| VisionHttpError {
        status: Some(status.as_u16()),
        message: scrub_sensitive_text(&format!("Vision OCR read error: {e}")),
        unsupported_effort: false,
        rejected_input_file: false,
    })?;

    if !status.is_success() {
        let parsed_msg = serde_json::from_str::<Value>(&raw_text)
            .ok()
            .and_then(|j| {
                j.get("error")
                    .and_then(|er| {
                        er.get("message")
                            .or_else(|| er.get("type"))
                            .and_then(|m| m.as_str())
                    })
                    .or_else(|| j.get("detail").and_then(|d| d.as_str()))
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| raw_text.chars().take(240).collect());

        let low = parsed_msg.to_lowercase();
        let code_u16 = status.as_u16();
        let unsupported_effort = (code_u16 == 400 || code_u16 == 422)
            && effort.is_some()
            && (low.contains("reasoning") || low.contains("effort"))
            && !low.contains("input_file");
        let rejected_input_file = sent_input_file
            && matches!(code_u16, 400 | 404 | 413 | 415 | 422)
            && !unsupported_effort;

        return Err(VisionHttpError {
            status: Some(code_u16),
            message: scrub_sensitive_text(&format!("HTTP {code_u16}: {parsed_msg}")),
            unsupported_effort,
            rejected_input_file,
        });
    }

    let j: Value = serde_json::from_str(&raw_text).map_err(|_| VisionHttpError {
        status: Some(status.as_u16()),
        message: format!("non-JSON Vision OCR response ({} bytes)", raw_text.len()),
        unsupported_effort: false,
        rejected_input_file: false,
    })?;

    if let Some(err_obj) = j.get("error").filter(|v| !v.is_null()) {
        let msg = err_obj
            .get("message")
            .or_else(|| err_obj.get("type"))
            .and_then(|m| m.as_str())
            .unwrap_or("unknown Vision OCR API error");
        let low = msg.to_lowercase();
        return Err(VisionHttpError {
            status: Some(status.as_u16()),
            message: scrub_sensitive_text(msg),
            unsupported_effort: false,
            rejected_input_file: sent_input_file
                && (low.contains("input_file")
                    || low.contains("file_data")
                    || low.contains("unsupported")),
        });
    }

    let text = extract_responses_output_text(&j).map_err(|msg| VisionHttpError {
        status: Some(status.as_u16()),
        message: scrub_sensitive_text(&msg),
        unsupported_effort: false,
        rejected_input_file: false,
    })?;
    let response_model = j
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(model)
        .to_string();
    let echoed_effort = j
        .pointer("/reasoning/effort")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| effort.map(str::to_string));
    if let Some(usage) = extract_usage(j.get("usage")) {
        crate::metering::add_llm_usage_detailed(
            Some(env),
            &usage,
            &response_model,
            Some(model),
            echoed_effort.as_deref(),
            Some("extraction_fast"),
            Some("pdf_vision_ocr"),
        );
    }
    Ok(text)
}

async fn send_vision_ocr_with_effort_fallback(
    http: &HttpClient,
    env: &EnvConfig,
    endpoint: &str,
    api_key: &str,
    spec: &ModelSpec,
    user_content: &Value,
    sent_input_file: bool,
    timeout_ms: u64,
) -> std::result::Result<String, VisionHttpError> {
    match send_vision_ocr_once(
        http,
        env,
        endpoint,
        api_key,
        &spec.model,
        spec.effort.as_deref(),
        user_content,
        sent_input_file,
        timeout_ms,
    )
    .await
    {
        Ok(txt) => Ok(txt),
        Err(err) if err.unsupported_effort && spec.effort.is_some() => {
            send_vision_ocr_once(
                http,
                env,
                endpoint,
                api_key,
                &spec.model,
                None,
                user_content,
                sent_input_file,
                timeout_ms,
            )
            .await
        }
        Err(err) => Err(err),
    }
}

fn extract_jpeg_images_for_vision_fallback(pdf_bytes: &[u8]) -> Vec<ExtractedPdfImage> {
    let Ok(doc) = Document::load_mem(pdf_bytes) else {
        return Vec::new();
    };
    let page_map = doc.get_pages();
    let mut out = Vec::new();
    for (&page_num, &page_id) in page_map.iter().take(MAX_OCR_PAGES_PER_DOC) {
        if let Ok(page_imgs) = extract_embedded_images_from_lopdf_page(&doc, page_num, page_id) {
            for img in select_page_images_for_ocr(page_imgs) {
                if img.format == PdfImageFormat::Jpeg {
                    out.push(img);
                }
            }
        }
    }
    out
}

/// Performs multimodal Vision OCR over a scanned PDF via `POST {OPENAI_BASE_URL}/responses`:
/// - Model chain: `LLM_MATCH_MODEL` (default `gpt-4o-mini`) -> `LLM_FALLBACK_MODEL` (default `gpt-4o-mini`)
/// - Primary payload: `input_file` (`data:application/pdf;base64,...`) + `input_text`
/// - Automatic per-model fallback: if the endpoint rejects `input_file`, extracts embedded `/DCTDecode`
///   JPEG page images via `extract_embedded_images_from_lopdf_page` and retries with `input_image`
///   (`image_url: "data:image/jpeg;base64,..."`).
pub async fn ocr_pdf_via_llm_responses(
    http: &HttpClient,
    env: &EnvConfig,
    filename: &str,
    pdf_bytes: &[u8],
) -> Result<Vec<PageExtract>> {
    if pdf_bytes.len() > MAX_SINGLE_PDF_BYTES {
        return Err(GuardrailError::new(
            "DOCUMENT_TOO_LARGE",
            "Uploaded PDF exceeds the 12 MB per-document size limit.",
            413,
        )
        .into());
    }
    let Some(base_url) = env.llm_base_url() else {
        return Err(anyhow!("LLM Vision OCR requires OPENAI_BASE_URL or OPENAI_API_KEY"));
    };
    let Some(api_key) = env.llm_api_key() else {
        return Err(anyhow!("LLM Vision OCR requires OPENAI_API_KEY"));
    };

    let endpoint = format!("{}/responses", base_url.trim_end_matches('/'));
    let safe_pdf_name = if filename.trim().is_empty() {
        "document.pdf".to_string()
    } else if filename.trim().to_lowercase().ends_with(".pdf") {
        filename.trim().chars().take(120).collect()
    } else {
        format!(
            "{}.pdf",
            filename.trim().chars().take(116).collect::<String>()
        )
    };

    let input_file_content = json!([
        {
            "type": "input_file",
            "filename": safe_pdf_name,
            "file_data": format!("data:application/pdf;base64,{}", STANDARD.encode(pdf_bytes)),
        },
        {
            "type": "input_text",
            "text": LLM_VISION_OCR_PROMPT,
        }
    ]);

    let jpeg_images = extract_jpeg_images_for_vision_fallback(pdf_bytes);
    let input_image_content: Option<Value> = if jpeg_images.is_empty() {
        None
    } else {
        let mut content_items: Vec<Value> = Vec::with_capacity(jpeg_images.len() + 1);
        for img in &jpeg_images {
            content_items.push(json!({
                "type": "input_image",
                "image_url": format!("data:image/jpeg;base64,{}", STANDARD.encode(&img.data)),
            }));
        }
        content_items.push(json!({
            "type": "input_text",
            "text": LLM_VISION_OCR_PROMPT,
        }));
        Some(Value::Array(content_items))
    };

    let models = resolve_vision_ocr_models(env);
    let mut errors: Vec<String> = Vec::new();

    for spec in &models {
        match send_vision_ocr_with_effort_fallback(
            http,
            env,
            &endpoint,
            api_key,
            spec,
            &input_file_content,
            true,
            DEFAULT_LLM_VISION_OCR_TIMEOUT_MS,
        )
        .await
        {
            Ok(transcribed) => {
                let pages = split_plain_text_into_pages(&transcribed);
                let total_chars: usize = pages.iter().map(|p| p.text.chars().count()).sum();
                if total_chars >= 20 && !pages.is_empty() {
                    return Ok(pages);
                }
                errors.push(format!(
                    "{}: Vision OCR output too short ({total_chars} chars)",
                    spec.model
                ));
            }
            Err(file_err) => {
                errors.push(format!("{} (input_file): {}", spec.model, file_err.message));
                if matches!(file_err.status, Some(401 | 403)) {
                    break;
                }
                if file_err.rejected_input_file {
                    if let Some(ref img_content) = input_image_content {
                        match send_vision_ocr_with_effort_fallback(
                            http,
                            env,
                            &endpoint,
                            api_key,
                            spec,
                            img_content,
                            false,
                            DEFAULT_LLM_VISION_OCR_TIMEOUT_MS,
                        )
                        .await
                        {
                            Ok(transcribed) => {
                                let pages = split_plain_text_into_pages(&transcribed);
                                let total_chars: usize =
                                    pages.iter().map(|p| p.text.chars().count()).sum();
                                if total_chars >= 20 && !pages.is_empty() {
                                    return Ok(pages);
                                }
                                errors.push(format!(
                                    "{} (input_image): Vision OCR output too short ({total_chars} chars)",
                                    spec.model
                                ));
                            }
                            Err(img_err) => {
                                errors.push(format!(
                                    "{} (input_image): {}",
                                    spec.model, img_err.message
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    Err(anyhow!(
        "LLM Vision OCR failed across model chain: {}",
        scrub_sensitive_text(&errors.join("; "))
    ))
}

/// Pure-Rust digital-only PDF check (<2ms, zero subprocesses). Returns `(pages, method, total_chars, digital_complete, preloaded_state)`
/// so `extract_document_pages_async` knows immediately whether a PDF is native digital (>= 25 chars) vs scanned,
/// and reuses `preloaded_state` without re-parsing `lopdf::Document::load_mem` on scanned PDFs.
fn inspect_digital_pdf_pages_only(
    bytes: &[u8],
) -> Result<(Vec<PageExtract>, &'static str, usize, bool, DigitalPdfState)> {
    let state = load_and_extract_digital_pdf(bytes)?;

    let has_sparse_image_page = if let Some(ref doc) = state.doc {
        state.page_map.iter().any(|(&pnum, &pid)| {
            let plen = state
                .page_texts
                .get(&pnum)
                .map(|s| s.chars().count())
                .unwrap_or(0);
            plen < MIN_SELECTABLE_PAGE_CHARS
                && !extract_embedded_images_from_lopdf_page(doc, pnum, pid)
                    .unwrap_or_default()
                    .is_empty()
        })
    } else {
        false
    };

    let pages: Vec<PageExtract> = state
        .page_texts
        .iter()
        .filter(|(_, t)| !t.is_empty())
        .map(|(&page_number, text)| PageExtract {
            page_number,
            text: text.clone(),
        })
        .collect();
    let total_chars: usize = pages.iter().map(|p| p.text.chars().count()).sum();
    let method = if state.used_pdf_extract {
        "pdf_extract"
    } else {
        "lopdf"
    };
    let digital_complete = if state.doc.is_some() {
        !has_sparse_image_page && total_chars >= MIN_SELECTABLE_PAGE_CHARS
    } else {
        total_chars >= MIN_SELECTABLE_PAGE_CHARS
    };

    Ok((pages, method, total_chars, digital_complete, state))
}

/// Async document extraction with automatic 4-tier routing:
/// 1) Digital PDF (`>= 25` selectable chars & no sparse scanned pages) -> `<2ms` pure-Rust `lopdf` / `pdf_extract`
/// 2) `OCR_MODE=llm` on scanned PDF (`< 25` digital chars) -> `ocr_pdf_via_llm_responses` (`method: "llm_vision_ocr"`)
/// 3) Local `lopdf+tesseract_ocr` / `pdftoppm+tesseract_ocr` via `spawn_blocking`
/// 4) Automatic fallback to `ocr_pdf_via_llm_responses` (`method: "llm_vision_ocr"`) when local `tesseract`
///    is unavailable on host or yields `< 25` chars (e.g. handwritten disclosures or photo captions).
pub async fn extract_document_pages_async(
    http: &HttpClient,
    env: &EnvConfig,
    doc: &UploadedDocInput,
) -> Result<ExtractedDocument> {
    let safe_name: String = if doc.name.trim().is_empty() {
        "uploaded-document.pdf".to_string()
    } else {
        doc.name.trim().chars().take(140).collect()
    };

    if let Some(ref txt) = doc.text {
        if txt.trim().chars().count() >= 20 {
            let pages = split_plain_text_into_pages(txt);
            let formatted = format_pages_with_citations(&pages);
            let chars = formatted.chars().count();
            return Ok(ExtractedDocument {
                name: safe_name,
                pages,
                formatted_text: formatted,
                chars,
                method: "plain_text".to_string(),
            });
        }
    }

    if let Some(ref b64) = doc.base64 {
        let bytes = decode_base64_payload(b64)?;
        let looks_like_pdf = bytes.starts_with(b"%PDF")
            || bytes.windows(4).take(1024).any(|w| w == b"%PDF")
            || safe_name.to_lowercase().ends_with(".pdf")
            || b64
                .trim_start()
                .to_lowercase()
                .starts_with("data:application/pdf");

        if looks_like_pdf {
            let (digital_pages, digital_method, digital_chars, digital_complete, preloaded_state) =
                inspect_digital_pdf_pages_only(&bytes)?;

            if digital_complete {
                let formatted = format_pages_with_citations(&digital_pages);
                let chars = formatted.chars().count();
                return Ok(ExtractedDocument {
                    name: safe_name,
                    pages: digital_pages,
                    formatted_text: formatted,
                    chars,
                    method: digital_method.to_string(),
                });
            }

            let ocr_mode = env.get("OCR_MODE").unwrap_or("");
            let force_llm_ocr = ocr_mode.trim().eq_ignore_ascii_case("llm");

            // Explicit `OCR_MODE=llm` on scanned/image-only PDF (`< 25` digital chars)
            if force_llm_ocr && digital_chars < MIN_SELECTABLE_PAGE_CHARS && env.llm_configured() {
                if let Ok(vision_pages) =
                    ocr_pdf_via_llm_responses(http, env, &safe_name, &bytes).await
                {
                    let formatted = format_pages_with_citations(&vision_pages);
                    let chars = formatted.chars().count();
                    return Ok(ExtractedDocument {
                        name: safe_name,
                        pages: vision_pages,
                        formatted_text: formatted,
                        chars,
                        method: "llm_vision_ocr".to_string(),
                    });
                }
            }

            // Default local-first OCR (`lopdf+tesseract_ocr` / `pdftoppm+tesseract_ocr`) off the async executor,
            // reusing `preloaded_state` so `lopdf::Document::load_mem` is never executed twice.
            let owned_bytes = bytes.clone();
            let ocr_cfg = OcrConfig::from_env_config(env);
            let local_res = tokio::task::spawn_blocking(move || {
                extract_pdf_pages_with_preloaded_state(&owned_bytes, preloaded_state, &ocr_cfg)
            })
            .await
            .map_err(|e| anyhow!("OCR blocking worker join error: {e}"))?;

            if let Ok((local_pages, local_method)) = local_res {
                let local_raw_chars: usize =
                    local_pages.iter().map(|p| p.text.chars().count()).sum();
                if local_raw_chars >= MIN_SELECTABLE_PAGE_CHARS
                    || (local_raw_chars >= 20 && !env.llm_configured())
                {
                    let formatted = format_pages_with_citations(&local_pages);
                    let chars = formatted.chars().count();
                    return Ok(ExtractedDocument {
                        name: safe_name,
                        pages: local_pages,
                        formatted_text: formatted,
                        chars,
                        method: local_method.to_string(),
                    });
                }
            }

            // Automatic Vision OCR fallback when `tesseract` is missing on host or yielded < 25 chars
            if digital_chars < MIN_SELECTABLE_PAGE_CHARS && env.llm_configured() {
                if let Ok(vision_pages) =
                    ocr_pdf_via_llm_responses(http, env, &safe_name, &bytes).await
                {
                    let formatted = format_pages_with_citations(&vision_pages);
                    let chars = formatted.chars().count();
                    return Ok(ExtractedDocument {
                        name: safe_name,
                        pages: vision_pages,
                        formatted_text: formatted,
                        chars,
                        method: "llm_vision_ocr".to_string(),
                    });
                }
            }

            if digital_chars >= 20 && !digital_pages.is_empty() {
                let formatted = format_pages_with_citations(&digital_pages);
                let chars = formatted.chars().count();
                return Ok(ExtractedDocument {
                    name: safe_name,
                    pages: digital_pages,
                    formatted_text: formatted,
                    chars,
                    method: digital_method.to_string(),
                });
            }
        } else {
            let utf8 = String::from_utf8_lossy(&bytes);
            let pages = split_plain_text_into_pages(&utf8);
            let formatted = format_pages_with_citations(&pages);
            let chars = formatted.chars().count();
            if chars >= 20 {
                return Ok(ExtractedDocument {
                    name: safe_name,
                    pages,
                    formatted_text: formatted,
                    chars,
                    method: "base64_text".to_string(),
                });
            }
        }
    }

    Err(GuardrailError::new(
        "UNREADABLE_DOCUMENT",
        format!(
            "Could not extract readable text from \"{safe_name}\". If this PDF is a scanned image without selectable text, run OCR or paste key summary pages."
        ),
        400,
    )
    .into())
}

/// Async counterpart to [`normalize_and_validate_uploads`] supporting both local `lopdf+tesseract_ocr`
/// AND multimodal LLM Vision OCR (`ocr_pdf_via_llm_responses`, `method: "llm_vision_ocr"`), followed by
/// residential domain guardrails, prompt-injection scrubbing, and automatic `redact_title_pii` on Title docs.
pub async fn extract_and_ocr_uploads_async(
    http: &HttpClient,
    env: &EnvConfig,
    documents: &[UploadedDocInput],
    kind: DocumentKind,
    max_docs: usize,
    max_total_chars: usize,
) -> Result<NormalizedUploadBatch> {
    let expanded = expand_uploaded_documents_with_zip(documents, max_docs)?;

    let mut extracted_docs = Vec::with_capacity(expanded.len());
    for doc in &expanded {
        extracted_docs.push(extract_document_pages_async(http, env, doc).await?);
    }

    finalize_upload_batch(extracted_docs, kind, max_total_chars)
}
