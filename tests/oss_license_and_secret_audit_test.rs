//! Permanent OSS release-gate audit test for `open-estate` (zero-knowledge invariants):
//! 1. Verifies every transitive Rust dependency in `cargo metadata` has a permissive
//!    license option (no mandatory GPL/AGPL/LGPL/SSPL/BUSL/Commons-Clause/CC-BY-NC).
//! 2. Verifies vendored frontend assets (`assets/vendor/`, `extension/vendor/`) contain
//!    zero copyleft/NC license headers.
//! 3. Verifies `.dockerignore` and `Dockerfile` strictly exclude secrets, `.env*`, `.git`,
//!    `target`, `.cache`, `data`, `reports`, `*.db*`, `*.pem`, `*.key`, and run non-root.
//! 4. Enforces zero leaked API keys/PATs, zero unapproved routable public IPv4 literals,
//!    zero private/Tailscale IPs in config/assets, and zero non-synthetic street addresses.

use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

const PERMISSIVE_TOKENS: &[&str] = &[
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "Unlicense",
    "0BSD",
    "Unicode-3.0",
    "CDLA-Permissive-2.0",
    "BSL-1.0",
];

fn normalize_spdx_atom(raw: &str) -> &str {
    raw.trim().trim_matches(|c| c == '(' || c == ')')
}

/// Returns `true` if the SPDX expression has at least one fully permissive choice.
/// - `A OR B` (or `A / B`): valid if *either* branch is permissive.
/// - `A AND B`: valid only if *both* branches are permissive.
fn is_permissive_license_expr(expr: &str) -> bool {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return false;
    }
    let inner = if trimmed.starts_with('(') && trimmed.ends_with(')') {
        let mut depth = 0usize;
        let mut wraps_all = true;
        for (idx, ch) in trimmed.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && idx + 1 < trimmed.len() {
                        wraps_all = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if wraps_all {
            &trimmed[1..trimmed.len() - 1]
        } else {
            trimmed
        }
    } else {
        trimmed
    };

    let or_parts = split_top_level(inner, &[" OR ", " / ", "/"]);
    if or_parts.len() > 1 {
        return or_parts.into_iter().any(is_permissive_license_expr);
    }

    let and_parts = split_top_level(inner, &[" AND "]);
    if and_parts.len() > 1 {
        return and_parts.into_iter().all(is_permissive_license_expr);
    }

    let atom = normalize_spdx_atom(inner);
    PERMISSIVE_TOKENS.contains(&atom)
}

fn split_top_level<'a>(s: &'a str, seps: &[&str]) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut last = 0usize;
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => {
                let rest = &s[i..];
                if let Some(sep) = seps.iter().find(|&&sep| rest.starts_with(sep)) {
                    parts.push(&s[last..i]);
                    i += sep.len();
                    last = i;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&s[last..]);
    parts
}

#[test]
fn audit_cargo_dependency_graph_licenses() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest_path = manifest_dir.join("Cargo.toml");

    let mut out = Command::new("cargo")
        .args(["metadata", "--manifest-path"])
        .arg(&manifest_path)
        .args(["--format-version", "1", "--offline", "--locked"])
        .output()
        .expect("run cargo metadata");
    if !out.status.success() {
        out = Command::new("cargo")
            .args(["metadata", "--manifest-path"])
            .arg(&manifest_path)
            .args(["--format-version", "1", "--locked"])
            .output()
            .expect("run cargo metadata online fallback");
    }
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let meta: Value = serde_json::from_slice(&out.stdout).expect("parse cargo metadata JSON");
    let pkgs = meta["packages"].as_array().expect("packages array");
    assert!(!pkgs.is_empty(), "dependency graph must not be empty");

    let mut violations = Vec::new();
    let mut checked = 0usize;
    for pkg in pkgs {
        let name = pkg["name"].as_str().unwrap_or("");
        let version = pkg["version"].as_str().unwrap_or("");
        if name == "open-estate" {
            assert_eq!(
                pkg["license_file"].as_str(),
                Some("LICENSE"),
                "root package must point license_file to LICENSE"
            );
            continue;
        }
        checked += 1;
        let lic = pkg["license"].as_str().unwrap_or("");
        if lic.is_empty() || !is_permissive_license_expr(lic) {
            violations.push(format!("{name} {version}: license={lic:?}"));
        }
    }

    assert!(checked > 100, "expected full transitive graph, got {checked}");
    assert!(
        violations.is_empty(),
        "Found dependencies without a permissive OSS license:\n{}",
        violations.join("\n")
    );
}

#[test]
fn audit_vendored_assets_and_docker_hygiene() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    // 1. Vendored JS assets must contain zero copyleft / NC license headers
    let forbid_vendor =
        Regex::new(r"(?i)(?:gnu general public|affero general public|cc[- ]by[- ]nc|sspl|commons clause)")
            .unwrap();
    for rel_dir in ["assets/vendor", "extension/vendor"] {
        let dir = root.join(rel_dir);
        assert!(dir.is_dir(), "missing vendor dir {}", dir.display());
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_file() {
                let txt = fs::read_to_string(&p).expect("read vendored file");
                assert!(
                    !forbid_vendor.is_match(&txt),
                    "Forbidden license header in vendored file {}",
                    p.display()
                );
            }
        }
    }

    // 2. .dockerignore must exclude sensitive/runtime artifacts
    let dockerignore = fs::read_to_string(root.join(".dockerignore")).expect("read .dockerignore");
    let lines: HashSet<&str> = dockerignore
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    for required in [
        ".git",
        ".env",
        ".env*",
        "target",
        ".cache",
        "data",
        "reports",
        "*.db*",
        "*.pem",
        "*.key",
    ] {
        assert!(
            lines.contains(required),
            ".dockerignore must exclude `{required}`"
        );
    }

    // 3. Dockerfile must not embed secrets, must run as non-root, and must default to Open Estate
    let dockerfile = fs::read_to_string(root.join("Dockerfile")).expect("read Dockerfile");
    assert!(
        !dockerfile.contains("COPY .env"),
        "Dockerfile must never COPY .env"
    );
    assert!(
        dockerfile.contains("build.rs")
            && dockerfile.contains("COPY proto")
            && dockerfile.contains("COPY extensions"),
        "Dockerfile must copy build.rs, proto, and extensions for build"
    );
    assert!(
        dockerfile.contains("USER 1000:1000"),
        "Dockerfile must drop privileges to non-root USER 1000:1000"
    );
    assert!(
        dockerfile.contains("BRAND_NAME=\"Open Estate\""),
        "Dockerfile must default BRAND_NAME to Open Estate"
    );

    // 4. Local .env (if present on developer machine) must never be tracked in git
    if root.join(".git").exists() {
        if let Ok(git_out) = Command::new("git")
            .args(["-C"])
            .arg(root)
            .args(["ls-files", ".env", ".env.*"])
            .output()
        {
            if git_out.status.success() {
                let tracked: Vec<String> = String::from_utf8_lossy(&git_out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && *l != ".env.example")
                    .map(str::to_string)
                    .collect();
                assert!(
                    tracked.is_empty(),
                    "Sensitive .env file(s) tracked by git: {:?}",
                    tracked
                );
            }
        }
    }
}

fn collect_repo_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name_str.as_ref(), ".git" | "target" | ".cache" | "node_modules") {
                continue;
            }
            collect_repo_files(&path, out);
        } else {
            if name_str == ".env" || (name_str.starts_with(".env.") && name_str != ".env.example") {
                continue;
            }
            out.push(path);
        }
    }
}

fn is_standard_test_or_private_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // Unspecified / loopback / RFC1918 / link-local / CGNAT (RFC6598) / multicast / broadcast / IETF
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
        || o[0] >= 240
    {
        return true;
    }
    // Standard public DNS / dummy placeholder literals used in docs & unit tests
    matches!(
        o,
        [1, 0, 0, 0]
            | [1, 0, 0, 1]
            | [1, 1, 1, 1]
            | [1, 2, 3, 4]
            | [4, 4, 4, 4]
            | [8, 8, 4, 4]
            | [8, 8, 8, 0]
            | [8, 8, 8, 8]
            | [9, 9, 9, 9]
    )
}

#[test]
fn audit_zero_secrets_cluster_ips_and_real_street_addresses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_repo_files(root, &mut files);

    // Generic secret/credential format patterns (no private literals embedded)
    let secret_pats = vec![
        ("Docker Hub PAT", Regex::new(r"dckr_pat_[A-Za-z0-9_\-]{10,}").unwrap()),
        ("OpenAI / Anthropic key", Regex::new(r"sk-(?:proj-|ant-)[A-Za-z0-9_\-]{20,}").unwrap()),
        ("OpenAI legacy key", Regex::new(r"sk-[A-Za-z0-9]{40,}").unwrap()),
        ("Google API key", Regex::new(r"AIza[0-9A-Za-z\-_]{30,}").unwrap()),
        ("GitHub PAT", Regex::new(r"(?:ghp_|github_pat_)[A-Za-z0-9_]{20,}").unwrap()),
        ("Slack token", Regex::new(r"xox[baprs]-[0-9A-Za-z\-]{15,}").unwrap()),
        ("Restricted live key", Regex::new(r"rk_live_[0-9A-Za-z]{15,}").unwrap()),
        ("Webhook secret", Regex::new(r"whsec_[0-9A-Za-z]{15,}").unwrap()),
        ("FreeRoute API key", Regex::new(r"fro_[0-9a-f]{32,}").unwrap()),
        ("Mapbox JWT token", Regex::new(r"pk\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{10,}").unwrap()),
        ("Hex public API key", Regex::new(r"pk\.[0-9a-f]{32,}").unwrap()),
        ("Base64 JSON API key", Regex::new(r"\beyJ[A-Za-z0-9+/=_-]{44,}\b").unwrap()),
        ("PEM private key", Regex::new(r"-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----").unwrap()),
        ("Tailscale magic domain", Regex::new(&format!(r"(?i)\b{}", concat!(r"ts\.", r"net\b"))).unwrap()),
    ];

    // Internal codenames/domains/handles constructed via concat! so this file contains zero raw literals
    let internal_codenames: Vec<(&str, String)> = vec![
        ("private product brand", concat!("deed", "buddy").to_string()),
        ("private author handle", concat!("eva", "wu").to_string()),
        ("private infra handle", concat!("kube", "b").to_string()),
        ("private github org", concat!("ql-", "owo-", "lp").to_string()),
        ("internal model codename", concat!("gpt-", "6-lu", "na").to_string()),
        ("internal fallback codename", concat!("gpt-", "re", "serve").to_string()),
        ("internal gateway codename", concat!("llm", "api").to_string()),
        ("private k8s repo", concat!("my", "k3s").to_string()),
        ("private host repo", concat!("home-", "server").to_string()),
        ("private side repo", concat!("leap", "term").to_string()),
        ("private domain token", concat!("ph", "slc").to_string()),
        ("private org token", concat!("onehuman", "corp").to_string()),
        ("private proxy token", concat!("lite", "llm").to_string()),
        ("OCI domain", concat!("oracle", "cloud.com").to_string()),
        ("OCI OCID", concat!("oci", "d1.").to_string()),
        ("developer home prefix", concat!("/home/", "kev", "in").to_string()),
        ("developer handle", concat!("kev", "in").to_string()),
    ];

    // Structural street-address check: reject any `<num> <TitleCaseWords> <StreetSuffix>` unless synthetic
    let street_addr_rx = Regex::new(
        r"\b\d{1,5}\s+(?:[A-Z][A-Za-z0-9'.-]*\s+){1,4}(?:Ave|Avenue|St|Street|Rd|Road|Dr|Drive|Ln|Lane|Way|Ct|Court|Blvd|Boulevard|Pl|Place|Cir|Circle|Ter|Terrace|Pkwy|Parkway)\b",
    )
    .unwrap();

    // Structural IPv4 check: catch any unexpected public/cluster/node IP without hardcoding any IP
    let ipv4_rx = Regex::new(r"(?:^|[^0-9.])(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})(?:[^0-9.]|$)").unwrap();
    let chrome_ua_rx = Regex::new(r"Chrome/\d+\.\d+\.\d+\.\d+").unwrap();

    let private_ip_pat = Regex::new(
        r"(?:^|[^0-9.])(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3}|100\.(?:6[4-9]|[7-9]\d|1[0-1]\d|12[0-7])\.\d{1,3}\.\d{1,3})(?:[^0-9.]|$)",
    )
    .unwrap();

    // Structural developer home-directory check: reject hardcoded `/Users/<name>/` or `/home/<user>/`
    let dev_home_rx = Regex::new(r#"(?:^|[\s"'`(=:]|file://)(?:/Users/|/home/)([A-Za-z0-9._-]+)/"#).unwrap();

    let mut violations = Vec::new();
    for path in &files {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let rel_str = rel.to_string_lossy();
        if rel_str == "Cargo.lock" {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };

        let is_config_or_asset = rel_str == "Dockerfile"
            || rel_str == "docker-compose.yml"
            || rel_str == ".env.example"
            || rel_str == "README.md"
            || rel_str.starts_with(".github/")
            || rel_str.starts_with("deploy/")
            || rel_str.starts_with("scripts/")
            || rel_str.starts_with("assets/")
            || rel_str.starts_with("extension/")
            || rel_str.starts_with("extensions/");

        let allow_proxy_or_rate_test_ips = rel_str == "tests/free_proxy_pool_test.rs"
            || rel_str == "tests/ddos_bombing_test.rs";

        for (idx, line) in text.lines().enumerate() {
            let lineno = idx + 1;

            for (label, rx) in &secret_pats {
                if rx.is_match(line) {
                    violations.push(format!("{rel_str}:{lineno}: {label}"));
                }
            }

            let line_low = line.to_ascii_lowercase();
            for (label, token) in &internal_codenames {
                if line_low.contains(token.as_str()) {
                    violations.push(format!("{rel_str}:{lineno}: leaked {label}"));
                }
            }

            for m in street_addr_rx.find_iter(line) {
                let s = m.as_str();
                if !s.contains("Synth")
                    && !s.contains("Example")
                    && !s.contains("Sample")
                    && !s.contains("Test")
                {
                    violations.push(format!("{rel_str}:{lineno}: non-synthetic street address `{s}`"));
                }
            }

            if is_config_or_asset && private_ip_pat.is_match(line) {
                violations.push(format!("{rel_str}:{lineno}: private/Tailscale IP in config or asset"));
            }

            for cap in dev_home_rx.captures_iter(line) {
                let user = &cap[1];
                if !matches!(user, "app" | "nonroot" | "ubuntu" | "openestate" | "runner") {
                    violations.push(format!("{rel_str}:{lineno}: hardcoded developer home path for `{user}`"));
                }
            }

            if !allow_proxy_or_rate_test_ips {
                let line_wo_ua = chrome_ua_rx.replace_all(line, "");
                for cap in ipv4_rx.captures_iter(&line_wo_ua) {
                    let raw_ip = &cap[1];
                    let Ok(ip) = Ipv4Addr::from_str(raw_ip) else {
                        continue;
                    };
                    if is_standard_test_or_private_ipv4(ip) {
                        continue;
                    }
                    // Allow statutory NMAC rule citations (`13.14.9.*`) and the explicit
                    // non-RFC1918 public-IP classification unit test in `src/web/ip_geo.rs`.
                    let o = ip.octets();
                    if (rel_str == "src/modules/title_legal_radar.rs" && o[0] == 13 && o[1] == 14 && o[2] == 9)
                        || (rel_str == "src/web/ip_geo.rs" && o == [172, 32, 10, 5])
                    {
                        continue;
                    }
                    violations.push(format!("{rel_str}:{lineno}: unexpected public IPv4 literal `{raw_ip}`"));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "OSS zero-knowledge security/address/IP audit failed ({} hits):\n{}",
        violations.len(),
        violations.into_iter().take(40).collect::<Vec<_>>().join("\n")
    );
}
