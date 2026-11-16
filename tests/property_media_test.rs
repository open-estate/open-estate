//! Integration & unit-level contract tests for `open_estate::property::media` using procedural
//! synthetic reports and synthetic holdout parcels:
//! - `ensure_property_media_on_report(&mut serde_json::Value)`
//! - `build_deterministic_property_media(&serde_json::Value)`
//! - `extract_photos_from_clean_html(...)` & `is_antibot_or_challenge_html(...)`
//! - `enrich_property_media(&reqwest::Client, &mut serde_json::Value)` & `enrich_property_media_with_config(...)`

mod common;

use open_estate::property::media::{
    attach_satellite_meta_to_report, build_deterministic_property_media,
    build_deterministic_property_media_with_env, enrich_property_media,
    enrich_property_media_with_config, ensure_property_media_on_report,
    ensure_property_media_on_report_with_env, extract_photos_from_clean_html,
    is_antibot_or_challenge_html, resolve_regional_ortho, resolve_satellite_source,
    MediaEnrichConfig, RegionalOrthoRegion, SatelliteProviderKind,
};
use open_estate::shared::EnvConfig;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const ALLOWED_CATEGORIES: &[&str] = &["staging", "exterior", "street", "aerial", "parcel"];

fn fixture_report(slug: &str) -> Value {
    common::synth_report_for_slug(slug)
}

fn holdout_home_by_id(id: &str) -> Value {
    common::synth_holdout_by_id(id)
}

fn forbidden_tokens() -> Vec<String> {
    vec![
        format!("{}{}", "prov", "ider"),
        format!("{}{}", "prov", "iders"),
        format!("{}{}", "eff", "ort"),
        format!("{}{}", "gpt-custom-", "primary"),
        format!("{}{}", "gpt-custom-", "fallback"),
        format!("{}{}", "fallback", "Used"),
        format!("{}{}", "Home", "Lens"),
        format!("{}{}", "home", "lens"),
    ]
}

fn assert_no_forbidden_tokens(val: &Value) {
    let raw = serde_json::to_string(val).expect("serialize JSON");
    let low = raw.to_ascii_lowercase();
    for tok in forbidden_tokens() {
        assert!(
            !low.contains(&tok.to_ascii_lowercase()),
            "propertyMedia leaked forbidden substring `{tok}` in: {raw}"
        );
    }
}

fn assert_valid_property_media_schema(pm: &Value) {
    let obj = pm.as_object().expect("propertyMedia must be an object");
    let has_sale = obj
        .get("hasSalePhotos")
        .and_then(Value::as_bool)
        .expect("hasSalePhotos bool");
    assert!(obj.contains_key("lastSaleDate"), "missing lastSaleDate key");
    assert!(
        obj.contains_key("lastSalePrice"),
        "missing lastSalePrice key"
    );

    let status = obj
        .get("mediaStatus")
        .and_then(Value::as_str)
        .expect("mediaStatus string");
    if has_sale {
        assert_eq!(status, "sale_photos_found");
    } else {
        assert_eq!(status, "exterior_archive_only");
    }

    let prov = obj
        .get("provenanceNote")
        .and_then(Value::as_str)
        .expect("provenanceNote string");
    assert!(!prov.trim().is_empty());

    let items = obj
        .get("items")
        .and_then(Value::as_array)
        .expect("items array");
    for it in items {
        let io = it.as_object().expect("item object");
        for k in [
            "id",
            "url",
            "thumbUrl",
            "category",
            "roomTag",
            "caption",
            "capturedAt",
            "sourceName",
            "sourceUrl",
        ] {
            let s = io
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("missing string key `{k}` on item {it}"));
            assert!(!s.trim().is_empty(), "empty `{k}` on item {it}");
        }
        let cat = io.get("category").and_then(Value::as_str).unwrap();
        assert!(
            ALLOWED_CATEGORIES.contains(&cat),
            "unexpected category `{cat}`"
        );
        assert!(
            io.get("isHistoricalSale")
                .and_then(Value::as_bool)
                .is_some(),
            "missing bool isHistoricalSale"
        );
    }

    let any_hist = items
        .iter()
        .any(|i| i.get("isHistoricalSale").and_then(Value::as_bool) == Some(true));
    assert_eq!(any_hist, has_sale);

    let links = obj
        .get("externalGalleryLinks")
        .and_then(Value::as_array)
        .expect("externalGalleryLinks array");
    assert!(!links.is_empty());
    for lk in links {
        for k in ["label", "url", "badge"] {
            let s = lk
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("missing `{k}` on externalGalleryLink {lk}"));
            assert!(!s.trim().is_empty());
        }
    }

    assert_no_forbidden_tokens(pm);
}

#[test]
fn deterministic_backfill_synthetic_secondary_and_primary_fixtures_schema_and_idempotency() {
    let empty_env = EnvConfig::from_map(HashMap::new());
    let mut secondary = fixture_report(common::SYNTH_SECONDARY_SLUG);
    if let Some(obj) = secondary.as_object_mut() {
        obj.remove("propertyMedia");
    }

    let direct_pm =
        serde_json::to_value(build_deterministic_property_media_with_env(&secondary, &empty_env))
            .expect("serialize pm");
    ensure_property_media_on_report_with_env(&mut secondary, &empty_env);
    let pm = secondary
        .get("propertyMedia")
        .expect("propertyMedia populated on secondary synthetic report");
    assert_valid_property_media_schema(pm);
    assert_eq!(pm, &direct_pm);

    assert_eq!(
        pm.get("lastSaleDate").and_then(Value::as_str),
        Some("2021-05-20")
    );
    assert_eq!(
        pm.get("lastSalePrice").and_then(Value::as_f64),
        Some(2_050_000.0)
    );

    // Secondary synthetic fixture (Campbell, CA) resolves to Santa Clara Valley 2025 0.1m ortho
    let sat = &secondary["meta"]["satellite"];
    assert_eq!(sat["source"], json!("scc_sj_ortho_2025"));
    assert_eq!(sat["maxNativeZoom"], json!(21));
    assert_eq!(sat["isHighRes"], json!(true));
    assert_eq!(sat["proxied"], json!(false));

    let items = pm.get("items").and_then(Value::as_array).unwrap();
    assert!(
        items.len() >= 5,
        "expected >=5 deterministic aerial/parcel/topo cards, got {}",
        items.len()
    );
    for required_id in [
        "scv-ortho-2025-rooftop",
        "scv-ortho-2025-lot",
        "scv-ortho-2025-neighborhood",
        "usgs-3dep-hillshade",
        "usgs-street-topo",
    ] {
        assert!(
            items
                .iter()
                .any(|i| i.get("id").and_then(Value::as_str) == Some(required_id)),
            "missing `{required_id}` card in secondary synthetic media"
        );
    }

    let links = pm
        .get("externalGalleryLinks")
        .and_then(Value::as_array)
        .unwrap();
    assert!(links.len() >= 4);
    assert!(links.iter().any(|l| {
        l.get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("cbll=37.282000,-121.954000")
    }));
    assert!(links.iter().any(|l| {
        l.get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("cp=37.282000~-121.954000&lvl=19&style=o")
    }));

    // Verify strict idempotency on repeated backfill calls.
    let snapshot_first = secondary["propertyMedia"].clone();
    ensure_property_media_on_report_with_env(&mut secondary, &empty_env);
    assert_eq!(secondary["propertyMedia"], snapshot_first);

    // Also verify primary synthetic report (`100 Synthparcelalpha Ln`).
    let mut primary = fixture_report(common::SYNTH_PRIMARY_SLUG);
    if let Some(obj) = primary.as_object_mut() {
        obj.remove("propertyMedia");
    }
    ensure_property_media_on_report_with_env(&mut primary, &empty_env);
    let primary_pm = primary
        .get("propertyMedia")
        .expect("propertyMedia populated on primary synthetic report");
    assert_valid_property_media_schema(primary_pm);
    assert_eq!(
        primary_pm.get("lastSaleDate").and_then(Value::as_str),
        Some("2019-06-15")
    );
    assert_eq!(
        primary_pm.get("lastSalePrice").and_then(Value::as_f64),
        Some(1_480_000.0)
    );
}

#[test]
fn structured_mls_staging_photos_classified_and_scrubbed_on_synth_fixture() {
    let tricky_word = format!(
        "{}{}less indoor-outdoor flow by top {}{}",
        "eff", "ort", "prov", "ider"
    );
    let mut report = fixture_report(common::SYNTH_PRIMARY_SLUG);
    if let Some(obj) = report.as_object_mut() {
        obj.remove("propertyMedia");
    }
    report["facts"]["mlsListing"] = json!({
        "photos": [
            {
                "url": "https://archive.synthmedia.example/photos/ML00000100_0.jpg",
                "caption": "Stone and stucco front facade and porch on Synthparcelalpha Ln"
            },
            {
                "url": "https://archive.synthmedia.example/photos/ML00000100_1.jpg",
                "caption": format!("Remodeled quartz kitchen island with {tricky_word}")
            },
            {
                "url": "https://archive.synthmedia.example/photos/ML00000100_2.jpg",
                "room": "Living Room",
                "caption": "Staged living room with stone fireplace"
            },
            {
                "url": "https://archive.synthmedia.example/photos/ML00000100_3.jpg",
                "caption": "Ground-floor primary bedroom suite"
            }
        ]
    });

    ensure_property_media_on_report(&mut report);
    let pm = &report["propertyMedia"];
    assert_valid_property_media_schema(pm);

    assert_eq!(pm.get("hasSalePhotos").and_then(Value::as_bool), Some(true));
    assert_eq!(
        pm.get("mediaStatus").and_then(Value::as_str),
        Some("sale_photos_found")
    );
    assert_eq!(
        pm.get("lastSaleDate").and_then(Value::as_str),
        Some("2019-06-15")
    );
    assert_eq!(
        pm.get("lastSalePrice").and_then(Value::as_f64),
        Some(1_480_000.0)
    );

    let items = pm.get("items").and_then(Value::as_array).unwrap();
    let sale_items: Vec<&Value> = items
        .iter()
        .filter(|i| i.get("isHistoricalSale").and_then(Value::as_bool) == Some(true))
        .collect();
    let archive_items: Vec<&Value> = items
        .iter()
        .filter(|i| i.get("isHistoricalSale").and_then(Value::as_bool) == Some(false))
        .collect();
    assert_eq!(sale_items.len(), 4);
    assert!(archive_items.len() >= 5);

    assert_eq!(
        sale_items[0].get("category").and_then(Value::as_str),
        Some("exterior")
    );
    assert_eq!(
        sale_items[0].get("roomTag").and_then(Value::as_str),
        Some("Front Exterior")
    );
    assert_eq!(
        sale_items[1].get("category").and_then(Value::as_str),
        Some("staging")
    );
    assert_eq!(
        sale_items[1].get("roomTag").and_then(Value::as_str),
        Some("Kitchen")
    );
    assert_eq!(
        sale_items[2].get("roomTag").and_then(Value::as_str),
        Some("Living Room")
    );
    assert_eq!(
        sale_items[3].get("roomTag").and_then(Value::as_str),
        Some("Bedroom")
    );
}

#[test]
fn clean_html_json_ld_og_twitter_extraction_and_svg_exclusion() {
    let html = r#"<!doctype html>
    <html>
    <head>
      <title>200 Synthparcelbeta Ave, Campbell, CA 95008 | MLS #ML00000200</title>
      <meta property="og:image" content="https://archive.synthmedia.example/photos/ML00000200_0.jpg" />
      <meta property="og:image:alt" content="Front curb elevation and porch" />
      <meta name="twitter:image" content="https://archive.synthmedia.example/photos/ML00000200_4.jpg" />
      <meta property="og:image" content="https://archive.synthmedia.example/assets/brand-logo.svg" />
      <script type="application/ld+json">
      {
        "@context": "https://schema.org",
        "@graph": [
          {
            "@type": ["SingleFamilyResidence", "RealEstateListing", "Product"],
            "name": "200 Synthparcelbeta Ave, Campbell, CA 95008",
            "photos": [
              {
                "@type": "Photograph",
                "name": "Remodeled Chef Kitchen & Range Island",
                "image": {
                  "@type": "ImageObject",
                  "contentUrl": "https://archive.synthmedia.example/photos/ML00000200_1.jpg",
                  "thumbnailUrl": "https://archive.synthmedia.example/photos/thumb_ML00000200_1.jpg",
                  "uploadDate": "2021-05-19"
                }
              },
              {
                "@type": "ImageObject",
                "url": "https://archive.synthmedia.example/photos/ML00000200_2.jpg",
                "caption": "Staged Living Room with Wood-Burning Fireplace"
              },
              {
                "@type": "ImageObject",
                "url": "https://archive.synthmedia.example/photos/ML00000200_3.jpg",
                "caption": "Rear Backyard & Pergola Patio"
              }
            ]
          }
        ]
      }
      </script>
    </head>
    <body>Clean property detail page</body>
    </html>"#;

    let extracted = extract_photos_from_clean_html(
        html,
        "https://archive.synthmedia.example/listings/200-synthparcelbeta-ave",
        Some("Public Listing Archive"),
        Some("2021-05-20"),
    );

    assert_eq!(
        extracted.len(),
        5,
        "Expected 3 JSON-LD + og:image + twitter:image (SVG excluded)"
    );
    assert!(extracted.iter().all(|p| p.is_historical_sale));
    assert!(extracted
        .iter()
        .any(|p| p.room_tag == "Kitchen" && p.captured_at == "2021-05-19"));
    assert!(extracted.iter().any(|p| p.room_tag == "Living Room"));
    assert!(extracted.iter().any(|p| p.room_tag == "Yard & Patio"));
    assert!(!extracted.iter().any(|p| p.url.contains(".svg")));

    let serialized = serde_json::to_value(&extracted).expect("serialize extracted items");
    assert_no_forbidden_tokens(&serialized);
}

#[test]
fn antibot_challenge_html_detection_never_extracts_gated_pages() {
    let cf_html = r#"<html><head><title>Just a moment...</title>
        <meta property="og:image" content="https://archive.synthmedia.example/photos/ML00000200_0.jpg" />
        </head><body><div class="cf-turnstile"></div></body></html>"#;
    let px_html = r#"<html><head>
        <meta property="og:image" content="https://archive.synthmedia.example/photos/ML00000200_0.jpg" />
        </head><body><div id="px-captcha">Verify you are human</div></body></html>"#;
    let hcaptcha_html = r#"<html><head><title>Security Check</title></head>
        <body><script src="https://hcaptcha.com/1/api.js"></script></body></html>"#;

    for gated in [cf_html, px_html, hcaptcha_html] {
        assert!(is_antibot_or_challenge_html(gated));
        let photos = extract_photos_from_clean_html(
            gated,
            "https://archive.synthmedia.example/listings/200-synthparcelbeta-ave",
            None,
            Some("2021-05-20"),
        );
        assert!(
            photos.is_empty(),
            "Gated challenge HTML must immediately return zero extracted photos"
        );
    }
}

#[tokio::test]
async fn enrich_property_media_and_assessor_plat_on_synth_holdout_homes() {
    let philly = holdout_home_by_id("synth-pa-19147-rowhouse");
    let mut philly_report = json!({
        "meta": { "displayAddress": philly["address"] },
        "geo": {
            "lat": philly["raw"]["geo"]["lat"],
            "lon": philly["raw"]["geo"]["lon"]
        },
        "facts": {
            "yearBuilt": philly["raw"]["facts"]["yearBuilt"],
            "saleHistory": [
                {
                    "date": philly["targetSaleDate"],
                    "price": philly["targetSalePrice"],
                    "event": "Sold"
                }
            ]
        }
    });

    let mut cfg = MediaEnrichConfig::from_env();
    cfg.request_timeout = Duration::from_millis(2_500);
    cfg.mapillary_access_token = None;
    cfg.enable_wikimedia = false;

    let client = reqwest::Client::new();
    let t0 = Instant::now();
    enrich_property_media_with_config(&client, &mut philly_report, &cfg).await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "Polite media enrichment must remain bounded, took {elapsed:?}"
    );

    let philly_pm = &philly_report["propertyMedia"];
    assert_valid_property_media_schema(philly_pm);
    assert_eq!(
        philly_pm.get("lastSaleDate").and_then(Value::as_str),
        philly["targetSaleDate"].as_str()
    );
    assert!(
        philly_pm
            .get("items")
            .and_then(Value::as_array)
            .unwrap()
            .len()
            >= 5
    );

    // 2. Synthetic Evanston, IL holdout parcel (`synth-il-60201-vintage-sfr`) exercising County
    // Assessor plat map, slash-date normalization, and `enrich_property_media(&client, &mut report)`.
    let evanston = holdout_home_by_id("synth-il-60201-vintage-sfr");
    let mut evanston_report = json!({
        "meta": { "displayAddress": evanston["address"] },
        "geo": {
            "lat": evanston["raw"]["geo"]["lat"],
            "lon": evanston["raw"]["geo"]["lon"]
        },
        "facts": {
            "apn": "000-00-400",
            "zpid": 10000400,
            "photos": ["https://archive.synthmedia.example/fp/synth-60201-front.jpg"],
            "saleHistory": [{
                "date": "05/12/2021",
                "price": evanston["targetSalePrice"],
                "event": "Sold"
            }],
            "countyRecords": {
                "parcelIdentity": {
                    "assessorMapImage": "https://maps.synthcounty.example/plats/000-00-400.png"
                }
            }
        }
    });

    enrich_property_media(&client, &mut evanston_report).await;
    let ev_pm = &evanston_report["propertyMedia"];
    assert_valid_property_media_schema(ev_pm);
    assert_eq!(
        ev_pm.get("hasSalePhotos").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        ev_pm.get("lastSaleDate").and_then(Value::as_str),
        Some("2021-05-12")
    );

    let ev_items = ev_pm.get("items").and_then(Value::as_array).unwrap();
    assert!(ev_items.iter().any(|i| {
        i.get("roomTag").and_then(Value::as_str) == Some("Assessor Plat")
            && i.get("isHistoricalSale").and_then(Value::as_bool) == Some(false)
    }));

    let ev_links = ev_pm
        .get("externalGalleryLinks")
        .and_then(Value::as_array)
        .unwrap();
    assert!(ev_links.len() >= 4);

    // Minimal empty object report must not panic and must produce valid schema.
    let mut empty_report = json!({});
    ensure_property_media_on_report(&mut empty_report);
    assert_valid_property_media_schema(&empty_report["propertyMedia"]);
}

#[test]
fn satellite_resolution_hierarchy_regional_ortho_and_zero_secret_leak() {
    let empty_env = EnvConfig::from_map(HashMap::new());
    // Regional bounds checks
    assert_eq!(
        resolve_regional_ortho(37.282354, -121.961661),
        Some(RegionalOrthoRegion::SantaClaraSanJose)
    );
    assert_eq!(
        resolve_regional_ortho(47.6062, -122.3321),
        Some(RegionalOrthoRegion::KingCountyWa)
    );
    // San Francisco & Philadelphia fall outside regional agency bounds -> USGS nationwide fallback
    assert_eq!(resolve_regional_ortho(37.7749, -122.4194), None);
    assert_eq!(resolve_regional_ortho(39.9400, -75.1500), None);

    // King County keyless parcel emits `kingco-ortho-2023-*` + `kingco_ortho_2023`
    let mut seattle_report = json!({
        "meta": { "displayAddress": "100 Synthlakeview Way, Seattle, WA 98101" },
        "geo": { "lat": 47.6062, "lon": -122.3321 }
    });
    ensure_property_media_on_report_with_env(&mut seattle_report, &empty_env);
    assert_valid_property_media_schema(&seattle_report["propertyMedia"]);
    assert_eq!(seattle_report["meta"]["satellite"]["source"], json!("kingco_ortho_2023"));
    assert_eq!(seattle_report["meta"]["satellite"]["maxNativeZoom"], json!(20));
    let sea_ids: Vec<&str> = seattle_report["propertyMedia"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert!(sea_ids.contains(&"kingco-ortho-2023-rooftop"));
    assert!(sea_ids.contains(&"kingco-ortho-2023-lot"));

    // Outside regional agency bounds falls back cleanly to USGSImageryOnly
    let mut philly_report = json!({
        "meta": { "displayAddress": "200 Synthrowhouse Ln, Philadelphia, PA 19106" },
        "geo": { "lat": 39.944, "lon": -75.146 }
    });
    ensure_property_media_on_report_with_env(&mut philly_report, &empty_env);
    assert_valid_property_media_schema(&philly_report["propertyMedia"]);
    assert_eq!(philly_report["meta"]["satellite"]["source"], json!("usgs_imagery_only"));
    assert_eq!(philly_report["meta"]["satellite"]["maxNativeZoom"], json!(16));
    let phl_ids: Vec<&str> = philly_report["propertyMedia"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert!(phl_ids.contains(&"usgs-aerial-rooftop"));

    // Legacy cached Campbell report holding old LOD-16 USGS cards upgrades automatically to 2025 ortho
    let mut legacy_campbell = json!({
        "meta": { "displayAddress": "200 Synthparcelbeta Ave, Campbell, CA 95008" },
        "geo": { "lat": 37.282354, "lon": -121.961661 },
        "propertyMedia": {
            "hasSalePhotos": false,
            "lastSaleDate": null,
            "lastSalePrice": null,
            "mediaStatus": "exterior_archive_only",
            "provenanceNote": "Legacy Esri note",
            "items": [
                {
                    "id": "usgs-aerial-rooftop",
                    "url": "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?bbox=-121.96,-1,1,1&size=960,640&format=jpg&f=image",
                    "thumbUrl": "https://basemap.nationalmap.gov/arcgis/rest/services/USGSImageryOnly/MapServer/export?size=480,320",
                    "category": "aerial",
                    "roomTag": "House Rooftop",
                    "caption": "Old blurry aerial",
                    "capturedAt": "USGS",
                    "sourceName": "USGS",
                    "sourceUrl": "https://apps.nationalmap.gov/viewer/",
                    "isHistoricalSale": false
                }
            ],
            "externalGalleryLinks": [
                { "label": "Street", "url": "https://www.google.com/maps", "badge": "Street" }
            ]
        }
    });
    ensure_property_media_on_report_with_env(&mut legacy_campbell, &empty_env);
    let up_ids: Vec<&str> = legacy_campbell["propertyMedia"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["id"].as_str())
        .collect();
    assert!(up_ids.contains(&"scv-ortho-2025-rooftop"));
    assert!(!up_ids.contains(&"usgs-aerial-rooftop"));

    // Commercial keys emit proxied non-secret `/api/media/satellite/*` URLs and never leak secret values
    for (env_key, secret_val, expected_kind, expected_id_prefix, expected_prefix) in [
        ("ARCGIS_API_KEY", "SECRET_ARCGIS_TOKEN_XYZ987", SatelliteProviderKind::ArcGisCommercial, "sat-arcgis-rooftop", "arcgis_world_imagery"),
        ("MAPBOX_ACCESS_TOKEN", "pk.SECRET_MAPBOX_TOK_987", SatelliteProviderKind::MapboxCommercial, "sat-mapbox-rooftop", "mapbox_satellite"),
        ("GOOGLE_MAPS_API_KEY", "AIzaSECRET_GOOGLE_KEY_987", SatelliteProviderKind::GoogleCommercial, "sat-google-rooftop", "google_satellite"),
    ] {
        let env = EnvConfig::from_map(HashMap::from([(env_key.to_string(), secret_val.to_string())]));
        let resolved = resolve_satellite_source(&env, Some((37.282354, -121.961661)));
        assert_eq!(resolved.provider, expected_kind);
        assert!(resolved.proxied);

        let mut rep = json!({
            "meta": { "displayAddress": "200 Synthparcelbeta Ave, Campbell, CA 95008" },
            "geo": { "lat": 37.282354, "lon": -121.961661 }
        });
        attach_satellite_meta_to_report(&mut rep, &env);
        ensure_property_media_on_report_with_env(&mut rep, &env);

        assert_valid_property_media_schema(&rep["propertyMedia"]);
        assert_eq!(rep["meta"]["satellite"]["source"], json!(expected_prefix));
        assert_eq!(rep["meta"]["satellite"]["proxied"], json!(true));
        assert_eq!(
            rep["meta"]["satellite"]["tileUrl"],
            json!("/api/media/satellite/tile/{z}/{y}/{x}")
        );

        let items = rep["propertyMedia"]["items"].as_array().unwrap();
        let rooftop = items.iter().find(|i| i["id"] == expected_id_prefix).expect("rotated commercial rooftop card");
        let rooftop_url = rooftop["url"].as_str().unwrap();
        assert!(rooftop_url.starts_with("/api/media/satellite/export?"));
        assert!(rooftop_url.contains("w=1600&h=1080"));
        assert!(rooftop["thumbUrl"].as_str().unwrap().contains("w=960&h=648"));
        let full_text = serde_json::to_string(&rep).unwrap();
        assert!(!full_text.contains(secret_val), "secret `{env_key}` must never appear in report JSON");
        assert!(!full_text.contains("server.arcgisonline.com"));

        // Sanitize / empty-env backfill must never downgrade proxied commercial metadata or cards
        ensure_property_media_on_report_with_env(&mut rep, &empty_env);
        assert_eq!(rep["meta"]["satellite"]["proxied"], json!(true));
        assert!(rep["propertyMedia"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == expected_id_prefix));
    }

    let _ = build_deterministic_property_media(&seattle_report);
}
