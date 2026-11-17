// Unit & integration tests for county parcel records, collinear polygon edge merging,
// PII sanitization, W3C `#:~:text=` deep anchors, multi-jurisdiction official portal routing,
// CSV public-record enrichment, and report HTML county-records rendering — 100% synthetic data.

mod common;

use open_estate::property::facts::{
    build_county_records_dossier, ensure_county_records_on_report,
};
use open_estate::property::mls::parse_public_gis_csv_all;
use open_estate::property::public_records::{
    build_arcgis_parcel_html_query_url, compute_lot_dimensions, is_coarse_acre_backcalc,
    is_two_decimal_acres, resolve_official_record_links, sanitize_county_raw_attributes,
    with_data_point_anchor,
};
use open_estate::web::render::{normalize_stored_report, render_report_html};
use serde_json::{json, Map};

#[test]
fn detects_two_decimal_acre_backcalc_artifact() {
    // USABLE_SQ_FT_LAN = 9,900 (0.22727 ac) -> truncated ACRES = 0.22 -> LOTSIZE = 9,583.2
    assert!(is_two_decimal_acres(0.22));
    assert!(is_coarse_acre_backcalc(9583, Some(0.22)));
    assert!(!is_coarse_acre_backcalc(9900, Some(0.22)));
    assert!(!is_coarse_acre_backcalc(10111, Some(0.22)));
    // High-precision 4-decimal acreage is NOT flagged as coarse 2-decimal truncation
    assert!(!is_two_decimal_acres(0.2273));
    assert!(!is_coarse_acre_backcalc(9901, Some(0.2273)));
}

#[test]
fn merges_collinear_split_edges_for_synth_parcel_polygon() {
    // Synthetic 65 ft x 142 ft rectangular lot ring with collinear split vertices along both N-S sides
    let base_lat = 37.282000_f64;
    let base_lon = -121.954000_f64;
    let lat_rad = base_lat.to_radians();
    let m_per_deg_lat = 111_132.92 - 559.82 * (2.0 * lat_rad).cos();
    let m_per_deg_lon = 111_412.84 * lat_rad.cos();
    let dlat_65ft = (65.0 * 0.3048) / m_per_deg_lat;
    let dlon_142ft = (142.0 * 0.3048) / m_per_deg_lon;

    let ring: Vec<(f64, f64)> = vec![
        (base_lat + dlat_65ft, base_lon),
        (base_lat + dlat_65ft * 0.65, base_lon),
        (base_lat, base_lon),
        (base_lat, base_lon - dlon_142ft),
        (base_lat + dlat_65ft * 0.45, base_lon - dlon_142ft),
        (base_lat + dlat_65ft, base_lon - dlon_142ft),
        (base_lat + dlat_65ft, base_lon),
    ];
    let (front_ft, rear_ft, depth_ft) =
        compute_lot_dimensions(&ring).expect("dimensions should compute");
    assert_eq!(
        front_ft, 65,
        "frontage should merge split vertices to 65 ft"
    );
    assert_eq!(rear_ft, 65, "rear should merge split vertices to 65 ft");
    assert_eq!(depth_ft, 142, "depth should be 142 ft");
}

#[test]
fn raw_county_sanitizer_redacts_pii_and_forbidden_tokens() {
    let raw = json!({
        "APN": "00000200",
        "USABLE_SQ_FT_LAN": 9200,
        "OwnerName": "REDACTED PERSON",
        "ASSESSEE": "REDACTED TRUST",
        "MailingAddress": "900 SYNTHREDACT LN",
        "ProjectName": "200 SYNTHPARCELBETA AVE (Owner)",
        "OBJECTID": 42,
        "provider": "internal",
        "effort": "max",
    });
    let clean = sanitize_county_raw_attributes(&raw);
    assert_eq!(clean.get("APN").and_then(|v| v.as_str()), Some("00000200"));
    assert_eq!(
        clean.get("USABLE_SQ_FT_LAN").and_then(|v| v.as_i64()),
        Some(9200)
    );
    assert!(!clean.contains_key("OwnerName"));
    assert!(!clean.contains_key("ASSESSEE"));
    assert!(!clean.contains_key("MailingAddress"));
    assert!(!clean.contains_key("ProjectName"));
    assert!(!clean.contains_key("OBJECTID"));
    assert!(!clean.contains_key("provider"));
    assert!(!clean.contains_key("effort"));
}

#[test]
fn never_guesses_sqft_from_rooftop_when_roll_omits_it() {
    // When parcel roll provides `apn` and `lotSqft` without interior `sqft`,
    // `build_county_records_dossier` with a rooftop polygon MUST NOT fabricate interior `sqft`.
    let geo = json!({
        "displayAddress": "100 Synthrooftop01 Ave, Pittsburgh, PA 15215",
        "matchedAddress": "100 SYNTHROOFTOP01 AVE, PITTSBURGH, PA, 15215",
        "lat": 40.496900,
        "lon": -79.900900,
        "city": "Pittsburgh",
        "county": "Allegheny County",
        "countyFips": "42003",
        "state": "PA",
        "zip": "15215",
    });
    let rooftop = json!({
        "sqft": 1240.0,
        "areaSqft": 1240.0,
        "levels": 1,
        "centerLat": 40.496900,
        "centerLon": -79.900900,
        "offsetM": 4.0,
    });

    let mut merged: Map<String, serde_json::Value> = Map::new();
    merged.insert("apn".into(), json!("000-00-360"));
    merged.insert("lotSqft".into(), json!(8400));

    let dossier = build_county_records_dossier(&merged, &geo, Some(&rooftop), &[]);
    merged.insert("countyRecords".into(), dossier);

    assert!(
        merged.get("sqft").is_none(),
        "rooftop footprint must never fabricate interior sqft when assessor roll omits it"
    );
    assert_eq!(
        merged
            .get("countyRecords")
            .and_then(|c| c.pointer("/parcelIdentity/apn"))
            .and_then(|v| v.as_str()),
        Some("000-00-360")
    );
}

#[test]
fn synth_secondary_county_dossier_renders_and_normalizes_idempotently() {
    let mut report = common::synth_report_secondary();
    report["facts"]["countyRecords"] = json!({
        "available": true,
        "parcelIdentity": {
            "apn": "000-00-200",
            "jurisdiction": "Santa Clara County, CA"
        },
        "lotReconciliation": {
            "authoritativeLotSqft": 9200,
            "assessorUsableSqft": 9200,
            "gisPolygonSqft": 9410,
            "portalRoundedAcres": 0.22,
            "portalRoundedSqft": 9583,
            "legacyRollAcres": 0.21,
            "legacyAcreDerivedSqft": 9148,
            "acreTruncationFlagged": true
        },
        "recorderAndTransfers": {
            "transfers": [
                {
                    "date": "2021-05-20",
                    "docNumber": "SYNTH-DOC-20210520",
                    "price": 2_050_000,
                    "documentType": "Grant Deed"
                }
            ]
        }
    });
    ensure_county_records_on_report(&mut report);

    let html = render_report_html(&report);
    assert!(html.contains("href=\"#county\""));
    assert!(html.contains("id=\"county\""));
    assert!(html.contains("County Records"));
    assert!(html.contains("县府档案"));
    assert!(html.contains("data-lot-hover=\"1\""));
    assert!(html.contains("lotDetailPopover"));
    assert!(html.contains("buildLotHoverHtml"));
    for forbidden in [
        "gpt-4.1",
        "gpt-4o-mini",
        "HomeLens",
        "\"provider\"",
        "\"effort\"",
        "fallbackUsed",
    ] {
        assert!(
            !html.contains(forbidden),
            "rendered HTML leaked forbidden token: {forbidden}"
        );
    }

    // Legacy cached report missing `portalRoundedSqft` gets normalized once and stays idempotent
    let mut legacy_cached = json!({
        "meta": { "slug": common::SYNTH_SECONDARY_SLUG },
        "translations": { "zh": { "data": {} }, "es": { "data": {} } },
        "facts": {
            "lotSqft": 9900,
            "countyRecords": {
                "available": true,
                "lotReconciliation": {
                    "authoritativeLotSqft": 9900,
                    "assessorUsableSqft": 9900,
                    "gisPolygonSqft": 10111,
                    "legacyRollAcres": 0.22,
                    "legacyAcreDerivedSqft": 9583,
                    "acreTruncationFlagged": true
                }
            }
        }
    });
    assert!(normalize_stored_report(&mut legacy_cached));
    assert_eq!(
        legacy_cached
            .pointer("/facts/countyRecords/lotReconciliation/portalRoundedSqft")
            .and_then(|v| v.as_i64()),
        Some(10018)
    );
    assert!(
        !normalize_stored_report(&mut legacy_cached),
        "normalize_stored_report must be idempotent"
    );
}

#[test]
fn public_gis_csv_and_zero_price_public_record_enrichment_works() {
    let csv = concat!(
        "SALE TYPE,SOLD DATE,PROPERTY TYPE,ADDRESS,CITY,STATE OR PROVINCE,ZIP OR POSTAL CODE,PRICE,BEDS,BATHS,LOCATION,SQUARE FEET,LOT SIZE,YEAR BUILT,DAYS ON MARKET,$/SQUARE FEET,HOA/MONTH,STATUS,NEXT OPEN HOUSE START TIME,NEXT OPEN HOUSE END TIME,URL,SOURCE,MLS#,FAVORITE,INTERESTED,LATITUDE,LONGITUDE\n",
        "PAST SALE,May-20-2021,Single Family Residential,200 Synthparcelbeta Ave,Campbell,CA,95008,2050000,4,3.0,Campbell,2200,9410,1958,,932,,Sold,,,https://gis.campbellca.gov/parcels/000-00-200,SyntheticMLS,SYNTH-MLS-200,N,Y,37.282000,-121.954000\n",
        "PAST SALE,,Single Family Residential,204 Synthparcelbeta Ct,Campbell,CA,95008,,4,3.0,,1940,9800,1962,,,,,,,https://gis.campbellca.gov/parcels/000-00-204,,,N,Y,37.282400,-121.954300\n"
    );
    let geo_200 = json!({
        "displayAddress": common::SYNTH_SECONDARY_ADDRESS,
        "city": "Campbell",
        "county": "Santa Clara County",
        "countyFips": "06085",
        "state": "CA",
        "zip": "95008",
        "lat": 37.282000,
        "lon": -121.954000,
    });
    let parsed_200 = parse_public_gis_csv_all(
        csv,
        common::SYNTH_SECONDARY_ADDRESS,
        &geo_200,
        37.282000,
        -121.954000,
    );
    assert_eq!(
        parsed_200
            .subject_facts
            .get("lastSale")
            .and_then(|x| x.get("price"))
            .and_then(|v| v.as_i64()),
        Some(2_050_000)
    );
    assert_eq!(
        parsed_200
            .subject_facts
            .get("mlsLotSqft")
            .and_then(|v| v.as_i64()),
        Some(9410)
    );

    let mut merged = parsed_200.subject_facts.clone();
    merged.insert("apn".into(), json!("000-00-200"));
    merged.insert("lotSqft".into(), json!(9200));
    merged.insert(
        "lotReconciliation".into(),
        json!({
            "authoritativeLotSqft": 9200,
            "assessorUsableSqft": 9200,
            "gisPolygonSqft": 9410,
            "legacyRollAcres": 0.21,
            "legacyAcreDerivedSqft": 9148,
            "acreTruncationFlagged": true,
        }),
    );
    let dossier = build_county_records_dossier(&merged, &geo_200, None, &[]);
    assert_eq!(
        dossier
            .pointer("/lotReconciliation/mlsLotSqft")
            .and_then(|v| v.as_i64()),
        Some(9410)
    );
    let portals = dossier
        .get("officialPortals")
        .and_then(|v| v.as_array())
        .expect("officialPortals");
    assert!(portals.iter().any(|p| p
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .contains("santaclaracounty.telleronline.net")));
    assert!(portals.iter().any(|p| p
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .contains("clerkrecorder.santaclaracounty.gov")));

    // Off-market public-record row with blank PRICE still populates interior & lot specs
    let geo_204 = json!({
        "displayAddress": "204 Synthparcelbeta Ct, Campbell, CA 95008",
        "city": "Campbell",
        "state": "CA",
        "zip": "95008",
        "lat": 37.282400,
        "lon": -121.954300,
    });
    let parsed_204 = parse_public_gis_csv_all(
        csv,
        "204 Synthparcelbeta Ct, Campbell, CA 95008",
        &geo_204,
        37.282400,
        -121.954300,
    );
    assert_eq!(
        parsed_204
            .subject_facts
            .get("sqft")
            .and_then(|v| v.as_i64()),
        Some(1940)
    );
    assert_eq!(
        parsed_204
            .subject_facts
            .get("lotSqft")
            .and_then(|v| v.as_i64()),
        Some(9800)
    );
    assert_eq!(
        parsed_204
            .subject_facts
            .get("mlsLotSqft")
            .and_then(|v| v.as_i64()),
        Some(9800)
    );
    assert!(parsed_204.subject_facts.get("lastSale").is_none());
}

#[test]
fn data_point_anchor_builder_encodes_values_and_strips_pii_and_forbidden_tokens() {
    // Encodes commas (`%2C`) so W3C Text Fragments do not split `textStart,textEnd`
    let u1 = with_data_point_anchor("https://www.sccassessor.org/", None, Some("2,053"));
    assert_eq!(u1, "https://www.sccassessor.org/#:~:text=2%2C053");

    // Combines section anchor + exact data point (`#housing:~:text=...`)
    let u2 = with_data_point_anchor(
        "https://censusreporter.org/profiles/14000US06085502907/",
        Some("housing"),
        Some("1974"),
    );
    assert_eq!(
        u2,
        "https://censusreporter.org/profiles/14000US06085502907/#housing:~:text=1974"
    );

    // Encodes leading/trailing `-` as `%2D` so W3C prefix/suffix syntax is not triggered
    let u3 = with_data_point_anchor("https://sjpermits.org/", None, Some("-SYNTH-2024-"));
    assert_eq!(u3, "https://sjpermits.org/#:~:text=%2DSYNTH-2024%2D");

    // Overrides generic parcel anchor when a specific field's data point is provided
    let u4 = with_data_point_anchor(
        "https://portal.assessor.lacounty.gov/parceldetail/0000001002#:~:text=0000-001-002",
        None,
        Some("1958"),
    );
    assert_eq!(
        u4,
        "https://portal.assessor.lacounty.gov/parceldetail/0000001002#:~:text=1958"
    );

    // Preserves OpenStreetMap `#map=19/...` router fragment untouched
    let osm = "https://www.openstreetmap.org/#map=19/37.282000/-121.954000";
    assert_eq!(with_data_point_anchor(osm, None, Some("2,200")), osm);

    // Never embeds owner PII or internal tokens in any fragment
    for forbidden_hint in ["OwnerName Jane Doe", "Assessee Trust", "user@example.com"] {
        let clean =
            with_data_point_anchor("https://www.sccassessor.org/", None, Some(forbidden_hint));
        assert_eq!(clean, "https://www.sccassessor.org/");
    }

    // Builds interactive Campbell parcel viewer URL (never disabled ArcGIS `f=html` or bogus `#:~:text=`)
    let arc = build_arcgis_parcel_html_query_url(
        "https://gis.campbellca.gov/arcgis/rest/services/Parcels/MapServer/0",
        Some("000-00-200"),
        "06085",
    )
    .expect("Campbell single-parcel viewer");
    assert_eq!(
        arc,
        "https://gis.campbellca.gov/portal/apps/experiencebuilder/experience/?id=a9b10db4e5f04070ae15c3f745443cbf"
    );
    assert!(!arc.contains("f=html"));
    assert!(!arc.contains(":~:text="));
    assert!(!arc.to_ascii_lowercase().contains("owner"));
}

#[test]
fn official_record_links_cover_major_us_counties_and_cities_with_deep_anchors() {
    // 1. Campbell / Santa Clara County, CA (including real APN 305-50-002 format check)
    let scc_real = resolve_official_record_links(
        "06085",
        "Santa Clara County",
        "CA",
        "Campbell",
        Some("305-50-002"),
        Some("236 Synthrincon Ave, Campbell, CA 95008"),
        None,
        None,
    );
    assert_eq!(
        scc_real.assessor_url,
        "https://santaclaracounty.telleronline.net/search/1/details?APN=305-50-002"
    );
    assert!(!scc_real.assessor_url.contains("sccassessor.org"));
    assert!(!scc_real.assessor_url.contains(":~:text="));

    let scc = resolve_official_record_links(
        "06085",
        "Santa Clara County",
        "CA",
        "Campbell",
        Some("000-00-200"),
        Some(common::SYNTH_SECONDARY_ADDRESS),
        None,
        None,
    );
    assert_eq!(
        scc.assessor_url,
        "https://santaclaracounty.telleronline.net/search/1/details?APN=000-00-200"
    );
    assert_eq!(
        scc.tax_collector_url,
        "https://santaclaracounty.telleronline.net/search/1/details?APN=000-00-200"
    );
    assert_eq!(scc.recorder_url, "https://clerkrecorder.santaclaracounty.gov/");
    assert_eq!(scc.permit_portal_url, "https://www.mgoconnect.org/cp?JID=187");
    assert!(!scc.permit_portal_url.contains("/161/Building"));
    assert_eq!(
        scc.zoning_portal_url.as_deref(),
        Some("https://www.campbellca.gov/166/Zoning-and-Land-Use")
    );
    assert_eq!(
        scc.gis_parcel_query_url.as_deref(),
        Some("https://gis.campbellca.gov/portal/apps/experiencebuilder/experience/?id=a9b10db4e5f04070ae15c3f745443cbf")
    );

    // 2. San Francisco, CA
    let sf = resolve_official_record_links(
        "06075",
        "San Francisco",
        "CA",
        "San Francisco",
        Some("0000-101"),
        Some("100 Synthbay01 St, San Francisco, CA"),
        None,
        None,
    );
    assert!(sf
        .assessor_url
        .contains("sfplanninggis.org/pim/?tab=property-details&search=0000-101"));
    assert!(sf
        .gis_parcel_query_url
        .as_deref()
        .unwrap_or("")
        .contains("wv5m-vpq2.json"));

    // 3. Los Angeles, CA
    let la = resolve_official_record_links(
        "06037",
        "Los Angeles County",
        "CA",
        "Los Angeles",
        Some("0000-001-002"),
        Some("100 Synthmetro02 Ter, Los Angeles, CA"),
        None,
        None,
    );
    assert!(la
        .assessor_url
        .starts_with("https://portal.assessor.lacounty.gov/parceldetail/0000001002"));
    assert!(la.permit_portal_url.contains("ladbsservices2.lacity.org"));

    // 4. King County / Seattle, WA
    let kc = resolve_official_record_links(
        "53033",
        "King County",
        "WA",
        "Seattle",
        Some("000000-0100"),
        Some("100 Synthsound03 Ave, Seattle, WA"),
        None,
        None,
    );
    assert!(kc.assessor_url.starts_with(
        "https://blue.kingcounty.com/Assessor/eRealProperty/Dashboard.aspx?ParcelNbr=0000000100"
    ));
    assert!(kc.permit_portal_url.contains("cosaccela.seattle.gov"));

    // 5. Cook County / Chicago, IL
    let cook = resolve_official_record_links(
        "17031",
        "Cook County",
        "IL",
        "Chicago",
        Some("00-00-000-001-0000"),
        Some("100 Synthlake04 St, Chicago, IL"),
        None,
        None,
    );
    assert!(cook
        .assessor_url
        .starts_with("https://www.cookcountyassessor.com/pin/00000000010000"));
    assert!(cook
        .gis_parcel_query_url
        .as_deref()
        .unwrap_or("")
        .contains("pin=00000000010000"));

    // 6. Philadelphia, PA (deep OPA + Atlas /deeds + Atlas /li)
    let phl = resolve_official_record_links(
        "42101",
        "Philadelphia County",
        "PA",
        "Philadelphia",
        Some("000000105"),
        Some("100 Synthliberty05 St, Philadelphia, PA 19107"),
        None,
        None,
    );
    assert!(phl
        .assessor_url
        .starts_with("https://property.phila.gov/?p=000000105"));
    assert!(phl
        .recorder_url
        .contains("atlas.phila.gov/100%20Synthliberty05%20St/deeds"));
    assert!(phl
        .permit_portal_url
        .contains("atlas.phila.gov/100%20Synthliberty05%20St/li"));

    // 7. NYC Manhattan (ZoLa lot + ACRIS + DOB BIS + MapPLUTO BBL)
    let nyc = resolve_official_record_links(
        "36061",
        "New York County",
        "NY",
        "New York",
        Some("1000010001"),
        Some("100 Synthgrid06 Ave, New York, NY"),
        None,
        None,
    );
    assert!(nyc
        .assessor_url
        .starts_with("https://zola.planning.nyc.gov/lot/1/1/1"));
    assert!(nyc.recorder_url.contains("a836-acris.nyc.gov"));
    assert!(nyc.permit_portal_url.contains("a810-bisweb.nyc.gov"));
    assert!(nyc
        .gis_parcel_query_url
        .as_deref()
        .unwrap_or("")
        .contains("bbl=1000010001"));

    // 8. Maricopa / Travis / Cambridge MA
    let mc = resolve_official_record_links(
        "04013",
        "Maricopa County",
        "AZ",
        "Phoenix",
        Some("000-00-108"),
        None,
        None,
        None,
    );
    assert!(mc
        .assessor_url
        .starts_with("https://mcassessor.maricopa.gov/mcs/?q=000-00-108"));
    let tcad = resolve_official_record_links(
        "48453",
        "Travis County",
        "TX",
        "Austin",
        Some("00000109"),
        None,
        None,
        None,
    );
    assert!(tcad
        .assessor_url
        .starts_with("https://travis.prodigycad.com/property-detail/00000109"));
    let camb = resolve_official_record_links(
        "25017",
        "Middlesex County",
        "MA",
        "Cambridge",
        None,
        Some("100 Synthcharles07 Ave, Cambridge, MA"),
        None,
        None,
    );
    assert!(camb
        .assessor_url
        .contains("cambridgema.gov/PropertyDatabase"));
    assert!(camb
        .recorder_url
        .contains("masslandrecords.com/MiddlesexSouth"));
}

#[test]
fn synth_reports_enrich_every_fact_and_render_anchored_cross_reference_links() {
    for (label, mut report) in [
        ("synth-primary", common::synth_report_primary()),
        ("synth-secondary", common::synth_report_secondary()),
    ] {
        ensure_county_records_on_report(&mut report);

        let fs = report
            .pointer("/facts/fieldSources")
            .and_then(|v| v.as_object())
            .expect("facts.fieldSources populated");
        for required_key in ["apn", "sqft", "lotSqft", "yearBuilt", "beds", "baths"] {
            if report
                .pointer(&format!("/facts/{required_key}"))
                .map(|v| !v.is_null())
                .unwrap_or(false)
            {
                let u = fs
                    .get(required_key)
                    .and_then(|e| e.get("url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                assert!(
                    u.starts_with("https://") && !u.contains(' '),
                    "{label}: fieldSources.{required_key}.url must be a clean https:// URL, got: {u}"
                );
            }
        }

        // Verify taxHistory, saleHistory, transfers, permits, and comps carry clean https:// sourceUrl without bogus homepage fragments
        for arr_ptr in [
            "/facts/taxHistory",
            "/facts/saleHistory",
            "/facts/titleAndLiens/transfers",
            "/facts/permits",
            "/facts/comps",
        ] {
            if let Some(arr) = report.pointer(arr_ptr).and_then(|v| v.as_array()) {
                for (idx, item) in arr.iter().enumerate() {
                    let su = item.get("sourceUrl").and_then(|v| v.as_str()).unwrap_or("");
                    assert!(
                        su.starts_with("https://") && !su.contains(' '),
                        "{label}: {arr_ptr}[{idx}].sourceUrl missing clean https:// URL: {su}"
                    );
                }
            }
        }

        let html = render_report_html(&report);
        if let Ok(dump_dir) = std::env::var("OPEN_ESTATE_DUMP_TEST_HTML_DIR") {
            let out_p = std::path::Path::new(&dump_dir).join(format!("{label}.html"));
            std::fs::write(&out_p, &html).expect("write test HTML");
        }
        assert!(html.contains("window.withDataPointAnchor = withDataPointAnchor"));
        assert!(html.contains("function factSourceLink("));
        assert!(html.contains(".src-lnk{"));
        assert!(html.contains(":~:text="));
        // Zero forbidden tokens in rendered HTML
        let forbidden = [
            "gpt-4o-mini",
            "gpt-4.1",
            "rust-sqlx-sqlite",
            "fallbackUsed",
            "deterministic-fallback",
            "max reasoning",
            "aiModel",
            "OPENAI_API_KEY",
            "effort",
            "provider",
            "HomeLens",
        ];
        for tok in forbidden {
            assert!(
                !html.contains(tok),
                "{label}: rendered HTML leaked forbidden token `{tok}`"
            );
        }
    }
}
