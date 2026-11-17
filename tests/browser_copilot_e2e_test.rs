//! Real Headless Chromium E2E Regression Suite (`cargo test --test openestate_browser_e2e_test`)
//!
//! Self-contained, zero-skip browser E2E test driven directly via Node 24 built-in `WebSocket`
//! + Chrome DevTools Protocol (`--remote-debugging-port=0`) against an in-process Axum server.
//! Uses CDP `Fetch.enable` to fail all `https://*` requests immediately (hermetic offline mode,
//! 0ms fallback) while allowing all `http://127.0.0.1:*` requests (including self-hosted
//! `/vendor/pdf.min.mjs`, `/vendor/pdf.worker.min.mjs`, `/vendor/pdf_viewer.min.css`, and
//! `/vendor/fflate.min.js`) to our seeded server.

#![recursion_limit = "512"]

mod common;

use flate2::{write::DeflateEncoder, Compression};
use lopdf::{
    content::{Content, Operation},
    dictionary, Document, Object, Stream, StringFormat,
};
use open_estate::ai::crc32_ieee;
use open_estate::ai::pdf::{extract_embedded_images_from_pdf_page, PdfImageFormat};
use open_estate::db::{DbStore, TABLE_NAMES};
use open_estate::guardrails::HttpDefenseShield;
use open_estate::search::{ingest_listings, ingest_report_into_db, IngestListingsRequest};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::f64::consts::PI;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::net::TcpListener;
use uuid::Uuid;

struct E2eServerHarness {
    base_url: String,
    db: DbStore,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl E2eServerHarness {
    async fn start(primary_rep: Value, secondary_rep: Value) -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-browser-e2e-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        let http_cache_dir = temp_dir.join("http_cache");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");
        std::fs::create_dir_all(&http_cache_dir).expect("create temp http cache dir");

        let db_path = temp_dir.join("openestate_e2e.sqlite");
        let env = EnvConfig::from_map(HashMap::from([
            (
                "SQLITE_DB_PATH".to_string(),
                db_path.to_string_lossy().into_owned(),
            ),
            (
                "SQLITE_PATH".to_string(),
                db_path.to_string_lossy().into_owned(),
            ),
            (
                "REPORTS_DIR".to_string(),
                reports_dir.to_string_lossy().into_owned(),
            ),
            ("SYNC_INTERVAL_MIN".to_string(), "0".to_string()),
        ]));

        let http = HttpClient::from_env(&env, Some(http_cache_dir));
        let db = DbStore::connect(&env).await.expect("connect e2e sqlite");
        db.ensure_schema().await.expect("ensure schema");
        assert_eq!(
            TABLE_NAMES.len(),
            44,
            "strict 44-table schema invariant must hold"
        );

        let primary_html = render_report_html(&primary_rep);
        db.put_cached_report(common::SYNTH_PRIMARY_SLUG, &primary_rep, Some(&primary_html))
            .await
            .expect("seed primary cached report");
        db.record_report_refresh(common::SYNTH_PRIMARY_SLUG)
            .await
            .expect("record primary refresh");

        let secondary_html = render_report_html(&secondary_rep);
        db.put_cached_report(
            common::SYNTH_SECONDARY_SLUG,
            &secondary_rep,
            Some(&secondary_html),
        )
        .await
        .expect("seed secondary cached report");

        ingest_report_into_db(&db, None, &env, &primary_rep, false)
            .await
            .expect("ingest primary report into db");
        ingest_report_into_db(&db, None, &env, &secondary_rep, false)
            .await
            .expect("ingest secondary report into db");

        let subj_lat = primary_rep["geo"]["lat"].as_f64().unwrap();
        let subj_lon = primary_rep["geo"]["lon"].as_f64().unwrap();
        ingest_listings(
            &db,
            &http,
            &env,
            IngestListingsRequest {
                zip: Some("95118".to_string()),
                listings: vec![json!({
                    "id": "mls-synth-active-e2e-2",
                    "address": "515 Synthbrook Cir, San Jose, CA 95118",
                    "city": "San Jose",
                    "state": "CA",
                    "zip": "95118",
                    "status": "Active",
                    "price": 1_598_000,
                    "listPrice": 1_598_000,
                    "beds": 4.0,
                    "baths": 2.5,
                    "sqft": 1940,
                    "yearBuilt": 1985,
                    "propertyType": "Single Family",
                    "lat": subj_lat + 0.016,
                    "lon": subj_lon,
                    "source": "licensed_mls_test"
                })],
                trigger_alerts: false,
                ..Default::default()
            },
        )
        .await
        .expect("seed active spatial listing");

        let state = AppState::with_components(
            env,
            http,
            db.clone(),
            HttpDefenseShield::default(),
            reports_dir,
            Some(temp_dir.join("datasets")),
        );
        let router = build_router(state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{addr}");

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
        });

        Self {
            base_url,
            db,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for E2eServerHarness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn build_flate_pdf_bytes(pages: &[&[&str]]) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let mut kids: Vec<Object> = Vec::with_capacity(pages.len());
    for lines in pages {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 11.into()]),
            Operation::new("TL", vec![16.into()]),
            Operation::new("Td", vec![48.into(), 740.into()]),
        ];
        for (idx, line) in lines.iter().enumerate() {
            if idx > 0 {
                ops.push(Operation::new("T*", vec![]));
            }
            ops.push(Operation::new(
                "Tj",
                vec![Object::String(
                    (*line).as_bytes().to_vec(),
                    StringFormat::Literal,
                )],
            ));
        }
        ops.push(Operation::new("ET", vec![]));

        let raw = Content { operations: ops }.encode().expect("encode PDF");
        let mut stream = Stream::new(dictionary! {}, raw);
        stream.compress().expect("compress PDF stream");
        let content_id = doc.add_object(stream);
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        kids.push(page_id.into());
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => pages.len() as i64,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut out = Vec::new();
    doc.save_to(&mut out).expect("save PDF");
    out
}

// =================================================================================================
// Pure-Rust ISO/IEC 10918-1 Baseline DCT JPEG Encoder + Scanned Image-Only PDF Builder (`0` text ops)
// =================================================================================================

const ZIGZAG_TO_NAT: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10,
    17, 24, 32, 25, 18, 11, 4, 5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13, 6, 7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

const STD_LUMA_Q: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61,
    12, 12, 14, 19, 26, 58, 60, 55,
    14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62,
    18, 22, 37, 56, 68, 109, 103, 77,
    24, 35, 55, 64, 81, 104, 113, 92,
    49, 64, 78, 87, 103, 121, 120, 101,
    72, 92, 95, 98, 112, 100, 103, 99,
];

const STD_CHROMA_Q: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99,
    18, 21, 26, 66, 99, 99, 99, 99,
    24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99,
];

const BITS_DC_LUMA: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const VALS_DC_LUMA: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const BITS_DC_CHROMA: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const VALS_DC_CHROMA: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const BITS_AC_LUMA: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
const VALS_AC_LUMA: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];
const BITS_AC_CHROMA: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
const VALS_AC_CHROMA: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

fn build_huff_table(bits: &[u8; 16], vals: &[u8]) -> [(u16, u8); 256] {
    let mut table = [(0u16, 0u8); 256];
    let mut code: u16 = 0;
    let mut k = 0usize;
    for len in 1..=16 {
        let count = bits[len - 1] as usize;
        for _ in 0..count {
            table[vals[k] as usize] = (code, len as u8);
            code += 1;
            k += 1;
        }
        code <<= 1;
    }
    table
}

fn scale_qtable_zigzag(std_q_nat: &[u8; 64], scale_pct: u32) -> [u8; 64] {
    let mut out = [1u8; 64];
    for z in 0..64 {
        let nat = ZIGZAG_TO_NAT[z];
        let v = ((std_q_nat[nat] as u32 * scale_pct + 50) / 100).clamp(1, 255);
        out[z] = v as u8;
    }
    out
}

struct JpegBitWriter {
    buf: Vec<u8>,
    acc: u32,
    bits: u8,
}

impl JpegBitWriter {
    fn new(buf: Vec<u8>) -> Self {
        Self { buf, acc: 0, bits: 0 }
    }
    fn write_bits(&mut self, val: u16, len: u8) {
        if len == 0 {
            return;
        }
        let mask = (1u32 << len) - 1;
        self.acc = (self.acc << len) | ((val as u32) & mask);
        self.bits += len;
        while self.bits >= 8 {
            self.bits -= 8;
            let b = ((self.acc >> self.bits) & 0xFF) as u8;
            self.buf.push(b);
            if b == 0xFF {
                self.buf.push(0x00);
            }
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            let pad = 8 - self.bits;
            self.write_bits((1u16 << pad) - 1, pad);
        }
        self.buf
    }
}

fn jpeg_bit_category(v: i32) -> (u8, u16) {
    if v == 0 {
        return (0, 0);
    }
    let abs = v.unsigned_abs();
    let cat = (32 - abs.leading_zeros()) as u8;
    let bits = if v > 0 {
        v as u16
    } else {
        ((v - 1) as i16 as u16) & ((1u16 << cat) - 1)
    };
    (cat, bits)
}

fn fdct_quant_zigzag(block: &[f64; 64], q_natural: &[u8; 64], cos_tbl: &[[f64; 8]; 8]) -> [i32; 64] {
    let mut tmp = [0.0f64; 64];
    for r in 0..8 {
        for u in 0..8 {
            let cu = if u == 0 { (0.5f64).sqrt() } else { 1.0 };
            let mut s = 0.0;
            for x in 0..8 {
                s += block[r * 8 + x] * cos_tbl[u][x];
            }
            tmp[r * 8 + u] = 0.5 * cu * s;
        }
    }
    let mut nat_q = [0i32; 64];
    for v in 0..8 {
        let cv = if v == 0 { (0.5f64).sqrt() } else { 1.0 };
        for u in 0..8 {
            let mut s = 0.0;
            for y in 0..8 {
                s += tmp[y * 8 + u] * cos_tbl[v][y];
            }
            let coeff = 0.5 * cv * s;
            let q = q_natural[v * 8 + u] as f64;
            nat_q[v * 8 + u] = (coeff / q).round() as i32;
        }
    }
    let mut zz = [0i32; 64];
    for z in 0..64 {
        zz[z] = nat_q[ZIGZAG_TO_NAT[z]];
    }
    zz
}

fn encode_jpeg_du(
    bw: &mut JpegBitWriter,
    zz: &[i32; 64],
    prev_dc: &mut i32,
    dc_tbl: &[(u16, u8); 256],
    ac_tbl: &[(u16, u8); 256],
) {
    let diff = zz[0] - *prev_dc;
    *prev_dc = zz[0];
    let (dc_cat, dc_bits) = jpeg_bit_category(diff);
    let (dc_code, dc_len) = dc_tbl[dc_cat as usize];
    bw.write_bits(dc_code, dc_len);
    bw.write_bits(dc_bits, dc_cat);

    let mut zero_run = 0usize;
    for &ac in &zz[1..] {
        if ac == 0 {
            zero_run += 1;
        } else {
            while zero_run >= 16 {
                let (zrl_c, zrl_l) = ac_tbl[0xF0];
                bw.write_bits(zrl_c, zrl_l);
                zero_run -= 16;
            }
            let (cat, bits) = jpeg_bit_category(ac);
            let sym = ((zero_run as u8) << 4) | cat;
            let (ac_c, ac_l) = ac_tbl[sym as usize];
            bw.write_bits(ac_c, ac_l);
            bw.write_bits(bits, cat);
            zero_run = 0;
        }
    }
    if zero_run > 0 {
        let (eob_c, eob_l) = ac_tbl[0x00];
        bw.write_bits(eob_c, eob_l);
    }
}

fn encode_rgb_baseline_jpeg(width: usize, height: usize, rgb: &[u8]) -> Vec<u8> {
    assert_eq!(width % 8, 0);
    assert_eq!(height % 8, 0);
    assert_eq!(rgb.len(), width * height * 3);

    let q_luma_zz = scale_qtable_zigzag(&STD_LUMA_Q, 20);
    let q_chroma_zz = scale_qtable_zigzag(&STD_CHROMA_Q, 20);
    let mut q_luma_nat = [1u8; 64];
    let mut q_chroma_nat = [1u8; 64];
    for i in 0..64 {
        q_luma_nat[i] = ((STD_LUMA_Q[i] as u32 * 20 + 50) / 100).clamp(1, 255) as u8;
        q_chroma_nat[i] = ((STD_CHROMA_Q[i] as u32 * 20 + 50) / 100).clamp(1, 255) as u8;
    }

    let dc_luma = build_huff_table(&BITS_DC_LUMA, &VALS_DC_LUMA);
    let dc_chroma = build_huff_table(&BITS_DC_CHROMA, &VALS_DC_CHROMA);
    let ac_luma = build_huff_table(&BITS_AC_LUMA, &VALS_AC_LUMA);
    let ac_chroma = build_huff_table(&BITS_AC_CHROMA, &VALS_AC_CHROMA);

    let mut cos_tbl = [[0.0f64; 8]; 8];
    for u in 0..8 {
        for x in 0..8 {
            cos_tbl[u][x] = (((2 * x + 1) * u) as f64 * PI / 16.0).cos();
        }
    }

    let mut out = Vec::with_capacity(65_536);
    out.extend_from_slice(&[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x01,
        0x00, 0x96, 0x00, 0x96, 0x00, 0x00,
    ]);
    for (id, qt) in [(0u8, &q_luma_zz), (1u8, &q_chroma_zz)] {
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 67, id]);
        out.extend_from_slice(qt);
    }
    out.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 17, 8,
        (height >> 8) as u8, (height & 0xFF) as u8,
        (width >> 8) as u8, (width & 0xFF) as u8,
        3,
        1, 0x11, 0,
        2, 0x11, 1,
        3, 0x11, 1,
    ]);
    let htables: [(u8, &[u8; 16], &[u8]); 4] = [
        (0x00, &BITS_DC_LUMA, &VALS_DC_LUMA),
        (0x10, &BITS_AC_LUMA, &VALS_AC_LUMA),
        (0x01, &BITS_DC_CHROMA, &VALS_DC_CHROMA),
        (0x11, &BITS_AC_CHROMA, &VALS_AC_CHROMA),
    ];
    for (tc_th, bits, vals) in htables {
        let seg_len = (19 + vals.len()) as u16;
        out.extend_from_slice(&[0xFF, 0xC4, (seg_len >> 8) as u8, (seg_len & 0xFF) as u8, tc_th]);
        out.extend_from_slice(bits);
        out.extend_from_slice(vals);
    }
    out.extend_from_slice(&[
        0xFF, 0xDA, 0x00, 12, 3,
        1, 0x00,
        2, 0x11,
        3, 0x11,
        0, 63, 0,
    ]);

    let mut bw = JpegBitWriter::new(out);
    let (mut dc_y, mut dc_cb, mut dc_cr) = (0i32, 0i32, 0i32);
    for by in (0..height).step_by(8) {
        for bx in (0..width).step_by(8) {
            let mut blk_y = [0.0f64; 64];
            let mut blk_cb = [0.0f64; 64];
            let mut blk_cr = [0.0f64; 64];
            for py in 0..8 {
                for px in 0..8 {
                    let idx = ((by + py) * width + (bx + px)) * 3;
                    let r = rgb[idx] as f64;
                    let g = rgb[idx + 1] as f64;
                    let b = rgb[idx + 2] as f64;
                    let k = py * 8 + px;
                    blk_y[k] = (0.299 * r + 0.587 * g + 0.114 * b) - 128.0;
                    blk_cb[k] = (-0.168736 * r - 0.331264 * g + 0.5 * b).clamp(-128.0, 127.0);
                    blk_cr[k] = (0.5 * r - 0.418688 * g - 0.081312 * b).clamp(-128.0, 127.0);
                }
            }
            let zz_y = fdct_quant_zigzag(&blk_y, &q_luma_nat, &cos_tbl);
            let zz_cb = fdct_quant_zigzag(&blk_cb, &q_chroma_nat, &cos_tbl);
            let zz_cr = fdct_quant_zigzag(&blk_cr, &q_chroma_nat, &cos_tbl);
            encode_jpeg_du(&mut bw, &zz_y, &mut dc_y, &dc_luma, &ac_luma);
            encode_jpeg_du(&mut bw, &zz_cb, &mut dc_cb, &dc_chroma, &ac_chroma);
            encode_jpeg_du(&mut bw, &zz_cr, &mut dc_cr, &dc_chroma, &ac_chroma);
        }
    }

    let mut out = bw.finish();
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

fn glyph_5x7(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        ':' => [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100],
        ',' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00100, 0b00100, 0b01000],
        '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
        '#' => [0b01010, 0b11111, 0b01010, 0b01010, 0b11111, 0b01010, 0b00000],
        '(' => [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010],
        ')' => [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000],
        '[' => [0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110],
        ']' => [0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        '|' => [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        '$' => [0b00100, 0b01111, 0b10100, 0b01110, 0b00101, 0b11110, 0b00100],
        _ => [0; 7],
    }
}

fn fill_rgb_rect(rgb: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, rw: usize, rh: usize, c: [u8; 3]) {
    for y in y0..(y0 + rh).min(h) {
        for x in x0..(x0 + rw).min(w) {
            let idx = (y * w + x) * 3;
            rgb[idx..idx + 3].copy_from_slice(&c);
        }
    }
}

fn stroke_rgb_rect(rgb: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, rw: usize, rh: usize, t: usize, c: [u8; 3]) {
    fill_rgb_rect(rgb, w, h, x0, y0, rw, t, c);
    fill_rgb_rect(rgb, w, h, x0, (y0 + rh).saturating_sub(t), rw, t, c);
    fill_rgb_rect(rgb, w, h, x0, y0, t, rh, c);
    fill_rgb_rect(rgb, w, h, (x0 + rw).saturating_sub(t), y0, t, rh, c);
}

fn draw_rgb_text(rgb: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, scale: usize, text: &str, c: [u8; 3]) {
    let mut cx = x0;
    for ch in text.chars() {
        let g = glyph_5x7(ch);
        for (ry, &row) in g.iter().enumerate() {
            for rx in 0..5 {
                if (row >> (4 - rx)) & 1 == 1 {
                    fill_rgb_rect(rgb, w, h, cx + rx * scale, y0 + ry * scale, scale, scale, c);
                }
            }
        }
        cx += 6 * scale;
    }
}

/// Builds a 1-page Scanned Image-Only PDF (`616x792` `/DCTDecode` JPEG XObject with **0** PDF text operators).
fn build_scanned_jpeg_pdf_bytes() -> Vec<u8> {
    let (w, h) = (616usize, 792usize);
    let mut rgb = vec![0u8; w * h * 3];
    fill_rgb_rect(&mut rgb, w, h, 0, 0, w, h, [250, 248, 243]);
    stroke_rgb_rect(&mut rgb, w, h, 18, 18, w - 36, h - 36, 2, [180, 188, 198]);

    fill_rgb_rect(&mut rgb, w, h, 32, 32, w - 64, 58, [15, 42, 67]);
    draw_rgb_text(&mut rgb, w, h, 48, 44, 2, "SCANNED SELLER DISCLOSURE (TDS)", [255, 255, 255]);
    draw_rgb_text(&mut rgb, w, h, 48, 66, 2, "100 SYNTHPARCELALPHA LN | APN 421-18-022", [186, 230, 253]);

    let rows = [
        (116, true, "1. GARAGE SUBPANEL: FEDERAL PACIFIC STAB-LOK"),
        (178, true, "2. ROOFING: ACTIVE CHIMNEY FLASHING STAINING"),
        (240, true, "3. KITCHEN PERMIT #B-2019-4410 EXPIRED"),
        (302, false, "4. SPECIAL FLOOD HAZARD AREA (FEMA ZONE X)"),
    ];
    for (y, yes, label) in rows {
        fill_rgb_rect(&mut rgb, w, h, 32, y, w - 64, 48, [255, 255, 255]);
        stroke_rgb_rect(&mut rgb, w, h, 32, y, w - 64, 48, 1, [203, 213, 225]);
        let badge = if yes { "[X] YES" } else { "[ ] NO " };
        let badge_col = if yes { [185, 28, 28] } else { [15, 118, 110] };
        draw_rgb_text(&mut rgb, w, h, 44, y + 10, 2, badge, badge_col);
        draw_rgb_text(&mut rgb, w, h, 44, y + 29, 2, label, [30, 41, 59]);
    }

    fill_rgb_rect(&mut rgb, w, h, 32, 376, w - 64, 270, [255, 255, 255]);
    stroke_rgb_rect(&mut rgb, w, h, 32, 376, w - 64, 270, 2, [148, 163, 184]);
    draw_rgb_text(&mut rgb, w, h, 48, 392, 2, "SCANNED INSPECTOR FLOORPLAN SKETCH", [51, 65, 85]);
    stroke_rgb_rect(&mut rgb, w, h, 68, 424, 300, 190, 3, [51, 65, 85]);
    draw_rgb_text(&mut rgb, w, h, 84, 508, 2, "MAIN HOUSE (2000 SQFT)", [71, 85, 105]);
    fill_rgb_rect(&mut rgb, w, h, 374, 454, 178, 160, [254, 242, 242]);
    stroke_rgb_rect(&mut rgb, w, h, 374, 454, 178, 160, 3, [220, 38, 38]);
    draw_rgb_text(&mut rgb, w, h, 390, 484, 2, "GARAGE", [153, 27, 27]);
    draw_rgb_text(&mut rgb, w, h, 390, 514, 2, "FPE PANEL", [185, 28, 28]);
    draw_rgb_text(&mut rgb, w, h, 390, 540, 2, "HAZARD", [185, 28, 28]);

    stroke_rgb_rect(&mut rgb, w, h, 32, 668, w - 64, 78, 1, [148, 163, 184]);
    draw_rgb_text(&mut rgb, w, h, 48, 684, 2, "SELLER ACKNOWLEDGMENT (SCANNED RASTER PAGE)", [51, 65, 85]);
    draw_rgb_text(&mut rgb, w, h, 48, 714, 2, "SIGNED: ALEX OWNER          DATE: 2026-11-10", [15, 118, 110]);

    let jpeg_bytes = encode_rgb_baseline_jpeg(w, h, &rgb);

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let img_stream = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => w as i64,
            "Height" => h as i64,
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => 8,
            "Filter" => "DCTDecode",
        },
        jpeg_bytes,
    );
    let img_id = doc.add_object(img_stream);
    let resources_id = doc.add_object(dictionary! {
        "XObject" => dictionary! { "Im0" => img_id },
    });

    // Zero text operators (no BT/ET/Tj/TJ) — strictly graphics state + matrix + paint Image XObject
    let content_ops = vec![
        Operation::new("q", vec![]),
        Operation::new(
            "cm",
            vec![
                612.into(),
                0.into(),
                0.into(),
                792.into(),
                0.into(),
                0.into(),
            ],
        ),
        Operation::new("Do", vec!["Im0".into()]),
        Operation::new("Q", vec![]),
    ];
    let raw_content = Content {
        operations: content_ops,
    }
    .encode()
    .expect("encode scanned PDF content");
    let content_id = doc.add_object(Stream::new(dictionary! {}, raw_content));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
        "Resources" => resources_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut out = Vec::new();
    doc.save_to(&mut out).expect("save scanned PDF");
    out
}

fn build_pkzip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut cd_records: Vec<Vec<u8>> = Vec::with_capacity(entries.len());

    for &(name, raw) in entries {
        let local_offset = buf.len() as u32;
        let crc = crc32_ieee(raw);
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
        enc.write_all(raw).expect("deflate");
        let comp = enc.finish().expect("finish deflate");
        let name_bytes = name.as_bytes();

        buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        buf.extend_from_slice(&20u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&8u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&comp);

        let mut cd = Vec::new();
        cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&8u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&crc.to_le_bytes());
        cd.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&local_offset.to_le_bytes());
        cd.extend_from_slice(name_bytes);
        cd_records.push(cd);
    }

    let cd_offset = buf.len() as u32;
    let mut cd_size = 0u32;
    for cd in &cd_records {
        cd_size += cd.len() as u32;
        buf.extend_from_slice(cd);
    }

    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf
}

fn discover_chromium_executable() -> PathBuf {
    if let Ok(raw) = std::env::var("PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let p = PathBuf::from(trimmed);
            if p.is_file() {
                return p;
            }
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let cache_root = Path::new(&home).join(".cache/ms-playwright");
        if let Ok(rd) = std::fs::read_dir(&cache_root) {
            let mut dirs: Vec<(u64, PathBuf)> = rd
                .flatten()
                .filter_map(|ent| {
                    let name = ent.file_name().to_string_lossy().into_owned();
                    let rev_str = name.strip_prefix("chromium-")?;
                    let rev: u64 = rev_str.parse().unwrap_or(0);
                    Some((rev, ent.path()))
                })
                .collect();
            dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
            for (_, dir) in dirs {
                for sub in ["chrome-linux64/chrome", "chrome-linux/chrome"] {
                    let candidate = dir.join(sub);
                    if candidate.is_file() {
                        return candidate;
                    }
                }
            }
        }
    }

    for bin_name in ["chromium", "chromium-browser", "google-chrome"] {
        if let Ok(out) = Command::new("which").arg(bin_name).output() {
            if out.status.success() {
                let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
                if p.is_file() {
                    return p;
                }
            }
        }
    }

    panic!("Headless Chromium binary not found (checked PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH, ~/.cache/ms-playwright/chromium-*/chrome-linux{{64,}}/chrome, and PATH)");
}

fn discover_node_executable() -> PathBuf {
    if let Ok(out) = Command::new("node").arg("--version").output() {
        if out.status.success() {
            return PathBuf::from("node");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let fallback = Path::new(&home).join(".local/bin/node");
        if fallback.is_file() {
            return fallback;
        }
    }
    panic!("Node >= 22/24 executable with built-in WebSocket is required");
}

const CDP_E2E_MJS: &str = r###"
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const CHROMIUM_EXEC = process.env.CHROMIUM_EXEC;
const BASE_URL = process.env.BASE_URL.replace(/\/+$/, '');
const PRIMARY_SLUG = process.env.PRIMARY_SLUG;
const SECONDARY_SLUG = process.env.SECONDARY_SLUG;
const SINGLE_PDF_PATH = process.env.SINGLE_PDF_PATH;
const ZIP_PACKET_PATH = process.env.ZIP_PACKET_PATH;
const SCANNED_PDF_PATH = process.env.SCANNED_PDF_PATH;
const UNSAFE_ZIP_PATH = process.env.UNSAFE_ZIP_PATH;
const USER_DATA_DIR = process.env.USER_DATA_DIR;
const SCREENSHOT_DIR = (process.env.OPEN_ESTATE_E2E_SCREENSHOT_DIR || '').trim();

if (SCREENSHOT_DIR) {
  fs.mkdirSync(SCREENSHOT_DIR, { recursive: true });
}

class CdpConnection {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 0;
    this.pending = new Map();
    this.listeners = new Map();

    ws.addEventListener('message', async (ev) => {
      let text;
      if (typeof ev.data === 'string') {
        text = ev.data;
      } else if (ev.data instanceof ArrayBuffer) {
        text = Buffer.from(ev.data).toString('utf8');
      } else if (ev.data && typeof ev.data.text === 'function') {
        text = await ev.data.text();
      } else {
        text = String(ev.data);
      }
      const msg = JSON.parse(text);
      if (msg.id != null) {
        const slot = this.pending.get(msg.id);
        if (slot) {
          this.pending.delete(msg.id);
          clearTimeout(slot.timer);
          if (msg.error) {
            slot.reject(new Error(`CDP ${slot.method} error: ${msg.error.message} (${msg.error.code})`));
          } else {
            slot.resolve(msg.result || {});
          }
        }
        return;
      }
      if (msg.method) {
        const key = `${msg.sessionId || 'browser'}::${msg.method}`;
        const list = this.listeners.get(key);
        if (list) {
          for (const fn of [...list]) {
            try { fn(msg.params || {}); } catch (_) {}
          }
        }
      }
    });

    ws.addEventListener('close', () => {
      for (const [id, slot] of this.pending.entries()) {
        clearTimeout(slot.timer);
        slot.reject(new Error(`CDP WebSocket closed while awaiting ${slot.method}`));
        this.pending.delete(id);
      }
    });
  }

  send(method, params = {}, sessionId = undefined, timeoutMs = 30000) {
    const id = ++this.nextId;
    const payload = sessionId ? { id, method, params, sessionId } : { id, method, params };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP ${method} timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, method, timer });
      this.ws.send(JSON.stringify(payload));
    });
  }

  on(sessionId, method, handler) {
    const key = `${sessionId || 'browser'}::${method}`;
    let arr = this.listeners.get(key);
    if (!arr) {
      arr = [];
      this.listeners.set(key, arr);
    }
    arr.push(handler);
    return () => {
      const idx = arr.indexOf(handler);
      if (idx >= 0) arr.splice(idx, 1);
    };
  }
}

class CdpPage {
  constructor(cdp, sessionId, browserContextId, targetId) {
    this.cdp = cdp;
    this.sessionId = sessionId;
    this.browserContextId = browserContextId;
    this.targetId = targetId;
    this.pageErrors = [];
    this.consoleErrors = [];
    this.blockedHttpsCount = 0;

    this.cdp.on(this.sessionId, 'Fetch.requestPaused', (params) => {
      const url = (params.request && params.request.url) || '';
      if (url.startsWith('https://')) {
        this.blockedHttpsCount += 1;
        this.cdp.send('Fetch.failRequest', {
          requestId: params.requestId,
          errorReason: 'Failed',
        }, this.sessionId).catch(() => {});
      } else {
        this.cdp.send('Fetch.continueRequest', {
          requestId: params.requestId,
        }, this.sessionId).catch(() => {});
      }
    });

    this.cdp.on(this.sessionId, 'Runtime.exceptionThrown', (params) => {
      const det = params.exceptionDetails || {};
      const msg = (det.exception && (det.exception.description || det.exception.value))
        || det.text
        || JSON.stringify(det);
      this.pageErrors.push(String(msg));
    });

    this.cdp.on(this.sessionId, 'Runtime.consoleAPICalled', (params) => {
      if (params.type === 'error') {
        const text = (params.args || [])
          .map((a) => a.value !== undefined ? String(a.value) : (a.description || a.type || ''))
          .join(' ');
        this.consoleErrors.push(text);
      }
    });

    this.cdp.on(this.sessionId, 'Page.javascriptDialogOpening', () => {
      this.cdp.send('Page.handleJavaScriptDialog', { accept: true }, this.sessionId).catch(() => {});
    });
  }

  async init() {
    await this.cdp.send('Page.enable', {}, this.sessionId);
    await this.cdp.send('Runtime.enable', {}, this.sessionId);
    await this.cdp.send('DOM.enable', {}, this.sessionId);
    await this.cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 1440,
      height: 920,
      deviceScaleFactor: 1,
      mobile: false,
    }, this.sessionId);
    await this.cdp.send('Fetch.enable', {
      patterns: [{ urlPattern: 'https://*', requestStage: 'Request' }],
    }, this.sessionId);
    await this.cdp.send('DOM.getDocument', { depth: 0 }, this.sessionId);
  }

  async close() {
    await this.cdp.send('Target.disposeBrowserContext', {
      browserContextId: this.browserContextId,
    }).catch(() => {});
  }

  async eval(fnOrExpr, ...args) {
    const expression = typeof fnOrExpr === 'function'
      ? `(${fnOrExpr.toString()})(${args.map((a) => JSON.stringify(a)).join(',')})`
      : String(fnOrExpr);
    const res = await this.cdp.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    }, this.sessionId);
    if (res.exceptionDetails) {
      const det = res.exceptionDetails;
      const errText = (det.exception && (det.exception.description || det.exception.value))
        || det.text
        || 'Runtime.evaluate exception';
      throw new Error(`JS evaluation error: ${errText}`);
    }
    return res.result ? res.result.value : undefined;
  }

  async waitFor(fnOrExpr, opts = {}, ...args) {
    const timeout = opts.timeout ?? 20000;
    const interval = opts.interval ?? 50;
    const desc = opts.desc || 'condition';
    const deadline = Date.now() + timeout;
    let lastErr = null;
    while (Date.now() < deadline) {
      try {
        const val = await this.eval(fnOrExpr, ...args);
        if (val) return val;
        lastErr = null;
      } catch (e) {
        lastErr = e;
      }
      await new Promise((r) => setTimeout(r, interval));
    }
    const diag = await this.eval(() => ({
      url: location.href,
      title: document.title,
      errBanner: document.getElementById('bcErrBanner')?.textContent || '',
      psErrBanner: document.getElementById('psErrorBannerText')?.textContent || '',
      bodyLen: (document.body?.innerText || '').length,
    })).catch(() => ({}));
    throw new Error(`Timed out after ${timeout}ms waiting for ${desc}${lastErr ? ` (${lastErr.message})` : ''} | diag=${JSON.stringify(diag)}`);
  }

  async goto(url) {
    let loaded = false;
    const unsub = this.cdp.on(this.sessionId, 'Page.loadEventFired', () => {
      loaded = true;
    });
    try {
      const nav = await this.cdp.send('Page.navigate', { url }, this.sessionId);
      if (nav.errorText) {
        throw new Error(`Navigation failed to ${url}: ${nav.errorText}`);
      }
      const deadline = Date.now() + 20000;
      while (!loaded && Date.now() < deadline) {
        await new Promise((r) => setTimeout(r, 30));
      }
      await this.waitFor(() => document.readyState === 'complete', { desc: `document.readyState === complete (${url})` });
    } finally {
      unsub();
    }
    await this.cdp.send('DOM.getDocument', { depth: 0 }, this.sessionId);
  }

  async click(selector, opts = {}) {
    const box = await this.waitFor((sel) => {
      const el = document.querySelector(sel);
      if (!el || el.disabled) return null;
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      const r = el.getBoundingClientRect();
      if (r.width <= 0 || r.height <= 0) return null;
      return {
        x: Math.max(1, Math.min(window.innerWidth - 2, r.left + r.width / 2)),
        y: Math.max(1, Math.min(window.innerHeight - 2, r.top + r.height / 2)),
      };
    }, { desc: `clickable ${selector}`, timeout: opts.timeout ?? 15000 }, selector);

    await this.cdp.send('Input.dispatchMouseEvent', {
      type: 'mouseMoved',
      x: box.x,
      y: box.y,
      button: 'none',
    }, this.sessionId);
    await this.cdp.send('Input.dispatchMouseEvent', {
      type: 'mousePressed',
      x: box.x,
      y: box.y,
      button: 'left',
      buttons: 1,
      clickCount: 1,
    }, this.sessionId);
    await this.cdp.send('Input.dispatchMouseEvent', {
      type: 'mouseReleased',
      x: box.x,
      y: box.y,
      button: 'left',
      buttons: 0,
      clickCount: 1,
    }, this.sessionId);
  }

  async fill(selector, value) {
    await this.waitFor((sel) => !!document.querySelector(sel), { desc: `input ${selector}` }, selector);
    await this.eval((sel, val) => {
      const el = document.querySelector(sel);
      el.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
      el.focus();
      const proto = el.tagName === 'TEXTAREA'
        ? HTMLTextAreaElement.prototype
        : HTMLInputElement.prototype;
      const desc = Object.getOwnPropertyDescriptor(proto, 'value');
      if (desc && desc.set) desc.set.call(el, String(val));
      else el.value = String(val);
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    }, selector, String(value));
  }

  async selectOption(selector, value) {
    await this.waitFor((sel) => !!document.querySelector(sel), { desc: `select ${selector}` }, selector);
    await this.eval((sel, val) => {
      const el = document.querySelector(sel);
      el.value = String(val);
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
    }, selector, String(value));
  }

  async setFiles(selector, filePaths) {
    let lastErr = null;
    for (let attempt = 0; attempt < 4; attempt++) {
      try {
        await this.waitFor((sel) => !!document.querySelector(sel), { desc: `file input ${selector}` }, selector);
        const evalRes = await this.cdp.send('Runtime.evaluate', {
          expression: `document.querySelector(${JSON.stringify(selector)})`,
          returnByValue: false,
        }, this.sessionId);
        const objectId = evalRes.result && evalRes.result.objectId;
        assert(objectId, `Could not resolve objectId for file input ${selector}`);
        const desc = await this.cdp.send('DOM.describeNode', { objectId }, this.sessionId);
        const backendNodeId = desc && desc.node && desc.node.backendNodeId;
        await this.cdp.send('DOM.setFileInputFiles', {
          ...(backendNodeId ? { backendNodeId } : { objectId }),
          files: filePaths,
        }, this.sessionId);
        return;
      } catch (err) {
        lastErr = err;
        await new Promise((r) => setTimeout(r, 120));
      }
    }
    throw lastErr;
  }

  async dragOnElement(selector, fromNorm, toNorm, steps = 8) {
    const rect = await this.waitFor((sel) => {
      const el = document.querySelector(sel);
      if (!el) return null;
      el.scrollIntoView({ block: 'nearest', inline: 'nearest', behavior: 'instant' });
      const r = el.getBoundingClientRect();
      if (r.width <= 10 || r.height <= 10) return null;
      let visLeft = Math.max(0, r.left);
      let visTop = Math.max(0, r.top);
      let visRight = Math.min(window.innerWidth, r.right);
      let visBottom = Math.min(window.innerHeight, r.bottom);
      let anc = el.parentElement;
      while (anc && anc !== document.body) {
        const cs = getComputedStyle(anc);
        if (/(auto|scroll|hidden|clip)/.test(`${cs.overflowY} ${cs.overflowX}`)) {
          const ar = anc.getBoundingClientRect();
          visLeft = Math.max(visLeft, ar.left);
          visTop = Math.max(visTop, ar.top);
          visRight = Math.min(visRight, ar.right);
          visBottom = Math.min(visBottom, ar.bottom);
        }
        anc = anc.parentElement;
      }
      if (visRight - visLeft < 20 || visBottom - visTop < 20) return null;
      return { left: visLeft, top: visTop, width: visRight - visLeft, height: visBottom - visTop };
    }, { desc: `draggable ${selector}` }, selector);

    const sx = rect.left + rect.width * fromNorm[0];
    const sy = rect.top + rect.height * fromNorm[1];
    const ex = rect.left + rect.width * toNorm[0];
    const ey = rect.top + rect.height * toNorm[1];
    const hitInfo = await this.eval((x, y, sel) => {
      const top = document.elementFromPoint(x, y);
      const target = document.querySelector(sel);
      return {
        matches: !!(top && target && (top === target || target.contains(top))),
        topTag: top ? `${top.tagName}#${top.id || ''}.${top.className || ''}` : 'null',
      };
    }, sx, sy, selector);
    if (!hitInfo.matches) {
      throw new Error(`dragOnElement(${selector}) point (${Math.round(sx)},${Math.round(sy)}) intercepted by ${hitInfo.topTag}`);
    }

    await this.cdp.send('Input.dispatchMouseEvent', {
      type: 'mouseMoved',
      x: sx,
      y: sy,
      button: 'none',
    }, this.sessionId);
    await this.cdp.send('Input.dispatchMouseEvent', {
      type: 'mousePressed',
      x: sx,
      y: sy,
      button: 'left',
      buttons: 1,
      clickCount: 1,
    }, this.sessionId);
    for (let i = 1; i <= steps; i++) {
      const t = i / steps;
      await this.cdp.send('Input.dispatchMouseEvent', {
        type: 'mouseMoved',
        x: sx + (ex - sx) * t,
        y: sy + (ey - sy) * t,
        button: 'left',
        buttons: 1,
      }, this.sessionId);
    }
    await this.cdp.send('Input.dispatchMouseEvent', {
      type: 'mouseReleased',
      x: ex,
      y: ey,
      button: 'left',
      buttons: 0,
      clickCount: 1,
    }, this.sessionId);
  }

  async snap(filename) {
    if (!SCREENSHOT_DIR) return;
    await this.eval(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    const { data } = await this.cdp.send('Page.captureScreenshot', { format: 'png' }, this.sessionId);
    const outPath = path.join(SCREENSHOT_DIR, filename);
    fs.writeFileSync(outPath, Buffer.from(data, 'base64'));
    console.log(`   📸 saved ${outPath}`);
  }

  assertZeroErrorsAndNoOverflow(label) {
    assert.deepStrictEqual(
      this.pageErrors,
      [],
      `[${label}] Unexpected uncaught pageerror(s): ${JSON.stringify(this.pageErrors)}`
    );
    assert.deepStrictEqual(
      this.consoleErrors,
      [],
      `[${label}] Unexpected console.error(s): ${JSON.stringify(this.consoleErrors)}`
    );
  }

  async assertNoHorizontalOverflow(label) {
    const dims = await this.eval(() => ({
      scrollWidth: document.documentElement.scrollWidth,
      innerWidth: window.innerWidth,
    }));
    assert(
      dims.scrollWidth <= dims.innerWidth + 2,
      `[${label}] Horizontal overflow detected: scrollWidth=${dims.scrollWidth} > innerWidth=${dims.innerWidth} + 2`
    );
  }
}

async function launchChromiumCdp() {
  const args = [
    '--headless=new',
    '--no-sandbox',
    '--disable-gpu',
    '--disable-dev-shm-usage',
    '--disable-background-networking',
    '--disable-component-update',
    '--disable-default-apps',
    '--disable-extensions',
    '--disable-sync',
    '--no-first-run',
    '--no-default-browser-check',
    '--metrics-recording-only',
    '--mute-audio',
    '--window-size=1440,920',
    '--user-agent=Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/153.0.0.0 Safari/537.36',
    '--remote-debugging-port=0',
    `--user-data-dir=${USER_DATA_DIR}`,
    'about:blank',
  ];

  const proc = spawn(CHROMIUM_EXEC, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let stderrBuf = '';
  const wsUrl = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`Chromium DevTools failed to start within 15s. stderr:\n${stderrBuf}`));
    }, 15000);

    const onChunk = (chunk) => {
      stderrBuf += chunk.toString('utf8');
      const m = stderrBuf.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      if (m) {
        clearTimeout(timer);
        resolve(m[1]);
      }
    };
    proc.stderr.on('data', onChunk);
    proc.stdout.on('data', onChunk);
    proc.on('exit', (code, sig) => {
      clearTimeout(timer);
      reject(new Error(`Chromium exited prematurely (code=${code}, sig=${sig}). stderr:\n${stderrBuf}`));
    });
  });

  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error('WebSocket connect timeout')), 10000);
    ws.addEventListener('open', () => { clearTimeout(t); resolve(); });
    ws.addEventListener('error', (e) => { clearTimeout(t); reject(new Error(`WebSocket error: ${e.message || e.type}`)); });
  });

  return { proc, ws, cdp: new CdpConnection(ws) };
}

async function createIsolatedPage(cdp) {
  const { browserContextId } = await cdp.send('Target.createBrowserContext', {
    disposeOnDetach: true,
  });
  const { targetId } = await cdp.send('Target.createTarget', {
    url: 'about:blank',
    browserContextId,
    width: 1440,
    height: 920,
  });
  const { sessionId } = await cdp.send('Target.attachToTarget', {
    targetId,
    flatten: true,
  });
  const page = new CdpPage(cdp, sessionId, browserContextId, targetId);
  await page.init();
  return page;
}

async function runFlow1AssistantAndSinglePdf(cdp) {
  console.log('▶ Flow 1: /assistant + Verified Email Challenge + Single .pdf + Self-Hosted PDF.js Studio');
  const page = await createIsolatedPage(cdp);
  try {
    await page.goto(`${BASE_URL}/assistant`);
    await page.waitFor(() => {
      const tab = document.getElementById('tab-assistant');
      const inp = document.getElementById('bcAddrInput');
      return !!(tab && tab.classList.contains('act') && inp);
    }, { desc: '/assistant active tab and Assistant mount' });

    const navChecks = await page.eval(() => {
      const btn = document.getElementById('tabBtnAssistant');
      const alertsBtn = document.getElementById('tabBtnAlerts');
      const smtpBanner = document.getElementById('alertsSmtpDisabledBanner');
      const visibleText = document.body ? document.body.innerText : '';
      const html = document.documentElement ? document.documentElement.innerHTML : '';
      return {
        tabBtnText: btn ? btn.textContent.trim() : '',
        alertsHidden: !!(alertsBtn && alertsBtn.hidden),
        smtpBannerPresent: !!smtpBanner,
        hasWhiteLabelAi: visibleText.includes('Open Estate AI'),
        hasBuyerAssistant: visibleText.includes('Buyer Assistant') || html.includes('Buyer Assistant'),
        hasAiAssistant: visibleText.includes('AI Assistant') || html.includes('AI Assistant'),
        hasBrandAssistant: visibleText.includes('Open Estate Assistant') || html.includes('Open Estate Assistant'),
      };
    });
    assert(
      navChecks.tabBtnText === 'Open Estate AI' && navChecks.hasWhiteLabelAi,
      `#tabBtnAssistant & /assistant must say 'Open Estate AI', got: ${JSON.stringify(navChecks)}`
    );
    assert.equal(navChecks.hasBuyerAssistant, false, 'Zero "Buyer Assistant" allowed on /assistant');
    assert.equal(navChecks.hasAiAssistant, false, 'Zero "AI Assistant" allowed on /assistant');
    assert.equal(navChecks.hasBrandAssistant, false, 'Zero "Open Estate Assistant" allowed on /assistant');
    assert.equal(navChecks.alertsHidden, true, '#tabBtnAlerts must be hidden when SMTP is disabled');
    assert.equal(navChecks.smtpBannerPresent, true, '#alertsSmtpDisabledBanner must be present');

    // Invalid address search -> #bcErrBanner.on
    await page.fill('#bcAddrInput', 'San Jose, CA');
    await page.click('#bcLookupBtn');
    const errText = await page.waitFor(() => {
      const el = document.getElementById('bcErrBanner');
      return el && el.classList.contains('on') && el.textContent.trim() ? el.textContent.trim() : null;
    }, { desc: '#bcErrBanner.on error for invalid address' });
    assert(errText.length > 5, `Expected clear invalid-address error, got: ${errText}`);

    // Off-market address soft gate sanity check
    await page.fill('#bcAddrInput', '200 Synthparcelbeta Ave, Campbell, CA 95008');
    await page.click('#bcLookupBtn');
    await page.waitFor(() => {
      const body = document.getElementById('bcDynamicBody');
      return body && body.innerText.includes('Not currently listed');
    }, { desc: 'Off-market status notice for secondary parcel' });

    // Active address search -> Actively For Sale + 3 Action Cards (1, 2, 3)
    await page.fill('#bcAddrInput', '100 Synthparcelalpha Ln, San Jose, CA 95118');
    await page.click('#bcLookupBtn');
    const activeInfo = await page.waitFor(() => {
      const body = document.getElementById('bcDynamicBody');
      if (!body || !body.innerText.includes('Actively For Sale')) return null;
      const nums = Array.from(body.querySelectorAll('[data-fn-pick] .bc-fn-num')).map((n) => n.textContent.trim());
      if (!nums.includes('1') || !nums.includes('2') || !nums.includes('3')) return null;
      return { nums, text: body.innerText };
    }, { desc: 'Actively For Sale + 3 action cards (1, 2, 3)' });
    assert(activeInfo.nums.includes('1') && activeInfo.nums.includes('2') && activeInfo.nums.includes('3'));

    // Open Function 2 (Disclosures): OSS allows direct .zip / PDF upload without account auth or sharing walls
    await page.click('[data-fn-pick="disclosures"]');
    await page.waitFor(
      () =>
        !!document.getElementById('bcDiscFileInput') &&
        !!document.getElementById('bcMailtoOpenLink') &&
        document.querySelectorAll('#bcPublicBaselineBox .bc-pub-card').length >= 1,
      {
        desc: 'Function 2 Disclosures upload dropzone + baseline cards (OSS no-auth)',
      }
    );

    const ossShareAbsence = await page.eval(() => {
      const sb = document.getElementById('shareReportBtn');
      const rbShare = document.querySelector('[data-rb-act="share"]');
      return {
        regBar: !!document.getElementById('bcRegUserBar'),
        shareForm: !!document.getElementById('bcShareDealTeamForm'),
        authSection: !!document.getElementById('bcAuthorizedSection'),
        shareBtnVisible: !!(sb && (!sb.hidden || getComputedStyle(sb).display !== 'none')),
        roleShareVisible: !!rbShare,
      };
    });
    assert.equal(ossShareAbsence.regBar, false, 'Open Estate OSS must not render #bcRegUserBar');
    assert.equal(ossShareAbsence.shareForm, false, 'Open Estate OSS must not render #bcShareDealTeamForm');
    assert.equal(ossShareAbsence.authSection, false, 'Open Estate OSS must not render #bcAuthorizedSection');
    assert.equal(ossShareAbsence.shareBtnVisible, false, 'Open Estate OSS must hide #shareReportBtn');
    assert.equal(ossShareAbsence.roleShareVisible, false, 'Open Estate OSS must not render role bar share link');

    const preUploadChecks = await page.eval(() => {
      const ret = document.getElementById('bcFileRetentionBanner');
      const mailCard = document.getElementById('bcMailtoRequestCard');
      const mailLink = document.getElementById('bcMailtoOpenLink');
      const pubBox = document.getElementById('bcPublicBaselineBox');
      const rawHref = mailLink ? (mailLink.getAttribute('href') || '') : '';
      let decodedHref = rawHref;
      try { decodedHref = decodeURIComponent(rawHref); } catch (_) {}
      return {
        retentionText: ret ? ret.innerText : '',
        mailCardPresent: !!mailCard,
        rawHref,
        decodedHref,
        pubBoxPresent: !!pubBox,
        pubBoxCardCount: pubBox ? pubBox.querySelectorAll('.bc-pub-card').length : 0,
      };
    });
    assert(
      preUploadChecks.retentionText.toLowerCase().includes('discard'),
      `#bcFileRetentionBanner must mention discarding original files: ${preUploadChecks.retentionText}`
    );
    assert(preUploadChecks.mailCardPresent, '#bcMailtoRequestCard must exist');
    assert(preUploadChecks.rawHref.startsWith('mailto:'), `mailtoHref must start with mailto:, got ${preUploadChecks.rawHref}`);
    assert(preUploadChecks.decodedHref.includes('421-18-022'), 'mailtoHref must contain APN 421-18-022');
    for (const kw of ['Home', 'Termite', 'Preliminary Title', 'Natural Hazard', 'HOA', 'Building Permits']) {
      assert(preUploadChecks.decodedHref.includes(kw), `mailtoHref missing priority doc '${kw}'`);
    }
    assert(preUploadChecks.pubBoxPresent && preUploadChecks.pubBoxCardCount >= 1, '#bcPublicBaselineBox (Mode 3) must render with 0 uploads');

    // Upload real single 2-page PDF via CDP DOM.setFileInputFiles
    await page.setFiles('#bcDiscFileInput', [SINGLE_PDF_PATH]);
    const singleResult = await page.waitFor(() => {
      const badge = document.getElementById('bcOneTimeCompletedBadge');
      const panel = document.getElementById('bcPanelFn2');
      if (!badge || !panel) return null;
      const cards = Array.from(panel.querySelectorAll('.bc-prio-card')).map((el) => ({
        text: el.innerText,
        missing: el.classList.contains('missing'),
      }));
      return {
        badgeText: badge.innerText,
        panelText: panel.innerText,
        cards,
      };
    }, { desc: 'Single PDF one-time analysis completion', timeout: 25000 });

    assert(singleResult.cards.length === 6, `Expected 6 priority cards, got ${singleResult.cards.length}`);
    assert.equal(singleResult.cards[0].missing, false, 'Priority #1 Home Inspection must be Present');
    for (let i = 1; i < 6; i++) {
      assert.equal(singleResult.cards[i].missing, true, `Priority #${i + 1} must be Missing`);
    }
    for (const missingKw of ['Termite', 'Preliminary Title', 'Natural Hazard', 'HOA', 'Building Permits']) {
      assert(singleResult.panelText.includes(missingKw), `Missing list should mention ${missingKw}`);
    }
    assert(singleResult.panelText.includes('Federal Pacific'), 'Red Flag "Federal Pacific" must be visible');
    await page.assertNoHorizontalOverflow('Flow 1 /assistant after single PDF');
    await page.snap('01_assistant_single_pdf.png');

    // Click Point & Circle in PDF -> PDF Studio (with https://* blocked, self-hosted /vendor/pdf.min.mjs MUST load!)
    await page.click('[data-pdf-cite-idx="0"]');
    const studioInit = await page.waitFor(() => {
      const bd = document.getElementById('oePdfStudioBackdrop');
      if (!bd || !bd.classList.contains('on')) return null;
      const st = window.OpenEstatePdfStudio?.getState?.();
      if (!st || !st.isOpen || st.currentDocName !== 'Home_Inspection_Single.pdf') return null;
      const pdfCanvas = document.getElementById('psPdfCanvas');
      if (!pdfCanvas || pdfCanvas.width < 100 || pdfCanvas.height < 100) return null;
      const ctx = pdfCanvas.getContext('2d');
      const data = ctx.getImageData(0, 0, pdfCanvas.width, pdfCanvas.height).data;
      let darkInkPixels = 0;
      for (let i = 0; i < data.length; i += 4) {
        if (data[i + 3] > 0 && (data[i] < 200 || data[i + 1] < 200 || data[i + 2] < 200)) {
          darkInkPixels++;
          if (darkInkPixels > 150) break;
        }
      }
      if (darkInkPixels <= 150) return null;
      const tl = document.getElementById('psTextLayer');
      const tlText = tl ? (tl.textContent || '') : '';
      const tlInner = tl ? (tl.innerText || '') : '';
      const tlSpanCount = tl ? tl.querySelectorAll('span').length : 0;
      if (st.pdfJsActive && tlSpanCount === 0) return null;
      const sel = document.getElementById('psFileSelect');
      const options = sel ? Array.from(sel.options).map((o) => o.textContent.trim()) : [];
      const banner = document.getElementById('psAiBanner');
      return {
        title: document.getElementById('psStudioTitle')?.textContent?.trim() || '',
        options,
        darkInkPixels,
        pdfJsActive: !!st.pdfJsActive,
        isScannedPage: !!st.isScannedPage,
        tlSpanCount,
        tlText,
        tlInner,
        activeAiPointer: st.activeAiPointer,
        bannerVisible: !!(banner && banner.style.display !== 'none'),
        numPages: st.numPages,
        currentPage: st.currentPage,
      };
    }, { desc: 'PDF Studio open + self-hosted PDF.js vector & TextLayer render for Home_Inspection_Single.pdf' });

    assert(
      studioInit.title.includes('Open Estate AI') && studioInit.title.includes('PDF Studio'),
      `#psStudioTitle must contain 'Open Estate AI — PDF Studio', got: ${studioInit.title}`
    );
    assert.equal(studioInit.pdfJsActive, true, 'Self-hosted /vendor/pdf.min.mjs must be active even with https://* blocked');
    assert.equal(studioInit.isScannedPage, false, 'Digital PDF must have isScannedPage === false');
    assert(studioInit.tlSpanCount >= 2, `Expected selectable TextLayer spans from self-hosted PDF.js, got ${studioInit.tlSpanCount}`);
    assert(
      !studioInit.tlText.includes('REPORTProperty') && !studioInit.tlInner.includes('REPORTProperty'),
      `TextLayer must preserve word spacing (zero REPORTProperty concatenation), got: ${studioInit.tlText}`
    );
    assert(studioInit.tlText.includes('HOME INSPECTION REPORT') && studioInit.tlText.includes('Property: 100 Synthparcelalpha Ln'));
    assert(studioInit.options.some((o) => o.includes('Home_Inspection_Single.pdf')), '#psFileSelect must contain Home_Inspection_Single.pdf');
    assert(studioInit.darkInkPixels > 150, '#psPdfCanvas must have rendered dark ink pixels');
    assert(studioInit.activeAiPointer && studioInit.activeAiPointer.matched && studioInit.bannerVisible, 'Active AI pointer must match coordinates on digital PDF');
    assert.equal(studioInit.numPages, 2, 'Home_Inspection_Single.pdf must render 2 pages');

    // Switch to page 2 and back to page 1
    await page.click('#psNextPageBtn');
    const p2Pixels = await page.waitFor(() => {
      const st = window.OpenEstatePdfStudio?.getState?.();
      if (!st || st.currentPage !== 2) return null;
      const pdfCanvas = document.getElementById('psPdfCanvas');
      const data = pdfCanvas.getContext('2d').getImageData(0, 0, pdfCanvas.width, pdfCanvas.height).data;
      let dark = 0;
      for (let i = 0; i < data.length; i += 4) {
        if (data[i + 3] > 0 && (data[i] < 200 || data[i + 1] < 200 || data[i + 2] < 200)) {
          dark++;
          if (dark > 80) return dark;
        }
      }
      return null;
    }, { desc: 'PDF Studio page 2 render' });
    assert(p2Pixels > 80, 'Page 2 must render dark ink pixels');

    await page.click('#psPrevPageBtn');
    await page.waitFor(
      () => window.OpenEstatePdfStudio?.getState?.()?.currentPage === 1 && !document.getElementById('psNextPageBtn')?.disabled,
      { desc: 'PDF Studio return to page 1' }
    );

    // Real mouse drag Circle, Arrow, and Pin annotations on #psAnnotCanvas
    await page.click('[data-ps-tool="circle"]');
    await page.waitFor(() => window.OpenEstatePdfStudio?.getState?.()?.tool === 'circle', {
      desc: 'tool === circle',
    });
    await page.dragOnElement('#psAnnotCanvas', [0.22, 0.24], [0.58, 0.38], 8);
    await page.waitFor(() => (window.OpenEstatePdfStudio?.getState?.()?.annotationsCount || 0) >= 1, {
      desc: 'Circle annotation persisted',
    });

    await page.click('[data-ps-tool="arrow"]');
    await page.waitFor(() => window.OpenEstatePdfStudio?.getState?.()?.tool === 'arrow', {
      desc: 'tool === arrow',
    });
    await page.dragOnElement('#psAnnotCanvas', [0.16, 0.48], [0.40, 0.34], 8);
    await page.waitFor(() => (window.OpenEstatePdfStudio?.getState?.()?.annotationsCount || 0) >= 2, {
      desc: 'Arrow annotation persisted',
    });

    await page.click('[data-ps-tool="pin"]');
    await page.waitFor(() => window.OpenEstatePdfStudio?.getState?.()?.tool === 'pin', {
      desc: 'tool === pin',
    });
    await page.dragOnElement('#psAnnotCanvas', [0.45, 0.56], [0.45, 0.56], 1);

    const annotState = await page.waitFor(() => {
      const st = window.OpenEstatePdfStudio?.getState?.();
      const chatVal = document.getElementById('psChatInput')?.value || '';
      if (!st || st.annotationsCount < 3 || !chatVal.includes('[Pin #')) return null;
      return { count: st.annotationsCount, chatVal, types: st.pageAnnotations.map((a) => a.type) };
    }, { desc: '3 real mouse annotations (circle, arrow, pin) + [Pin # token' });
    assert(annotState.count >= 3, `Expected >= 3 annotations, got ${annotState.count}`);
    assert(annotState.types.includes('circle') && annotState.types.includes('arrow') && annotState.types.includes('pin'));
    assert(annotState.chatVal.includes('[Pin #'), `#psChatInput must include [Pin #, got ${annotState.chatVal}`);
    await page.snap('02_pdf_studio_single_pdf_circle.png');

    // Switch to #psTabBtnChat and ask live /api/ask question
    await page.click('#psTabBtnChat');
    const curChatVal = await page.eval(() => document.getElementById('psChatInput')?.value || '');
    await page.fill('#psChatInput', `${curChatVal}What is the electrical panel risk and monthly ownership cost?`);
    await page.click('#psChatSendBtn');
    const chatReply = await page.waitFor(() => {
      const userMsgs = Array.from(document.querySelectorAll('#psChatLog .ps-msg.u'));
      const msgs = Array.from(document.querySelectorAll('#psChatLog .ps-msg.a'));
      if (!userMsgs.length || !msgs.length) return null;
      const last = msgs[msgs.length - 1].innerText.trim();
      if (!last || last.includes('Checking records') || last.includes('Ask questions grounded')) return null;
      return last;
    }, { desc: 'PDF Studio live /api/ask reply', timeout: 20000 });
    assert(
      !chatReply.includes('Unable to complete AI chat request') && chatReply.length >= 20,
      `Expected grounded /api/ask reply, got: ${chatReply}`
    );

    page.assertZeroErrorsAndNoOverflow('Flow 1');
    console.log('✓ Flow 1 passed (/assistant + UI email verify + single PDF + self-hosted PDF.js + annotations + /api/ask)');
  } finally {
    await page.close();
  }
}

async function runFlow2ReportModalZipCompsShareOffer(cdp) {
  console.log('▶ Flow 2: /r/:slug Report Modal + Role Switch + .zip (fflate) + Comps + Verified Share + Offer Builder');
  const page = await createIsolatedPage(cdp);
  try {
    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}`);
    await page.waitFor(() => {
      const hdrBtn = document.getElementById('buyerCopilotHdrBtn');
      const fab = document.getElementById('openEstateFab');
      const sellerTab = document.getElementById('roleTabHdr_seller');
      return !!(hdrBtn && fab && sellerTab && fab.innerText.length > 0);
    }, { desc: '/r/:slug report page loaded with Assistant header & FAB' });
    await page.assertNoHorizontalOverflow('Flow 2 /r/:slug initial');

    const hdrChecks = await page.eval(() => ({
      hdrBtnText: document.getElementById('buyerCopilotHdrBtn')?.textContent?.trim() || '',
      fabText: document.getElementById('openEstateFab')?.innerText?.trim() || '',
      hasAiAssistant: (document.documentElement?.innerHTML || '').includes('AI Assistant'),
    }));
    assert(hdrChecks.hdrBtnText.includes('Open Estate AI'), `#buyerCopilotHdrBtn must say Open Estate AI, got: ${hdrChecks.hdrBtnText}`);
    assert(hdrChecks.fabText.includes('Open Estate AI'), `#openEstateFab must contain Open Estate AI, got: ${hdrChecks.fabText}`);
    assert.equal(hdrChecks.hasAiAssistant, false, 'Zero "AI Assistant" allowed on /r/:slug');

    // Switch role to Seller via #roleTabHdr_seller
    await page.click('#roleTabHdr_seller');
    await page.waitFor(() => window.OpenEstateRole?.getRole?.() === 'seller', {
      desc: 'OpenEstateRole.getRole() === seller',
    });

    // Open #openEstateFab -> #openEstateDrawer.open shows Open Estate AI + Seller pill + #dbCopilotToolsBar
    await page.click('#openEstateFab');
    const drawerState = await page.waitFor(() => {
      const dr = document.getElementById('openEstateDrawer');
      if (!dr || !dr.classList.contains('open')) return null;
      const rolePill = document.getElementById('dbRolePill');
      const toolsBar = document.getElementById('dbCopilotToolsBar');
      const titleText = dr.querySelector('.db-dtitle')?.textContent?.trim() || '';
      if (!rolePill || !toolsBar) return null;
      return {
        titleText,
        rolePillText: rolePill.textContent.trim(),
        roleActive: rolePill.getAttribute('data-role-active') || '',
        toolsPresent: !!toolsBar,
      };
    }, { desc: '#openEstateDrawer.open with Seller pill & #dbCopilotToolsBar' });
    assert(drawerState.titleText.includes('Open Estate AI'), `Drawer title must contain Open Estate AI, got: ${drawerState.titleText}`);
    assert(
      drawerState.roleActive === 'seller' && drawerState.rolePillText.includes('Seller'),
      `Drawer must show Seller pill, got: ${JSON.stringify(drawerState)}`
    );

    // Click #buyerCopilotHdrBtn -> frosted light modal with Seller Mode
    await page.click('#buyerCopilotHdrBtn');
    const modalInfo = await page.waitFor(() => {
      const bd = document.getElementById('bcReportModalBackdrop');
      if (!bd || !bd.classList.contains('on')) return null;
      const title = document.getElementById('bcModalTitle')?.textContent?.trim() || '';
      const bg = getComputedStyle(bd).backgroundColor || '';
      if (!document.querySelector('#bcReportModalBackdrop [data-fn-pick="comps_pricing"]')) return null;
      return { title, bg };
    }, { desc: '#bcReportModalBackdrop.on in Seller Mode' });
    assert(
      modalInfo.bg.startsWith('rgba(248, 250, 252,') || modalInfo.bg.startsWith('rgb(248, 250, 252)'),
      `Modal backdrop must be frosted light wash rgba(248, 250, 252, ...), got: ${modalInfo.bg}`
    );
    assert(
      modalInfo.title.includes('Open Estate AI') && modalInfo.title.includes('Seller Mode'),
      `#bcModalTitle must contain Open Estate AI & Seller Mode, got: ${modalInfo.title}`
    );
    await page.snap('04_report_copilot_drawer_and_modal.png');

    // Switch back to Buyer role via [data-bc-set-role="buyer"]
    await page.click('#bcReportModalBackdrop [data-bc-set-role="buyer"]');
    await page.waitFor(() => {
      const title = document.getElementById('bcModalTitle')?.textContent || '';
      return title.includes('Buyer Mode') && window.OpenEstateRole?.getRole?.() === 'buyer';
    }, { desc: 'Switch back to Buyer Mode inside modal' });

    // Function 1: Comps & Pricing (strict -> expanded -> reverse non-linear follow-ups)
    await page.click('#bcReportModalBackdrop [data-fn-pick="comps_pricing"]');
    const strictRows = await page.waitFor(() => {
      const p = document.getElementById('bcPanelFn1');
      if (!p) return null;
      const rows = p.querySelectorAll('.bc-table tbody tr').length;
      return rows >= 2 && p.innerText.includes('Set ID:') ? rows : null;
    }, { desc: 'Strict comps table rendered' });
    assert.equal(strictRows, 2, `Expected 2 strict comps, got ${strictRows}`);

    await page.click('#bcExpandCompsBtn');
    const expandedInfo = await page.waitFor(() => {
      const p = document.getElementById('bcPanelFn1');
      if (!p) return null;
      const rows = p.querySelectorAll('.bc-table tbody tr').length;
      if (rows < 3 || !p.innerText.includes('410 Synthridge Way')) return null;
      const m = p.innerText.match(/Set ID:\s*([^\s\n]+)/);
      return m ? { rows, compSetId: m[1] } : null;
    }, { desc: 'Expanded 6 months / 2 miles comps table (includes 3rd comp)' });
    assert(expandedInfo.rows >= 3, `Expanded comps must include at least 3 comps, got ${expandedInfo.rows}`);
    const pinnedId = expandedInfo.compSetId;

    // Reverse non-linear order: Sold / Pending -> Similar Active -> Price Estimate
    await page.click('[data-followup="sold_or_pending"]');
    const spId = await page.waitFor(() => {
      const p = document.getElementById('bcPanelFn1');
      if (!p || !p.innerText.includes('Pending / Under Contract & Recently Sold')) return null;
      const m = p.innerText.match(/Set ID:\s*([^\s\n]+)/);
      return m ? m[1] : null;
    }, { desc: 'Sold / Pending follow-up' });
    assert.equal(spId, pinnedId, 'compSetId must remain identical after sold_or_pending');

    await page.click('[data-followup="similar_active"]');
    const simId = await page.waitFor(() => {
      const p = document.getElementById('bcPanelFn1');
      if (!p || !p.innerText.includes('Similar Properties Actively For Sale')) return null;
      const m = p.innerText.match(/Set ID:\s*([^\s\n]+)/);
      return m ? m[1] : null;
    }, { desc: 'Similar Active follow-up' });
    assert.equal(simId, pinnedId, 'compSetId must remain identical after similar_active');

    await page.click('[data-followup="estimate"]');
    const estCheck = await page.waitFor(() => {
      const p = document.getElementById('bcPanelFn1');
      if (!p || !p.innerText.includes('Comp-Based Price Estimate')) return null;
      const m = p.innerText.match(/Set ID:\s*([^\s\n]+)/);
      const txt = p.innerText;
      const rangeMatch = txt.match(/\$[\d,]+\s*[–-]\s*\$[\d,]+/);
      const hasConf = /Confidence:\s*(High|Medium|Low)/i.test(txt);
      if (!m || !rangeMatch || !hasConf) return null;
      return { compSetId: m[1], rangeText: rangeMatch[0], txt };
    }, { desc: 'Price Estimate range + Confidence' });
    assert.equal(estCheck.compSetId, pinnedId, 'compSetId must remain identical after estimate');

    // Function 2: Disclosures -> Register & 1-Click Verify Dave -> Upload Disclosure_Packet_4Docs.zip
    await page.click('#bcReportModalBackdrop [data-fn-pick="disclosures"]');
    await page.waitFor(() => !!document.querySelector('#bcReportModalBackdrop #bcDiscFileInput'), {
      desc: 'Function 2 Disclosures upload dropzone in modal (OSS)',
    });

    await page.setFiles('#bcReportModalBackdrop #bcDiscFileInput', [ZIP_PACKET_PATH]);
    const zipResult = await page.waitFor(() => {
      const badge = document.querySelector('#bcReportModalBackdrop #bcOneTimeCompletedBadge');
      const panel = document.querySelector('#bcReportModalBackdrop #bcPanelFn2');
      const studioBtn = document.querySelector('#bcReportModalBackdrop #bcOpenPdfStudioBtn');
      if (!badge || !panel || !studioBtn) return null;
      if (!studioBtn.innerText.includes('4 files')) return null;
      const cards = Array.from(panel.querySelectorAll('.bc-prio-card')).map((el) => ({
        text: el.innerText,
        missing: el.classList.contains('missing'),
      }));
      return {
        studioBtnText: studioBtn.innerText,
        panelText: panel.innerText,
        hasRegBar: !!document.querySelector('#bcReportModalBackdrop #bcRegUserBar'),
        hasShareForm: !!document.querySelector('#bcReportModalBackdrop #bcShareDealTeamForm'),
        hasAuthSection: !!document.querySelector('#bcReportModalBackdrop #bcAuthorizedSection'),
        cards,
      };
    }, { desc: '4-doc .zip upload + analysis + 4 files badge', timeout: 25000 });

    assert.equal(zipResult.hasRegBar, false, 'OSS modal must not show #bcRegUserBar');
    assert.equal(zipResult.hasShareForm, false, 'OSS modal must not show #bcShareDealTeamForm');
    assert.equal(zipResult.hasAuthSection, false, 'OSS modal must not show #bcAuthorizedSection');

    assert.equal(zipResult.cards.length, 6);
    assert.equal(zipResult.cards[0].missing, false, '#1 Home Inspection present');
    assert.equal(zipResult.cards[1].missing, true, '#2 Termite missing');
    assert.equal(zipResult.cards[2].missing, false, '#3 Preliminary Title present');
    assert.equal(zipResult.cards[3].missing, false, '#4 NHD present');
    assert.equal(zipResult.cards[4].missing, true, '#5 HOA missing');
    assert.equal(zipResult.cards[5].missing, false, '#6 Permits present');
    assert(zipResult.studioBtnText.includes('4 files'), `#bcOpenPdfStudioBtn must indicate 4 files, got: ${zipResult.studioBtnText}`);

    // Open PDF Studio from #bcOpenPdfStudioBtn -> verify all 4 unpacked files + fflateActive === true
    await page.click('#bcReportModalBackdrop #bcOpenPdfStudioBtn');
    const zipStudio = await page.waitFor(() => {
      const bd = document.getElementById('oePdfStudioBackdrop');
      if (!bd || !bd.classList.contains('on')) return null;
      const st = window.OpenEstatePdfStudio?.getState?.();
      const sel = document.getElementById('psFileSelect');
      const opts = sel ? Array.from(sel.options).map((o) => o.textContent.trim()) : [];
      if (!st || opts.length !== 4 || !st.pdfJsActive) return null;
      const pdfCanvas = document.getElementById('psPdfCanvas');
      if (!pdfCanvas || pdfCanvas.width < 100) return null;
      return { opts, pdfJsActive: !!st.pdfJsActive, fflateActive: !!st.fflateActive };
    }, { desc: 'PDF Studio opened with 4 unpacked .zip PDFs via self-hosted fflate' });
    assert.equal(zipStudio.opts.length, 4);
    assert.equal(zipStudio.pdfJsActive, true, 'Self-hosted PDF.js must be active in Flow 2');
    assert.equal(zipStudio.fflateActive, true, 'Self-hosted /vendor/fflate.min.js must be active in Flow 2');
    for (const expectedName of [
      '01_Home_Inspection_Report.pdf',
      '02_Preliminary_Title_Report.pdf',
      '03_Natural_Hazard_Disclosure_NHD.pdf',
      '04_Building_Permits.pdf',
    ]) {
      assert(
        zipStudio.opts.some((o) => o.includes(expectedName)),
        `#psFileSelect missing ${expectedName}, got: ${JSON.stringify(zipStudio.opts)}`
      );
    }

    // Switch to index 1 (02_Preliminary_Title_Report.pdf) -> verify crisp spacing & re-render -> draw circle
    await page.selectOption('#psFileSelect', '1');
    const prelimRendered = await page.waitFor(() => {
      const st = window.OpenEstatePdfStudio?.getState?.();
      if (!st || st.docIndex !== 1 || st.currentDocName !== '02_Preliminary_Title_Report.pdf') return null;
      const pdfCanvas = document.getElementById('psPdfCanvas');
      if (!pdfCanvas || pdfCanvas.width < 100) return null;
      const data = pdfCanvas.getContext('2d').getImageData(0, 0, pdfCanvas.width, pdfCanvas.height).data;
      let dark = 0;
      for (let i = 0; i < data.length; i += 4) {
        if (data[i + 3] > 0 && (data[i] < 200 || data[i + 1] < 200 || data[i + 2] < 200)) {
          dark++;
          if (dark > 100) break;
        }
      }
      if (dark <= 100) return null;
      const tlText = document.getElementById('psTextLayer')?.textContent || '';
      if (!tlText.includes('PRELIMINARY REPORT')) return null;
      return { dark, tlText };
    }, { desc: '02_Preliminary_Title_Report.pdf canvas + TextLayer render' });
    assert(prelimRendered.dark > 100, 'Prelim PDF must render non-white dark ink pixels');
    assert(
      !prelimRendered.tlText.includes('COMMITMENTProperty'),
      `Prelim PDF TextLayer must have clean word spacing (no COMMITMENTProperty), got: ${prelimRendered.tlText}`
    );

    await page.click('[data-ps-tool="circle"]');
    await page.waitFor(() => window.OpenEstatePdfStudio?.getState?.()?.tool === 'circle', {
      desc: 'ZIP Studio tool === circle',
    });
    await page.dragOnElement('#psAnnotCanvas', [0.18, 0.24], [0.55, 0.38], 8);
    await page.waitFor(() => (window.OpenEstatePdfStudio?.getState?.()?.annotationsCount || 0) >= 1, {
      desc: 'Circle annotation on 02_Preliminary_Title_Report.pdf',
    });
    await page.snap('03_zip_packet_pdf_studio_4docs.png');

    await page.click('#psCloseBtn');
    await page.waitFor(() => !document.getElementById('oePdfStudioBackdrop')?.classList?.contains('on'), {
      desc: 'PDF Studio closed',
    });

    // Function 3: Offer Builder
    await page.click('#bcReportModalBackdrop [data-fn-pick="offer_builder"]');
    await page.waitFor(() => !!document.querySelector('#bcOfferStepForm input[name="offerPriceUsd"]'), {
      desc: 'Offer Builder Step 1',
    });
    await page.fill('#bcOfferStepForm input[name="offerPriceUsd"]', '1600000');
    await page.click('#bcOfferStepForm button[type="submit"]');
    await page.waitFor(() => {
      const sum = document.querySelector('#bcPanelFn3 .bc-summary-box');
      return sum && sum.innerText.includes('$1,600,000');
    }, { desc: 'Offer Price $1,600,000 reflected in Running Summary' });

    // Jump to Step 5: verify prefilled 2.5% commission, update to 2.0%
    await page.click('#bcPanelFn3 [data-offer-step="5"]');
    const prefilledComm = await page.waitFor(() => {
      const inp = document.querySelector('#bcOfferStepForm input[name="buyerAgentCommissionPct"]');
      return inp ? inp.value : null;
    }, { desc: 'Offer Builder Step 5 commission input' });
    assert.equal(prefilledComm, '2.5', `Expected buyerAgentCommissionPct prefilled 2.5, got ${prefilledComm}`);

    await page.fill('#bcOfferStepForm input[name="buyerAgentCommissionPct"]', '2.0');
    await page.click('#bcOfferStepForm button[type="submit"]');
    await page.waitFor(() => {
      const sum = document.querySelector('#bcPanelFn3 .bc-summary-box');
      return sum && sum.innerText.includes('2%') && sum.innerText.includes('$32,000');
    }, { desc: '2% ($32,000) commission updated in Running Summary' });

    // Step 6: fill Seller Repairs & Seller Credit and submit
    await page.waitFor(() => !!document.querySelector('#bcOfferStepForm textarea[name="sellerRepairs"]'), {
      desc: 'Offer Builder Step 6 sellerRepairs textarea',
    });
    await page.fill(
      '#bcOfferStepForm textarea[name="sellerRepairs"]',
      'Replace Federal Pacific Stab-Lok subpanel and clear expired permit #B-2019-4410 prior to COE'
    );
    await page.fill('#bcOfferStepForm input[name="sellerCreditUsd"]', '12000');
    await page.click('#bcOfferStepForm button[type="submit"]');

    const summaryText = await page.waitFor(() => {
      const sum = document.querySelector('#bcPanelFn3 .bc-summary-box');
      if (!sum) return null;
      const t = sum.innerText;
      if (!t.includes('$1,600,000') || !t.includes('2%') || !t.includes('$12,000') || !t.includes('Replace Federal Pacific')) {
        return null;
      }
      return t;
    }, { desc: 'Final Running Summary updated with offer price, commission, credit, and repairs' });

    assert(
      /not a signed/i.test(summaryText),
      `Final Offer Summary must explicitly state it is NOT a signed contract, got:\n${summaryText}`
    );
    const legalNotices = await page.eval(() => ({
      nar: document.querySelector('#bcPanelFn3 #bcPostNarNotice')?.innerText?.trim() || '',
      tax: document.querySelector('#bcPanelFn3 #bcTransferTaxDisclaimer')?.innerText?.trim() || '',
    }));
    assert(
      legalNotices.nar.length > 20 || /buyer-broker|representation|nar/i.test(summaryText),
      `Offer Builder must render post-NAR buyer-broker notice, got: ${JSON.stringify(legalNotices)}`
    );
    await page.snap('05_offer_builder_running_summary.png');

    await page.assertNoHorizontalOverflow('Flow 2 /r/:slug final');
    page.assertZeroErrorsAndNoOverflow('Flow 2');
    console.log('✓ Flow 2 passed (/r/:slug + Role Switch + .zip fflate + Comps + Verified Mode 2 Share + Offer Builder)');
  } finally {
    await page.close();
  }
}

async function runFlow3ScannedImageOnlyPdfAndAlerts(cdp) {
  console.log('▶ Flow 3: /alerts Branding + Scanned Image-Only PDF (/DCTDecode JPEG, 0 text ops) in PDF Studio');
  const page = await createIsolatedPage(cdp);
  try {
    // 3a. Verify /alerts Assistant branding & SMTP-disabled banner (#82 item 3)
    await page.goto(`${BASE_URL}/alerts`);
    const alertsCheck = await page.waitFor(() => {
      const nav = document.getElementById('navAssistant');
      const smtp = document.getElementById('alertsSmtpDisabledBanner');
      if (!nav || !smtp) return null;
      const html = document.documentElement?.innerHTML || '';
      return {
        navText: nav.textContent.trim(),
        smtpText: smtp.innerText.trim(),
        hasBuyerAssistant: html.includes('Buyer Assistant'),
        hasAiAssistant: html.includes('AI Assistant'),
        hasPropIntelAssistant: html.includes('Property Intelligence Assistant'),
      };
    }, { desc: '/alerts Open Estate AI nav & SMTP banner' });
    assert(alertsCheck.navText === 'Open Estate AI', `/alerts #navAssistant must say 'Open Estate AI', got: ${alertsCheck.navText}`);
    assert.equal(alertsCheck.hasBuyerAssistant, false, 'Zero Buyer Assistant on /alerts');
    assert.equal(alertsCheck.hasAiAssistant, false, 'Zero AI Assistant on /alerts');
    assert.equal(alertsCheck.hasPropIntelAssistant, false, 'Zero Property Intelligence Assistant on /alerts');
    await page.assertNoHorizontalOverflow('Flow 3 /alerts');

    // 3b. Navigate to /r/:slug, open Open Estate — PDF Studio via modal Disclosures -> #bcOpenPdfStudioBtn,
    //     verify client-side .zip traversal guard, then load Scanned_Seller_Disclosure.pdf (/DCTDecode JPEG, 0 text ops)
    await page.goto(`${BASE_URL}/r/${PRIMARY_SLUG}`);
    await page.click('#buyerCopilotHdrBtn');
    await page.click('#bcReportModalBackdrop [data-fn-pick="disclosures"]');
    await page.click('#bcReportModalBackdrop #bcOpenPdfStudioBtn');
    await page.waitFor(() => {
      const bd = document.getElementById('oePdfStudioBackdrop');
      return !!(bd && bd.classList.contains('on'));
    }, { desc: 'PDF Studio opened via #bcOpenPdfStudioBtn' });

    // Client-side path-traversal .zip rejection inside PDF Studio (#psErrorBanner)
    await page.setFiles('#psAddFilesInput', [UNSAFE_ZIP_PATH]);
    const zipSafetyUi = await page.waitFor(() => {
      const st = window.OpenEstatePdfStudio?.getState?.();
      const errBanner = document.getElementById('psErrorBanner');
      const errText = document.getElementById('psErrorBannerText')?.textContent?.trim() || '';
      if (!st || !st.lastErrorCode || !errBanner || errBanner.style.display === 'none' || !errText) return null;
      return { code: st.lastErrorCode, text: errText, docCount: st.docs.length };
    }, { desc: 'Client-side UNSAFE_ARCHIVE_ENTRY error banner in PDF Studio' });
    assert.equal(zipSafetyUi.code, 'UNSAFE_ARCHIVE_ENTRY', `Expected UNSAFE_ARCHIVE_ENTRY, got ${zipSafetyUi.code}`);
    assert.equal(zipSafetyUi.docCount, 0, 'Rejected archive must ingest 0 documents');

    // Load 3rd example: Scanned_Seller_Disclosure.pdf (embedded /DCTDecode JPEG with 0 text operators)
    await page.setFiles('#psAddFilesInput', [SCANNED_PDF_PATH]);
    const scannedStudio = await page.waitFor(() => {
      const st = window.OpenEstatePdfStudio?.getState?.();
      if (!st || st.currentDocName !== 'Scanned_Seller_Disclosure.pdf') return null;
      const pdfCanvas = document.getElementById('psPdfCanvas');
      if (!pdfCanvas || pdfCanvas.width < 200 || pdfCanvas.height < 250) return null;
      const data = pdfCanvas.getContext('2d').getImageData(0, 0, pdfCanvas.width, pdfCanvas.height).data;
      let darkPixels = 0;
      let redAccentPixels = 0;
      for (let i = 0; i < data.length; i += 4) {
        const r = data[i], g = data[i + 1], b = data[i + 2], a = data[i + 3];
        if (a > 0 && (r < 190 || g < 190 || b < 190)) darkPixels++;
        if (a > 0 && r > 170 && g < 80 && b < 80) redAccentPixels++;
      }
      if (darkPixels < 1500) return null;
      const scannedBanner = document.getElementById('psScannedBanner');
      const scannedBannerText = document.getElementById('psScannedBannerText')?.textContent?.trim() || '';
      const scannedVisible = !!(scannedBanner && getComputedStyle(scannedBanner).display !== 'none');
      if (!scannedVisible) return null;
      const tlSpanCount = document.getElementById('psTextLayer')?.querySelectorAll('span')?.length ?? 0;
      const errBannerHidden = getComputedStyle(document.getElementById('psErrorBanner')).display === 'none';
      return {
        title: document.getElementById('psStudioTitle')?.textContent?.trim() || '',
        pdfJsActive: !!st.pdfJsActive,
        isScannedPage: !!st.isScannedPage,
        numPages: st.numPages,
        darkPixels,
        redAccentPixels,
        scannedVisible,
        scannedBannerText,
        tlSpanCount,
        errBannerHidden,
      };
    }, { desc: 'Scanned_Seller_Disclosure.pdf raster render + #psScannedBanner' });

    assert(
      scannedStudio.title.includes('Open Estate AI') && scannedStudio.title.includes('PDF Studio'),
      `#psStudioTitle must contain Open Estate AI & PDF Studio: ${scannedStudio.title}`
    );
    assert.equal(scannedStudio.isScannedPage, true, 'Scanned image-only PDF must report isScannedPage === true');
    assert.equal(scannedStudio.tlSpanCount, 0, 'Scanned image-only PDF with 0 text ops must have 0 TextLayer spans');
    assert.equal(scannedStudio.scannedVisible, true, '#psScannedBanner must be visible on scanned image-only page');
    assert(/scan|raster|image|circle/i.test(scannedStudio.scannedBannerText), `Expected scanned banner explanation, got: ${scannedStudio.scannedBannerText}`);
    assert(scannedStudio.darkPixels >= 1500, `Expected rich scanned raster pixels, got ${scannedStudio.darkPixels}`);
    assert(scannedStudio.redAccentPixels >= 100, `Expected red callout pixels from embedded /DCTDecode JPEG, got ${scannedStudio.redAccentPixels}`);
    assert.equal(scannedStudio.errBannerHidden, true, '#psErrorBanner must clear once valid Scanned PDF is loaded');

    // Draw Circle around item #1 ("1. GARAGE SUBPANEL: FEDERAL PACIFIC STAB-LOK") and Arrow to diagram on scanned raster
    await page.eval(() => {
      const vp = document.getElementById('psViewport');
      if (vp) vp.scrollTop = 0;
    });
    await page.click('[data-ps-tool="circle"]');
    await page.waitFor(() => window.OpenEstatePdfStudio?.getState?.()?.tool === 'circle', {
      desc: 'Scanned PDF Studio tool === circle',
    });
    await page.dragOnElement('#psAnnotCanvas', [0.06, 0.18], [0.93, 0.29], 10);
    await page.waitFor(() => (window.OpenEstatePdfStudio?.getState?.()?.annotationsCount || 0) >= 1, {
      desc: 'Circle annotation persisted on Scanned PDF',
    });

    await page.click('[data-ps-tool="arrow"]');
    await page.waitFor(() => window.OpenEstatePdfStudio?.getState?.()?.tool === 'arrow', {
      desc: 'Scanned PDF Studio tool === arrow',
    });
    await page.dragOnElement('#psAnnotCanvas', [0.44, 0.78], [0.60, 0.78], 8);
    await page.waitFor(() => (window.OpenEstatePdfStudio?.getState?.()?.annotationsCount || 0) >= 2, {
      desc: 'Arrow annotation persisted on Scanned PDF',
    });

    await page.eval(() => {
      const vp = document.getElementById('psViewport');
      if (vp) vp.scrollTop = 0;
    });
    await page.snap('06_scanned_pdf_studio.png');
    await page.assertNoHorizontalOverflow('Flow 3 Scanned PDF Studio');
    page.assertZeroErrorsAndNoOverflow('Flow 3');
    console.log('✓ Flow 3 passed (/alerts Assistant + client zip guard + Scanned /DCTDecode PDF Studio + 06_scanned_pdf_studio.png)');
  } finally {
    await page.close();
  }
}

async function main() {
  const { proc, ws, cdp } = await launchChromiumCdp();
  try {
    await runFlow1AssistantAndSinglePdf(cdp);
    await runFlow2ReportModalZipCompsShareOffer(cdp);
    await runFlow3ScannedImageOnlyPdfAndAlerts(cdp);
    console.log('\nALL OPEN ESTATE COPILOT BROWSER E2E CHECKS PASSED');
  } finally {
    try { await cdp.send('Browser.close'); } catch (_) {}
    try { ws.close(); } catch (_) {}
    try { proc.kill('SIGKILL'); } catch (_) {}
  }
}

main().catch((err) => {
  console.error('\nOPEN ESTATE COPILOT BROWSER E2E FAILED:', err);
  process.exit(1);
});
"###;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_estate_copilot_chromium_browser_e2e() {
    let chromium_exec = discover_chromium_executable();
    let node_exec = discover_node_executable();

    let mut primary_rep = common::synth_report_primary();
    normalize_stored_report(&mut primary_rep);
    primary_rep["facts"]["asOf"] = json!("2026-11-15");
    primary_rep["facts"]["forSale"] = json!(true);
    primary_rep["facts"]["listingStatus"] = json!("Active");
    primary_rep["facts"]["listPrice"] = json!(1_620_000);
    primary_rep["facts"]["sqft"] = json!(2000);
    primary_rep["facts"]["yearBuilt"] = json!(1988);
    primary_rep["facts"]["propertyType"] = json!("Single Family");
    primary_rep["facts"]["apn"] = json!("421-18-022");
    primary_rep["facts"]["mlsListing"] = json!({
        "status": "Active",
        "listPrice": 1_620_000,
        "mlsNumber": "ML81999999",
        "agentEmail": "listing.agent@example.com",
        "agentName": "Jane Listing",
        "officeName": "Silicon Valley Realty"
    });
    primary_rep["facts"]["comps"] = json!([
        {
            "address": "104 Synthparcelalpha Ln, San Jose, CA 95118",
            "soldPrice": 1_600_000,
            "soldDate": "2026-10-12",
            "sqft": 1980,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1990,
            "propertyType": "Single Family",
            "distanceMi": 0.28
        },
        {
            "address": "118 Synthparcelalpha Ln, San Jose, CA 95118",
            "soldPrice": 1_660_000,
            "soldDate": "2026-09-25",
            "sqft": 2060,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1986,
            "propertyType": "Single Family",
            "distanceMi": 0.55
        },
        {
            "address": "410 Synthridge Way, San Jose, CA 95118",
            "soldPrice": 1_710_000,
            "soldDate": "2026-06-20",
            "sqft": 2240,
            "beds": 4,
            "baths": 3.0,
            "yearBuilt": 1979,
            "propertyType": "Single Family",
            "distanceMi": 1.62
        }
    ]);
    primary_rep["facts"]["nearbyListings"] = json!([
        {
            "address": "320 Synthmeadow Dr, San Jose, CA 95118",
            "price": 1_645_000,
            "sqft": 2120,
            "beds": 4,
            "baths": 2.5,
            "yearBuilt": 1991,
            "propertyType": "Single Family",
            "distanceMi": 1.38
        },
        {
            "address": "990 Synthmansion Ct, San Jose, CA 95118",
            "price": 3_450_000,
            "sqft": 4300,
            "beds": 6,
            "baths": 5.0,
            "yearBuilt": 2016,
            "propertyType": "Single Family",
            "distanceMi": 1.15
        }
    ]);
    primary_rep["facts"]["nearbyPending"] = json!([
        {
            "address": "142 Synthgrove St, San Jose, CA 95118",
            "price": 1_635_000,
            "sqft": 1990,
            "beds": 4,
            "baths": 2.0,
            "yearBuilt": 1987,
            "propertyType": "Single Family",
            "distanceMi": 0.74
        }
    ]);

    let mut secondary_rep = common::synth_report_secondary();
    normalize_stored_report(&mut secondary_rep);
    secondary_rep["facts"]["forSale"] = json!(false);
    secondary_rep["facts"]["listingStatus"] = json!("Off Market");
    secondary_rep["facts"]["listPrice"] = Value::Null;

    let srv = E2eServerHarness::start(primary_rep, secondary_rep).await;

    // Verify hosted sharing/account routes remain disabled (404) in Open Estate standalone OSS
    let http_client = Client::builder().no_proxy().build().unwrap();
    for (route, payload) in [
        (
            "/api/copilot/auth/register",
            json!({ "email": "bob.agent@example.com", "displayName": "Bob Agent", "role": "agent" }),
        ),
        (
            "/api/copilot/disclosures/share",
            json!({ "slug": common::SYNTH_PRIMARY_SLUG, "recipientEmail": "bob.agent@example.com", "collaboratorAttested": true }),
        ),
        (
            "/api/copilot/disclosures/authorized",
            json!({ "slug": common::SYNTH_PRIMARY_SLUG, "uploaderRole": "seller", "signerName": "Owner", "authorizedToDistribute": true, "documents": [] }),
        ),
    ] {
        let resp = http_client
            .post(format!("{}{}", srv.base_url, route))
            .header("Origin", &srv.base_url)
            .json(&payload)
            .send()
            .await
            .expect("POST OSS disabled sharing route");
        assert_eq!(resp.status(), 404, "Open Estate OSS must return 404 for {route}");
    }

    // Build real binary sample files on disk in temp_dir:
    // 1) Home_Inspection_Single.pdf (2-page /FlateDecode digital PDF)
    let single_pdf_bytes = build_flate_pdf_bytes(&[
        &[
            "HOME INSPECTION REPORT",
            "Property: 100 Synthparcelalpha Ln, San Jose, CA 95118",
            "Client Contact: seller.private@example.com Phone (408) 555-0199 Tax ID 123-45-6789",
            "Electrical Panel: Federal Pacific Electric Stab-Lok subpanel fire hazard observed in garage.",
            "Roofing: Active roof leak around chimney flashing with moisture staining.",
        ],
        &[
            "HOME INSPECTION REPORT - PAGE 2",
            "Plumbing & Foundation: Copper supply lines intact; minor hairline garage slab crack noted.",
            "Recommendation: Obtain licensed electrical evaluation prior to close of escrow.",
        ],
    ]);
    let single_pdf_path = srv.temp_dir.join("Home_Inspection_Single.pdf");
    std::fs::write(&single_pdf_path, &single_pdf_bytes).expect("write Home_Inspection_Single.pdf");

    // 2) Disclosure_Packet_4Docs.zip (4 PDFs omitting #2 Termite and #5 HOA)
    let zip_home_insp = build_flate_pdf_bytes(&[&[
        "HOME INSPECTION REPORT",
        "Property: 100 Synthparcelalpha Ln, San Jose, CA 95118",
        "Owner Reference: seller.private@example.com Phone (408) 555-0199 Tax ID 123-45-6789",
        "Electrical Panel: Federal Pacific Electric Stab-Lok subpanel fire hazard observed (Loan #LN-99887766).",
        "Roofing: Active roof leak around chimney flashing observed.",
    ]]);
    let zip_prelim = build_flate_pdf_bytes(&[&[
        "PRELIMINARY REPORT - ALTA COMMITMENT",
        "Property Address: 100 Synthparcelalpha Ln, San Jose, CA 95118",
        "Schedule B Exceptions:",
        "Item 8. Delinquent Property Tax Lien $6,400 recorded against parcel (Escrow Account #ESC-44332211).",
    ]]);
    let zip_nhd = build_flate_pdf_bytes(&[&[
        "CALIFORNIA NATURAL HAZARD DISCLOSURE STATEMENT (NHD)",
        "Subject Property IS LOCATED IN a Liquefaction Zone mapped by CGS.",
        "Subject Property IS NOT located in a Special Flood Hazard Area.",
    ]]);
    let zip_permits = build_flate_pdf_bytes(&[&[
        "CITY BUILDING PERMIT HISTORY & RECORDS",
        "Kitchen remodel: EXPIRED permit #B-2019-4410 without final sign-off.",
    ]]);
    let zip_bytes = build_pkzip_bytes(&[
        ("01_Home_Inspection_Report.pdf", &zip_home_insp),
        ("02_Preliminary_Title_Report.pdf", &zip_prelim),
        ("03_Natural_Hazard_Disclosure_NHD.pdf", &zip_nhd),
        ("04_Building_Permits.pdf", &zip_permits),
    ]);
    let zip_packet_path = srv.temp_dir.join("Disclosure_Packet_4Docs.zip");
    std::fs::write(&zip_packet_path, &zip_bytes).expect("write Disclosure_Packet_4Docs.zip");

    // 3) Scanned_Seller_Disclosure.pdf (1-page /DCTDecode JPEG image-only PDF with 0 text operators)
    let scanned_pdf_bytes = build_scanned_jpeg_pdf_bytes();
    let extracted_imgs =
        extract_embedded_images_from_pdf_page(&scanned_pdf_bytes, 1).expect("extract scanned jpeg");
    assert_eq!(extracted_imgs.len(), 1);
    assert_eq!(extracted_imgs[0].format, PdfImageFormat::Jpeg);
    assert_eq!(extracted_imgs[0].width, 616);
    assert_eq!(extracted_imgs[0].height, 792);
    let parsed_scanned_doc = Document::load_mem(&scanned_pdf_bytes).expect("load scanned PDF");
    let (&p1_num, &p1_id) = parsed_scanned_doc.get_pages().iter().next().unwrap();
    assert_eq!(p1_num, 1);
    let p1_raw = parsed_scanned_doc.get_page_content(p1_id);
    let p1_content = Content::decode(&p1_raw).unwrap();
    assert!(
        p1_content
            .operations
            .iter()
            .all(|op| !matches!(op.operator.as_str(), "BT" | "ET" | "Tj" | "TJ" | "'" | "\"")),
        "Scanned_Seller_Disclosure.pdf must contain zero PDF text operators"
    );
    let scanned_pdf_path = srv.temp_dir.join("Scanned_Seller_Disclosure.pdf");
    std::fs::write(&scanned_pdf_path, &scanned_pdf_bytes)
        .expect("write Scanned_Seller_Disclosure.pdf");

    // 4) Unsafe_Traversal_Packet.zip (client-side path-traversal guard fixture)
    let unsafe_zip_bytes = build_pkzip_bytes(&[("../../traversal_escape.pdf", &zip_permits)]);
    let unsafe_zip_path = srv.temp_dir.join("Unsafe_Traversal_Packet.zip");
    std::fs::write(&unsafe_zip_path, &unsafe_zip_bytes)
        .expect("write Unsafe_Traversal_Packet.zip");

    let script_path = srv.temp_dir.join("cdp_e2e.mjs");
    std::fs::write(&script_path, CDP_E2E_MJS).expect("write cdp_e2e.mjs");

    let user_data_dir = srv.temp_dir.join("chrome_profile");
    std::fs::create_dir_all(&user_data_dir).expect("create chrome profile dir");

    let base_url_clone = srv.base_url.clone();
    let screenshot_dir = std::env::var("OPEN_ESTATE_E2E_SCREENSHOT_DIR").unwrap_or_default();

    let output = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&node_exec);
        cmd.arg(&script_path)
            .env("CHROMIUM_EXEC", &chromium_exec)
            .env("BASE_URL", &base_url_clone)
            .env("PRIMARY_SLUG", common::SYNTH_PRIMARY_SLUG)
            .env("SECONDARY_SLUG", common::SYNTH_SECONDARY_SLUG)
            .env("SINGLE_PDF_PATH", &single_pdf_path)
            .env("ZIP_PACKET_PATH", &zip_packet_path)
            .env("SCANNED_PDF_PATH", &scanned_pdf_path)
            .env("UNSAFE_ZIP_PATH", &unsafe_zip_path)
            .env("USER_DATA_DIR", &user_data_dir);
        if !screenshot_dir.trim().is_empty() {
            cmd.env("OPEN_ESTATE_E2E_SCREENSHOT_DIR", screenshot_dir.trim());
        }
        cmd.output().expect("spawn node CDP E2E runner")
    })
    .await
    .expect("join CDP E2E blocking task");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    if !stderr.trim().is_empty() {
        eprintln!("{stderr}");
    }

    assert!(
        output.status.success(),
        "Headless Chromium CDP E2E failed (exit {:?})\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("ALL OPEN ESTATE COPILOT BROWSER E2E CHECKS PASSED"),
        "Expected completion banner in stdout, got:\n{stdout}"
    );

    // Cross-check OSS sharing removal + DB persistence from browser actions:
    // ?recipientView=true must return 404 FEATURE_DISABLED in Open Estate OSS
    let recipient_view_resp = http_client
        .get(format!(
            "{}/api/copilot/disclosures?slug={}&recipientView=true",
            srv.base_url,
            common::SYNTH_PRIMARY_SLUG
        ))
        .send()
        .await
        .expect("oss recipientView disclosures check");
    assert_eq!(recipient_view_resp.status(), 404);

    let logs = srv
        .db
        .list_admin_audit_logs(30)
        .await
        .expect("list admin audit logs");
    assert!(
        logs.iter()
            .any(|l| l.action == "disclosure_findings_logged"),
        "Browser disclosure analysis must persist audit log"
    );
}
