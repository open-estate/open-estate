//! Zero-Mock Policy Gate for `open-estate` (`public/`).
//!
//! Enforces Implementation Plan §7 across `public/Cargo.toml`, `public/src/`, and `public/tests/`:
//! - No mock HTTP crates (`wiremock`, `mockito`, `httpmock`, `mockall`).
//! - No new `Mock*`, `Fake*`, `Stub*`, or `TestDouble*` code identifiers.
//! - No canned/recorded upstream payload helpers or fake upstream `axum::serve` servers.
//! - Real in-process servers (`build_router(state)`), real unreachable closed-port listeners,
//!   real invalid credentials, and deterministic logic tests on real inputs are allowed.
//! - Zero legacy exceptions are permitted in any `tests/live_*.rs` or `tests/live_support/*.rs` file.

use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name_str.as_ref(),
                ".git" | "target" | ".cache" | "node_modules"
            ) {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Strip `//` line comments and `/* ... */` block comments (respecting string literals)
/// so policy documentation comments such as `"zero MockSmtpServer"` never trigger false positives.
fn strip_rust_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    let mut in_char = false;
    let mut raw_hashes: Option<usize> = None;
    let mut block_depth = 0usize;

    while i < bytes.len() {
        if block_depth > 0 {
            if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                block_depth += 1;
                i += 2;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                block_depth -= 1;
                i += 2;
                continue;
            }
            if bytes[i] == b'\n' {
                out.push('\n');
            }
            i += 1;
            continue;
        }

        if let Some(hashes) = raw_hashes {
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            if ch == '"' {
                let mut matched = 0;
                while matched < hashes && i + matched < bytes.len() && bytes[i + matched] == b'#' {
                    matched += 1;
                }
                if matched == hashes {
                    for _ in 0..hashes {
                        out.push('#');
                    }
                    i += hashes;
                    raw_hashes = None;
                }
            }
            continue;
        }

        if in_str {
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            if ch == '\\' && i < bytes.len() {
                let esc = src[i..].chars().next().unwrap();
                out.push(esc);
                i += esc.len_utf8();
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }

        if in_char {
            let ch = src[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            if ch == '\\' && i < bytes.len() {
                let esc = src[i..].chars().next().unwrap();
                out.push(esc);
                i += esc.len_utf8();
            } else if ch == '\'' {
                in_char = false;
            }
            continue;
        }

        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            block_depth = 1;
            i += 2;
            continue;
        }

        if bytes[i] == b'r' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'#' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                let hashes = j - (i + 1);
                out.push_str(&src[i..=j]);
                i = j + 1;
                raw_hashes = Some(hashes);
                continue;
            }
        }

        if bytes[i] == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if bytes[i] == b'\'' {
            // Distinguish lifetime `'a` from char literal `'x'` / `'\n'`.
            let rem = &src[i + 1..];
            let is_char_lit =
                rem.starts_with('\\') || rem.chars().nth(1).map(|c| c == '\'').unwrap_or(false);
            if is_char_lit {
                in_char = true;
            }
            out.push('\'');
            i += 1;
            continue;
        }

        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

#[test]
fn zero_mock_policy_gate_enforced_across_public_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // 1. Cargo.toml must never depend on HTTP mock crates.
    let cargo_toml = fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let forbidden_crates = [
        concat!("wire", "mock"),
        concat!("mock", "ito"),
        concat!("http", "mock"),
        concat!("mock", "all"),
    ];
    for crate_name in forbidden_crates {
        let rx = Regex::new(&format!(r"(?m)^\s*{crate_name}\s*=")).unwrap();
        assert!(
            !rx.is_match(&cargo_toml),
            "Cargo.toml must not contain mock crate `{crate_name}`"
        );
    }

    // 2. Scan src/ and tests/ for forbidden mock/fake/stub code identifiers & mock HTTP servers.
    let mut files = Vec::new();
    collect_rs_files(&root.join("src"), &mut files);
    collect_rs_files(&root.join("tests"), &mut files);
    files.sort();

    let mock_ident_rx = Regex::new(&format!(
        r"\b(?:{}|{}|{}|{}Double)[A-Z0-9_][A-Za-z0-9_]*\b",
        "Mock", "Fake", "Stub", "Test"
    ))
    .unwrap();
    let mock_crate_use_rx = Regex::new(&format!(
        r"\b(?:{}|{}|{}|{})\b",
        concat!("wire", "mock"),
        concat!("mock", "ito"),
        concat!("http", "mock"),
        concat!("mock", "all"),
    ))
    .unwrap();
    let canned_payload_rx = Regex::new(&format!(
        r"(?i)\b(?:canned_{0}|recorded_upstream_{0}|synthetic_upstream_{0}|{1}_http_server)\b",
        "payload", "mock"
    ))
    .unwrap();

    // Documented legacy baseline (untouched files scheduled for future conversion; must NOT grow).
    let legacy_ident_ratchet: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::from([
        ("src/modules/commute.rs", BTreeSet::from(["MockState"])),
        (
            "tests/osrm_recovery_test.rs",
            BTreeSet::from(["MockOsrmState"]),
        ),
        (
            "tests/routing_extensions_load_balancer_test.rs",
            BTreeSet::from(["MockProviderServer"]),
        ),
    ]);

    // Legacy tests that spawn non-AppState axum::Router upstream servers (zero new files allowed).
    let legacy_mock_server_files: BTreeSet<&str> = BTreeSet::from([
        "src/modules/commute.rs",
        "tests/campbell_parity_test.rs",
        "tests/osrm_recovery_test.rs",
        "tests/property_chat_and_research_test.rs",
        "tests/routing_extensions_load_balancer_test.rs",
    ]);

    let mut violations = Vec::new();

    for path in &files {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str == "tests/zero_mock_policy_test.rs" {
            continue;
        }
        let Ok(raw) = fs::read_to_string(path) else {
            continue;
        };
        let code = strip_rust_comments(&raw);
        let is_live_test = rel_str.starts_with("tests/live_");

        for (idx, line) in code.lines().enumerate() {
            let lineno = idx + 1;

            if mock_crate_use_rx.is_match(line) {
                violations.push(format!(
                    "{rel_str}:{lineno}: forbidden mock crate reference"
                ));
            }
            if canned_payload_rx.is_match(line) {
                violations.push(format!(
                    "{rel_str}:{lineno}: forbidden canned/mock payload identifier"
                ));
            }

            for m in mock_ident_rx.find_iter(line) {
                let ident = m.as_str();
                let allowed_legacy = !is_live_test
                    && legacy_ident_ratchet
                        .get(rel_str.as_str())
                        .map(|s| s.contains(ident))
                        .unwrap_or(false);
                if !allowed_legacy {
                    violations.push(format!(
                        "{rel_str}:{lineno}: forbidden mock/fake/stub identifier `{ident}`"
                    ));
                }
            }
        }

        // Check for new local mock HTTP servers in tests/ or #[cfg(test)] blocks.
        let has_router_serve = code.contains("axum::serve") || code.contains("Router::new");
        if has_router_serve && rel_str.starts_with("tests/") {
            let uses_real_server = code.contains("build_router(")
                || code.contains("admin_router(")
                || code.contains("ops_router(");
            let is_non_app_server = !uses_real_server
                || code.contains("MockProviderServer")
                || code.contains("MockOsrmState")
                || code.contains("fred_app");
            if is_non_app_server
                && (is_live_test || !legacy_mock_server_files.contains(rel_str.as_str()))
            {
                violations.push(format!(
                    "{rel_str}: non-AppState axum mock server detected (use real upstream or real build_router(state))"
                ));
            }
        }
        if is_live_test && code.contains("Router::new") && !code.contains("build_router(") {
            violations.push(format!(
                "{rel_str}: live_* test must not build ad-hoc Router::new() mock endpoints"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Zero-mock policy gate failed ({} violations):\n{}",
        violations.len(),
        violations.join("\n")
    );
}
