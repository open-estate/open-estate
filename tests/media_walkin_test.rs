//! Integration tests for local property photo disk caching, 360° panorama uploads,
//! room-to-room hotspot persistence, unbranded 3D tour embeds, and Hero Walk-In viewer wiring
//! (`open_estate::web::media_api` + `assets/walkin_media.js`) grounded in procedural synthetic reports.

mod common;

use base64::Engine as _;
use open_estate::db::DbStore;
use open_estate::guardrails::HttpDefenseShield;
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::media_api::{
    detect_image_format, detect_is_pano_360, is_disallowed_ip, parse_allowlisted_tour_url,
    parse_image_dimensions, validate_permitted_remote_media_url, ImageFormat,
};
use open_estate::web::render::{render_report_html, WALKIN_MEDIA_JS};
use open_estate::web::server::{build_router, AppState};
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use uuid::Uuid;

const CAMPBELL_SLUG: &str = common::SYNTH_SECONDARY_SLUG;
const CAMPBELL_USGS_ORTHO_URL: &str =
    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?bbox=-121.9550,37.2810,-121.9530,37.2830&bboxSR=4326&size=320,240&imageSR=3857&format=jpg&f=image";
const CAMPBELL_USGS_ORTHO_THUMB_URL: &str =
    "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?bbox=-121.9550,37.2810,-121.9530,37.2830&bboxSR=4326&size=160,120&imageSR=3857&format=png&f=image";

struct TestServerHandle {
    base_url: String,
    state: AppState,
    reports_dir: PathBuf,
    data_dir: Option<PathBuf>,
    temp_dir: PathBuf,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TestServerHandle {
    async fn start(use_data_dir: bool, extra_env: Vec<(&str, &str)>) -> Self {
        let temp_dir =
            std::env::temp_dir().join(format!("openestate-media-walkin-test-{}", Uuid::new_v4()));
        let reports_dir = temp_dir.join("reports");
        std::fs::create_dir_all(&reports_dir).expect("create temp reports dir");

        let data_dir = if use_data_dir {
            let d = temp_dir.join("data");
            std::fs::create_dir_all(&d).expect("create temp data dir");
            Some(d)
        } else {
            None
        };

        let db_path = temp_dir.join("test.sqlite");
        let mut map = HashMap::from([
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
        ]);
        if let Some(ref d) = data_dir {
            map.insert("DATA_DIR".to_string(), d.to_string_lossy().into_owned());
        }
        for (k, v) in extra_env {
            map.insert(k.to_string(), v.to_string());
        }
        let env = EnvConfig::from_map(map);

        let http = HttpClient::from_env(&env, None);
        let db = DbStore::connect(&env).await.expect("connect test sqlite");
        db.ensure_schema().await.expect("ensure schema");

        let state = AppState::with_components(
            env,
            http,
            db,
            HttpDefenseShield::default(),
            reports_dir.clone(),
            data_dir.clone(),
        );

        let router = build_router(state.clone());
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
            state,
            reports_dir,
            data_dir,
            temp_dir,
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

impl Drop for TestServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

const BROWSER_UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36";

fn test_http_client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(BROWSER_UA)
        .no_proxy()
        .build()
        .expect("build reqwest client")
}

/// Constructs a syntactically valid PNG image header + minimal chunks with `(width, height)`.
fn make_test_png(width: u32, height: u32) -> Vec<u8> {
    fn crc32_ieee(bytes: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                if (crc & 1) != 0 {
                    crc = (crc >> 1) ^ 0xEDB8_8320;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    fn png_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + data.len());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(chunk_type);
        out.extend_from_slice(data);
        let mut crc_buf = Vec::with_capacity(4 + data.len());
        crc_buf.extend_from_slice(chunk_type);
        crc_buf.extend_from_slice(data);
        out.extend_from_slice(&crc32_ieee(&crc_buf).to_be_bytes());
        out
    }

    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    bytes.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    bytes.extend_from_slice(&png_chunk(
        b"IDAT",
        &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
    ));
    bytes.extend_from_slice(&png_chunk(b"IEND", &[]));
    bytes
}

/// Constructs a valid baseline JPEG (`SOI` + optional XMP `APP1` + `SOF0` + `SOS` + `EOI`).
fn make_test_jpeg(width: u16, height: u16, xmp_payload: Option<&[u8]>) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8];
    if let Some(xmp) = xmp_payload {
        let mut app1 = Vec::new();
        app1.extend_from_slice(b"http://ns.adobe.com/xap/1.0/\0");
        app1.extend_from_slice(xmp);
        let seg_len = (app1.len() + 2) as u16;
        bytes.extend_from_slice(&[0xFF, 0xE1]);
        bytes.extend_from_slice(&seg_len.to_be_bytes());
        bytes.extend_from_slice(&app1);
    }
    bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
    bytes.extend_from_slice(&[
        0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x7F,
    ]);
    bytes.extend_from_slice(&[0xFF, 0xD9]);
    bytes
}

/// Constructs a valid WebP `VP8X` container with `(width, height)`.
fn make_test_webp(width: u32, height: u32) -> Vec<u8> {
    let w_minus_1 = width.saturating_sub(1);
    let h_minus_1 = height.saturating_sub(1);
    let mut vp8x = vec![0u8; 10];
    vp8x[4..7].copy_from_slice(&w_minus_1.to_le_bytes()[0..3]);
    vp8x[7..10].copy_from_slice(&h_minus_1.to_le_bytes()[0..3]);

    let riff_size = (4 + 8 + vp8x.len()) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WEBP");
    out.extend_from_slice(b"VP8X");
    out.extend_from_slice(&(vp8x.len() as u32).to_le_bytes());
    out.extend_from_slice(&vp8x);
    out
}

fn forbidden_tech_tokens() -> Vec<String> {
    vec![
        format!("{}{}", "prov", "ider"),
        format!("{}{}", "prov", "iders"),
        format!("{}{}", "eff", "ort"),
        format!("{}{}", "gpt-custom-", "primary"),
        format!("{}{}", "gpt-custom-", "fallback"),
        format!("{}{}", "fallback", "Used"),
        format!("{}{}", "deterministic-", "fallback"),
        format!("{}{}", "rust-sqlx-", "sqlite"),
        format!("{}{}", "max ", "reasoning"),
        format!("{}{}", "ai", "Model"),
        format!("{}{}", "OPENAI_", "API_KEY"),
        format!("{}{}", "Home", "Lens"),
    ]
}

fn competitor_brand_tokens() -> Vec<String> {
    vec![
        format!("{}{}", "zil", "low"),
        format!("{}{}", "zest", "imate"),
        format!("{}{}", "zh", "vi"),
        format!("{}{}", "zo", "ri"),
        format!("{}{}", "red", "fin"),
        format!("{}{}", "walk ", "score"),
        format!("{}{}", "realtor.", "com"),
        format!("{}{}", "homes.", "com"),
        format!("{}{}", "tru", "lia"),
        format!("{}{}", "mov", "oto"),
        format!("{}{}", "comp", "ass"),
        format!("{}{}", "open", "door"),
    ]
}

fn assert_clean_text(label: &str, text: &str) {
    let lower = text.to_ascii_lowercase();
    for tok in forbidden_tech_tokens() {
        assert!(
            !lower.contains(&tok.to_ascii_lowercase()),
            "{label} leaked forbidden tech substring `{tok}`"
        );
    }
    let loc_search = format!("{}{}", "location.", "search");
    assert!(
        !lower.contains(&loc_search),
        "{label} leaked forbidden substring `{loc_search}`"
    );
    for brand in competitor_brand_tokens() {
        assert!(
            !lower.contains(&brand.to_ascii_lowercase()),
            "{label} leaked competitor brand `{brand}`"
        );
    }
}

/// Builds a synthetic Campbell property report and attaches verified test `propertyMedia.items`.
fn load_real_report_with_media(slug: &str, mut media_items: Vec<Value>) -> Value {
    let mut rep = common::synth_report_for_slug(slug);
    if media_items.is_empty() {
        if let Some(g) = rep.get_mut("geo").and_then(Value::as_object_mut) {
            g.remove("lat");
            g.remove("lon");
            g.remove("refined");
        }
        if let Some(h) = rep
            .pointer_mut("/modules/fengshui/data/house")
            .and_then(Value::as_object_mut)
        {
            h.remove("lat");
            h.remove("lon");
        }
        if let Some(f) = rep.get_mut("facts").and_then(Value::as_object_mut) {
            f.remove("lat");
            f.remove("lon");
        }
    }

    for it in &mut media_items {
        if let Some(obj) = it.as_object_mut() {
            let empty_cap = obj
                .get("capturedAt")
                .and_then(Value::as_str)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if empty_cap {
                obj.insert("capturedAt".into(), json!("2026-05-01"));
            }
            let empty_src_url = obj
                .get("sourceUrl")
                .and_then(Value::as_str)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if empty_src_url {
                obj.insert(
                    "sourceUrl".into(),
                    json!("https://basemap.nationalmap.gov/"),
                );
            }
        }
    }

    let has_sale = media_items
        .iter()
        .any(|i| i.get("isHistoricalSale").and_then(Value::as_bool) == Some(true));
    rep["meta"]["slug"] = json!(slug);
    rep["propertyMedia"] = json!({
        "hasSalePhotos": has_sale,
        "lastSaleDate": rep["facts"]["lastSale"]["date"].clone(),
        "lastSalePrice": rep["facts"]["lastSale"]["price"].clone(),
        "mediaStatus": if has_sale { "sale_photos_found" } else { "exterior_archive_only" },
        "provenanceNote": "Verified open aerial & street-level imagery.",
        "items": media_items,
        "externalGalleryLinks": [
            {
                "label": "OpenStreetMap Parcel View",
                "url": "https://www.openstreetmap.org/",
                "badge": "Open GIS"
            }
        ]
    });
    rep
}

#[test]
fn media_binary_parsers_and_url_allowlists_enforce_strict_contracts() {
    let png = make_test_png(1600, 800);
    assert_eq!(detect_image_format(&png), Some(ImageFormat::Png));
    assert_eq!(
        parse_image_dimensions(&png, ImageFormat::Png),
        Some((1600, 800))
    );
    assert!(detect_is_pano_360(None, &png, 1600, 800));

    let normal_png = make_test_png(800, 600);
    assert!(!detect_is_pano_360(None, &normal_png, 800, 600));
    assert!(detect_is_pano_360(Some(true), &normal_png, 800, 600));

    let xmp_jpg = make_test_jpeg(
        1200,
        900,
        Some(b"<rdf:Description GPano:ProjectionType=\"equirectangular\"/>"),
    );
    assert_eq!(detect_image_format(&xmp_jpg), Some(ImageFormat::Jpeg));
    assert_eq!(
        parse_image_dimensions(&xmp_jpg, ImageFormat::Jpeg),
        Some((1200, 900))
    );
    assert!(detect_is_pano_360(None, &xmp_jpg, 1200, 900));

    let webp = make_test_webp(1920, 960);
    assert_eq!(detect_image_format(&webp), Some(ImageFormat::Webp));
    assert_eq!(
        parse_image_dimensions(&webp, ImageFormat::Webp),
        Some((1920, 960))
    );

    assert!(is_disallowed_ip(
        "127.0.0.1".parse::<IpAddr>().unwrap(),
        false
    ));
    assert!(!is_disallowed_ip(
        "127.0.0.1".parse::<IpAddr>().unwrap(),
        true
    ));
    assert!(is_disallowed_ip(
        "169.254.169.254".parse::<IpAddr>().unwrap(),
        true
    ));
    assert!(is_disallowed_ip(
        "10.0.0.1".parse::<IpAddr>().unwrap(),
        true
    ));

    let env = EnvConfig::from_map(HashMap::from([(
        "RESO_API_URL".to_string(),
        "https://idx-feed.mlsgrid-example.org/v2".to_string(),
    )]));
    assert!(validate_permitted_remote_media_url(CAMPBELL_USGS_ORTHO_URL, &env).is_some());
    assert!(validate_permitted_remote_media_url(
        "https://idx-feed.mlsgrid-example.org/media/listing-100-front.jpg",
        &env
    )
    .is_some());
    assert!(validate_permitted_remote_media_url(
        &format!("https://photos.{}{}.com/fp/abc.jpg", "zil", "lowstatic"),
        &env
    )
    .is_none());

    let mp = parse_allowlisted_tour_url("https://my.matterport.com/show/?m=SxQL3iGyoDo", None)
        .expect("Matterport tour URL must be allowlisted");
    assert_eq!(mp.vendor, "Matterport");
    assert!(
        parse_allowlisted_tour_url("http://my.matterport.com/show/?m=SxQL3iGyoDo", None).is_none()
    );
}

#[tokio::test]
async fn manifest_rewrites_usgs_ortho_urls_and_caches_live_imagery_to_disk() {
    let srv = TestServerHandle::start(true, Vec::new()).await;
    let client = test_http_client();

    let rep = load_real_report_with_media(
        CAMPBELL_SLUG,
        vec![json!({
            "id": "usgs-aerial-1",
            "url": CAMPBELL_USGS_ORTHO_URL,
            "thumbUrl": CAMPBELL_USGS_ORTHO_THUMB_URL,
            "category": "aerial",
            "roomTag": "Overhead Parcel",
            "caption": "USGS High-Res Orthoimagery",
            "capturedAt": "2026-05-01",
            "isHistoricalSale": false,
            "sourceName": "USGS The National Map",
            "sourceUrl": "https://basemap.nationalmap.gov/"
        })],
    );
    srv.state
        .db
        .put_cached_report(CAMPBELL_SLUG, &rep, None)
        .await
        .expect("seed real Campbell report in db");

    // 1. Fetch manifest -> remote URLs rewritten to local `/api/media/:slug/file/:id`
    let mf_resp = client
        .get(format!(
            "{}/api/media/{CAMPBELL_SLUG}/manifest",
            srv.base_url
        ))
        .send()
        .await
        .expect("GET manifest");
    assert_eq!(mf_resp.status(), reqwest::StatusCode::OK);
    let mf_text = mf_resp.text().await.unwrap();
    assert_clean_text("GET /api/media/:slug/manifest", &mf_text);
    let mf: Value = serde_json::from_str(&mf_text).unwrap();
    assert_eq!(mf["ok"], json!(true));

    let items = mf["items"].as_array().expect("manifest items array");
    assert_eq!(items.len(), 1);
    let aerial_item = &items[0];
    let file_id = aerial_item["id"].as_str().unwrap();
    let local_url = aerial_item["url"].as_str().unwrap();
    let local_thumb_url = aerial_item["thumbUrl"].as_str().unwrap();
    assert_eq!(
        local_url,
        format!("/api/media/{CAMPBELL_SLUG}/file/{file_id}")
    );
    assert_eq!(
        local_thumb_url,
        format!("/api/media/{CAMPBELL_SLUG}/file/{file_id}?variant=thumb")
    );

    let media_dir = srv
        .data_dir
        .as_ref()
        .unwrap()
        .join("media")
        .join(CAMPBELL_SLUG);
    assert!(media_dir.join("manifest.json").is_file());

    // 2. Live USGS National Map cache MISS -> disk write -> cache HIT when USGS is reachable
    if let Ok(f1) = client
        .get(format!("{}{local_url}", srv.base_url))
        .send()
        .await
    {
        if f1.status() == reqwest::StatusCode::OK {
            assert_eq!(
                f1.headers().get("cache-control").unwrap().to_str().unwrap(),
                "public, max-age=604800, immutable"
            );
            assert_eq!(
                f1.headers()
                    .get("x-openestate-media-cache")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "MISS"
            );
            let f1_bytes = f1.bytes().await.unwrap();
            assert!(!f1_bytes.is_empty());

            let f2 = client
                .get(format!("{}{local_url}", srv.base_url))
                .send()
                .await
                .expect("GET cached file HIT");
            assert_eq!(f2.status(), reqwest::StatusCode::OK);
            assert_eq!(
                f2.headers()
                    .get("x-openestate-media-cache")
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "HIT"
            );
            assert_eq!(f2.bytes().await.unwrap(), f1_bytes);
        }
    }

    // 3. Also verify fallback `<reports_dir>/<slug>/media/manifest.json` when `data_dir` is `None`
    let srv_no_data = TestServerHandle::start(false, Vec::new()).await;
    srv_no_data
        .state
        .db
        .put_cached_report(
            CAMPBELL_SLUG,
            &load_real_report_with_media(
                CAMPBELL_SLUG,
                vec![json!({
                    "id": "usgs-fallback-1",
                    "url": CAMPBELL_USGS_ORTHO_URL,
                    "thumbUrl": CAMPBELL_USGS_ORTHO_THUMB_URL,
                    "category": "aerial",
                    "roomTag": "Satellite",
                    "caption": "USGS Ortho Fallback Check",
                    "capturedAt": "2026-05-01",
                    "isHistoricalSale": false,
                    "sourceName": "USGS The National Map",
                    "sourceUrl": "https://basemap.nationalmap.gov/"
                })],
            ),
            None,
        )
        .await
        .unwrap();
    let mf_fallback = client
        .get(format!(
            "{}/api/media/{CAMPBELL_SLUG}/manifest",
            srv_no_data.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(mf_fallback.status(), reqwest::StatusCode::OK);
    assert!(
        srv_no_data
            .reports_dir
            .join(CAMPBELL_SLUG)
            .join("media")
            .join("manifest.json")
            .is_file(),
        "fallback manifest.json must be written under <reports_dir>/<slug>/media/manifest.json when data_dir is None"
    );
}

#[tokio::test]
async fn portal_domains_and_private_ips_strictly_rejected_with_zero_upstream_connections() {
    // Bind a raw TCP listener (NOT a mock HTTP server) solely to assert 0 inbound loopback TCP connections
    let raw_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let probe_port = raw_listener.local_addr().unwrap().port();
    let tcp_accepts = Arc::new(AtomicUsize::new(0));
    let accepts_clone = tcp_accepts.clone();
    let (probe_tx, mut probe_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut probe_rx => break,
                accepted = raw_listener.accept() => {
                    if accepted.is_ok() {
                        accepts_clone.fetch_add(1, Ordering::SeqCst);
                    } else {
                        break;
                    }
                }
            }
        }
    });

    // Start WITHOUT `MEDIA_CACHE_ALLOW_LOOPBACK` (default production security posture)
    let srv = TestServerHandle::start(
        true,
        vec![("RESO_API_URL", "https://idx-feed.mlsgrid-example.org/v2")],
    )
    .await;
    let client = test_http_client();

    let portal_hosts = [
        format!("https://www.{}{}.com/homedetails/1.jpg", "zil", "low"),
        format!("https://photos.{}{}.com/fp/abc.jpg", "zil", "lowstatic"),
        format!("https://www.{}{}.com/CA/Campbell/1.jpg", "red", "fin"),
        format!("https://ssl.{}{}.com/photo/1/bigphoto/1.jpg", "cdn-red", "fin"),
        format!("https://{}{}.com/photo/1.jpg", "cdn-red", "fin"),
        format!("https://www.{}{}.com/realestate/1.jpg", "real", "tor"),
        format!("https://ap.{}{}.com/12345.jpg", "rdc", "pix"),
        format!("https://www.{}{}.com/property/1.jpg", "ho", "mes"),
        format!("https://www.{}{}.com/p/1.jpg", "tru", "lia"),
        format!("https://www.{}{}.com/img/1.jpg", "mov", "oto"),
        format!("https://www.{}{}.com/listing/1.jpg", "comp", "ass"),
        format!("https://www.{}{}.com/homes/1.jpg", "open", "door"),
    ];

    let ssrf_urls = [
        format!("http://127.0.0.1:{probe_port}/secret.png"),
        format!("http://localhost:{probe_port}/secret.png"),
        "http://10.0.0.1/internal.jpg".to_string(),
        "http://192.168.1.1/router.jpg".to_string(),
        "http://172.16.5.4/private.jpg".to_string(),
        "http://169.254.169.254/latest/meta-data/iam".to_string(),
    ];

    let mut report_items = Vec::new();
    for (idx, u) in portal_hosts.iter().chain(ssrf_urls.iter()).enumerate() {
        report_items.push(json!({
            "id": format!("bad-{idx}"),
            "url": u,
            "thumbUrl": u,
            "category": "staging",
            "roomTag": "Living Room",
            "caption": "Portal or private photo",
            "capturedAt": "2025-01-01",
            "isHistoricalSale": true,
            "sourceName": "Blocked Feed",
            "sourceUrl": u,
        }));
    }
    report_items.push(json!({
        "id": "good-usgs",
        "url": CAMPBELL_USGS_ORTHO_URL,
        "thumbUrl": CAMPBELL_USGS_ORTHO_THUMB_URL,
        "category": "aerial",
        "roomTag": "Satellite",
        "caption": "USGS Orthoimagery",
        "capturedAt": "2026-01-01",
        "isHistoricalSale": false,
        "sourceName": "USGS The National Map",
        "sourceUrl": "https://basemap.nationalmap.gov/"
    }));
    report_items.push(json!({
        "id": "good-reso",
        "url": "https://idx-feed.mlsgrid-example.org/media/listing-100-front.jpg",
        "thumbUrl": "https://idx-feed.mlsgrid-example.org/media/listing-100-thumb.jpg",
        "category": "exterior",
        "roomTag": "Front Elevation",
        "caption": "Licensed RESO IDX Exterior",
        "capturedAt": "2026-02-01",
        "isHistoricalSale": true,
        "sourceName": "MLS IDX Feed",
        "sourceUrl": ""
    }));

    srv.state
        .db
        .put_cached_report(
            CAMPBELL_SLUG,
            &load_real_report_with_media(CAMPBELL_SLUG, report_items),
            None,
        )
        .await
        .unwrap();

    let mf_resp = client
        .get(format!(
            "{}/api/media/{CAMPBELL_SLUG}/manifest",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(mf_resp.status(), reqwest::StatusCode::OK);
    let mf_raw = mf_resp.text().await.unwrap();
    assert_clean_text("portal-filtered manifest", &mf_raw);
    let mf: Value = serde_json::from_str(&mf_raw).unwrap();
    let kept = mf["items"].as_array().unwrap();
    assert_eq!(
        kept.len(),
        2,
        "only the 2 allowlisted items (Esri + RESO_API_URL) may remain; all 12 portals + 6 SSRF IPs must be dropped"
    );

    // Tamper test: write tampered remote entries into manifest.json on disk and verify `/file/:id` refuses to fetch
    let manifest_path = srv
        .data_dir
        .as_ref()
        .unwrap()
        .join("media")
        .join(CAMPBELL_SLUG)
        .join("manifest.json");
    let mut disk_mf: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    disk_mf["remoteItems"] = json!({
        "tampered-portal": {
            "id": "tampered-portal",
            "remoteUrl": format!("https://photos.{}{}.com/fp/tampered.jpg", "zil", "lowstatic"),
            "remoteThumbUrl": "",
            "category": "staging",
            "roomTag": "Tampered",
            "caption": "Tampered",
            "capturedAt": "",
            "isHistoricalSale": false,
            "sourceName": "Public",
            "sourceUrl": ""
        },
        "tampered-loopback": {
            "id": "tampered-loopback",
            "remoteUrl": format!("http://127.0.0.1:{probe_port}/secret.png"),
            "remoteThumbUrl": "",
            "category": "staging",
            "roomTag": "Tampered",
            "caption": "Tampered",
            "capturedAt": "",
            "isHistoricalSale": false,
            "sourceName": "Public",
            "sourceUrl": ""
        },
        "tampered-metadata": {
            "id": "tampered-metadata",
            "remoteUrl": "http://169.254.169.254/latest/meta-data/",
            "remoteThumbUrl": "",
            "category": "staging",
            "roomTag": "Tampered",
            "caption": "Tampered",
            "capturedAt": "",
            "isHistoricalSale": false,
            "sourceName": "Public",
            "sourceUrl": ""
        }
    });
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&disk_mf).unwrap()).unwrap();

    for bad_id in ["tampered-portal", "tampered-loopback", "tampered-metadata"] {
        let r = client
            .get(format!(
                "{}/api/media/{CAMPBELL_SLUG}/file/{bad_id}",
                srv.base_url
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            reqwest::StatusCode::FORBIDDEN,
            "expected 403 FORBIDDEN for `{bad_id}`"
        );
    }

    let _ = probe_tx.send(());
    assert_eq!(
        tcp_accepts.load(Ordering::SeqCst),
        0,
        "0 TCP connections must be made to blocked loopback/private hosts"
    );
}

#[tokio::test]
async fn upload_photos_and_360_panoramas_with_hotspot_persistence_and_byte_roundtrip() {
    let srv = TestServerHandle::start(true, Vec::new()).await;
    let client = test_http_client();

    srv.state
        .db
        .put_cached_report(
            CAMPBELL_SLUG,
            &load_real_report_with_media(CAMPBELL_SLUG, vec![]),
            None,
        )
        .await
        .unwrap();

    // 1. Cross-origin POST must be blocked by `verify_same_origin`
    let normal_png = make_test_png(800, 600);
    let b64_normal = base64::engine::general_purpose::STANDARD.encode(&normal_png);
    let cross_origin_resp = client
        .post(format!("{}/api/media/{CAMPBELL_SLUG}/upload", srv.base_url))
        .header("Origin", "https://attacker.example")
        .json(&json!({
            "filename": "front.png",
            "dataBase64": b64_normal,
            "category": "exterior"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin_resp.status(), reqwest::StatusCode::FORBIDDEN);

    // 2. Upload normal 800x600 PNG -> `isPano360: false`
    let up_normal_resp = client
        .post(format!("{}/api/media/{CAMPBELL_SLUG}/upload", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "filename": "front_yard.png",
            "dataBase64": format!("data:image/png;base64,{b64_normal}"),
            "roomTag": "Front Yard",
            "caption": "Morning Curb View",
            "category": "exterior",
            "isPano360": null
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(up_normal_resp.status(), reqwest::StatusCode::OK);
    let up_normal_json: Value = up_normal_resp.json().await.unwrap();
    assert_eq!(up_normal_json["ok"], json!(true));
    assert_eq!(up_normal_json["item"]["width"], json!(800));
    assert_eq!(up_normal_json["item"]["height"], json!(600));
    assert_eq!(up_normal_json["item"]["isPano360"], json!(false));
    let normal_id = up_normal_json["item"]["id"].as_str().unwrap().to_string();

    // 3. Upload 1600x800 (2:1) equirectangular PNG -> auto-detects `isPano360: true`
    let pano_living_png = make_test_png(1600, 800);
    let up_living_resp = client
        .post(format!("{}/api/media/{CAMPBELL_SLUG}/upload", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "filename": "living_room_360.png",
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&pano_living_png),
            "roomTag": "Living Room",
            "category": "staging",
            "isPano360": null
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(up_living_resp.status(), reqwest::StatusCode::OK);
    let up_living_json: Value = up_living_resp.json().await.unwrap();
    assert_eq!(up_living_json["item"]["width"], json!(1600));
    assert_eq!(up_living_json["item"]["height"], json!(800));
    assert_eq!(up_living_json["item"]["isPano360"], json!(true));
    let living_id = up_living_json["item"]["id"].as_str().unwrap().to_string();

    // 4. Upload 2000x1000 (2:1) equirectangular JPEG -> auto-detects `isPano360: true`
    let pano_kitchen_jpg = make_test_jpeg(2000, 1000, None);
    let up_kitchen_resp = client
        .post(format!("{}/api/media/{CAMPBELL_SLUG}/upload", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "filename": "kitchen_360.jpg",
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&pano_kitchen_jpg),
            "roomTag": "Kitchen",
            "category": "staging"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(up_kitchen_resp.status(), reqwest::StatusCode::OK);
    let up_kitchen_json: Value = up_kitchen_resp.json().await.unwrap();
    assert_eq!(up_kitchen_json["item"]["width"], json!(2000));
    assert_eq!(up_kitchen_json["item"]["height"], json!(1000));
    assert_eq!(up_kitchen_json["item"]["isPano360"], json!(true));
    let kitchen_id = up_kitchen_json["item"]["id"].as_str().unwrap().to_string();

    // 5. Upload 1200x900 non-2:1 JPEG with GPano XMP metadata + WebP 2:1 -> both `isPano360: true`
    let xmp_jpg = make_test_jpeg(
        1200,
        900,
        Some(b"<rdf:Description GPano:ProjectionType=\"equirectangular\"/>"),
    );
    let up_xmp_resp = client
        .post(format!("{}/api/media/{CAMPBELL_SLUG}/upload", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "filename": "theta_xmp.jpg",
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&xmp_jpg),
            "roomTag": "Backyard Patio"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(up_xmp_resp.status(), reqwest::StatusCode::OK);
    let up_xmp_json: Value = up_xmp_resp.json().await.unwrap();
    assert_eq!(up_xmp_json["item"]["isPano360"], json!(true));
    let patio_id = up_xmp_json["item"]["id"].as_str().unwrap().to_string();

    let pano_webp = make_test_webp(1920, 960);
    let up_webp_resp = client
        .post(format!("{}/api/media/{CAMPBELL_SLUG}/upload", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "filename": "hallway.webp",
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(&pano_webp),
            "roomTag": "Hallway"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(up_webp_resp.status(), reqwest::StatusCode::OK);
    let up_webp_json: Value = up_webp_resp.json().await.unwrap();
    assert_eq!(up_webp_json["item"]["isPano360"], json!(true));

    // Reject non-image payload
    let bad_img_resp = client
        .post(format!("{}/api/media/{CAMPBELL_SLUG}/upload", srv.base_url))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "filename": "fake.jpg",
            "dataBase64": base64::engine::general_purpose::STANDARD.encode(b"<html>not an image</html>")
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_img_resp.status(), reqwest::StatusCode::BAD_REQUEST);

    // 6. Save room-to-room hotspot links (`POST /api/media/:slug/hotspots`)
    let hs_resp = client
        .post(format!(
            "{}/api/media/{CAMPBELL_SLUG}/hotspots",
            srv.base_url
        ))
        .header("Origin", &srv.base_url)
        .json(&json!({
            "fromId": living_id,
            "hotspots": [
                { "yaw": 38.5, "pitch": -3.2, "targetId": kitchen_id, "label": "Kitchen" },
                { "yaw": -142.0, "pitch": -5.0, "targetId": patio_id, "label": "Backyard Patio" }
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(hs_resp.status(), reqwest::StatusCode::OK);
    let hs_json: Value = hs_resp.json().await.unwrap();
    assert_eq!(hs_json["ok"], json!(true));
    assert_eq!(hs_json["hotspots"].as_array().unwrap().len(), 2);

    // Verify manifest reload preserves uploads, panos list, and hotspots
    let mf_resp = client
        .get(format!(
            "{}/api/media/{CAMPBELL_SLUG}/manifest",
            srv.base_url
        ))
        .send()
        .await
        .unwrap();
    let mf_raw = mf_resp.text().await.unwrap();
    assert_clean_text("uploaded manifest", &mf_raw);
    let mf: Value = serde_json::from_str(&mf_raw).unwrap();
    assert_eq!(mf["items"].as_array().unwrap().len(), 5);
    assert_eq!(mf["panos"].as_array().unwrap().len(), 4);

    let living_manifest_entry = mf["panos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"].as_str() == Some(living_id.as_str()))
        .expect("living room pano in panos");
    assert_eq!(
        living_manifest_entry["hotspots"],
        json!([
            { "yaw": 38.5, "pitch": -3.2, "targetId": kitchen_id, "label": "Kitchen" },
            { "yaw": -142.0, "pitch": -5.0, "targetId": patio_id, "label": "Backyard Patio" }
        ])
    );

    // 7. Verify exact byte-for-byte round-trip via `GET /api/media/:slug/file/:id`
    for (id, expected_bytes, expected_mime) in [
        (&normal_id, normal_png.as_slice(), "image/png"),
        (&living_id, pano_living_png.as_slice(), "image/png"),
        (&kitchen_id, pano_kitchen_jpg.as_slice(), "image/jpeg"),
        (&patio_id, xmp_jpg.as_slice(), "image/jpeg"),
    ] {
        let file_resp = client
            .get(format!(
                "{}/api/media/{CAMPBELL_SLUG}/file/{id}",
                srv.base_url
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(file_resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            file_resp
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            expected_mime
        );
        assert_eq!(
            file_resp
                .headers()
                .get("cache-control")
                .unwrap()
                .to_str()
                .unwrap(),
            "public, max-age=604800, immutable"
        );
        let actual = file_resp.bytes().await.unwrap();
        assert_eq!(actual.as_ref(), expected_bytes);
    }
}

#[tokio::test]
async fn tour_endpoint_accepts_allowlisted_3d_hosts_and_rejects_portals_and_unknown_domains() {
    let srv = TestServerHandle::start(true, Vec::new()).await;
    let client = test_http_client();

    srv.state
        .db
        .put_cached_report(
            CAMPBELL_SLUG,
            &load_real_report_with_media(CAMPBELL_SLUG, vec![]),
            None,
        )
        .await
        .unwrap();

    let valid_tours = [
        (
            "https://my.matterport.com/show/?m=SxQL3iGyoDo",
            "Matterport",
        ),
        (
            "https://youriguide.com/200_synthparcelbeta_ave_campbell_ca",
            "iGUIDE",
        ),
        ("https://kuula.co/share/collection/7kP9Q", "Kuula"),
        ("https://app.cloudpano.com/tours/t987654", "CloudPano"),
        ("https://view.ricoh360.com/tours/r123456", "RICOH360"),
        ("https://giraffe360.com/tour/g123456", "Giraffe360"),
        ("https://players.cupix.com/p/c123456", "Cupix"),
    ];

    for (url, expected_vendor) in valid_tours {
        let resp = client
            .post(format!("{}/api/media/{CAMPBELL_SLUG}/tour", srv.base_url))
            .header("Origin", &srv.base_url)
            .json(&json!({ "url": url }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "expected allowlisted tour `{url}` to succeed"
        );
        let body_raw = resp.text().await.unwrap();
        assert_clean_text("POST /api/media/:slug/tour", &body_raw);
        let body: Value = serde_json::from_str(&body_raw).unwrap();
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["tour"]["vendor"], json!(expected_vendor));
    }

    let rejected_urls = [
        format!(
            "https://www.{}{}.com/view-3d-home/12345678-1234-1234-1234-1234567890ab",
            "zil", "low"
        ),
        format!("https://www.{}{}.com/CA/Campbell/home/123", "red", "fin"),
        format!("https://www.{}{}.com/listing/123", "comp", "ass"),
        "http://my.matterport.com/show/?m=SxQL3iGyoDo".to_string(),
        "https://evil-phishing.example.com/show/?m=SxQL3iGyoDo".to_string(),
        "javascript:alert(1)".to_string(),
    ];

    for bad_url in rejected_urls {
        let resp = client
            .post(format!("{}/api/media/{CAMPBELL_SLUG}/tour", srv.base_url))
            .header("Origin", &srv.base_url)
            .json(&json!({ "url": bad_url }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "expected `{bad_url}` to be rejected with 400"
        );
        let err_raw = resp.text().await.unwrap();
        assert_clean_text("rejected tour error payload", &err_raw);
    }

    let mf: Value = client
        .get(format!(
            "{}/api/media/{CAMPBELL_SLUG}/manifest",
            srv.base_url
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(mf["tours"].as_array().unwrap().len(), 7);
}

#[tokio::test]
async fn rendered_report_html_contains_walkin_viewer_and_zero_forbidden_tokens() {
    assert_clean_text("assets/walkin_media.js", WALKIN_MEDIA_JS);
    assert!(
        !WALKIN_MEDIA_JS.to_ascii_lowercase().contains("</script"),
        "assets/walkin_media.js must never contain literal `</script`"
    );

    let srv = TestServerHandle::start(true, Vec::new()).await;
    let client = test_http_client();

    let rep = load_real_report_with_media(CAMPBELL_SLUG, vec![]);
    let rendered = render_report_html(&rep);
    srv.state
        .db
        .put_cached_report(CAMPBELL_SLUG, &rep, Some(&rendered))
        .await
        .unwrap();

    let resp = client
        .get(format!("{}/r/{CAMPBELL_SLUG}/", srv.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let html = resp.text().await.unwrap();

    for expected_marker in [
        "__OPEN_ESTATE_WALKIN_MEDIA_INIT__",
        "heroWalkinStage",
        "mediaTabWalkin",
        "SphereGeometry",
        "3D Walk-In",
        "三维实景漫游",
        "Recorrido 3D",
        "+ Upload Photos / 360°",
    ] {
        assert!(
            html.contains(expected_marker),
            "Rendered `/r/{CAMPBELL_SLUG}/` HTML missing expected Walk-In marker `{expected_marker}`"
        );
    }

    let lower_html = html.to_ascii_lowercase();
    for tok in forbidden_tech_tokens() {
        assert!(
            !lower_html.contains(&tok.to_ascii_lowercase()),
            "Rendered `/r/{CAMPBELL_SLUG}/` HTML leaked forbidden token `{tok}`"
        );
    }
}
