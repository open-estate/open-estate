use open_estate::shared::{EnvConfig, HttpClient};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[test]
fn test_egress_bypass_control_plane_vs_public_gis_and_redaction() {
    let mut vars = HashMap::new();
    vars.insert(
        "OUTBOUND_PROXY_URLS".to_string(),
        "http://user1:secretpass@127.0.0.1:18081,http://ops:topsecret@127.0.0.1:18082".to_string(),
    );
    vars.insert(
        "OUTBOUND_NO_PROXY".to_string(),
        "custom-internal.open-estate.local".to_string(),
    );
    let env = EnvConfig::from_map(vars);
    let client = HttpClient::from_env(&env, None);

    // Control-plane, LLM, and API-keyed hosts MUST bypass outbound scraping proxies
    for host in [
        "api.openai.com",
        "us.api.openai.com",
        "api.gateway.attomdata.com",
        "places.googleapis.com",
        "translation.googleapis.com",
        "api.shovels.ai",
        "api.zoneomics.com",
        "custom-internal.open-estate.local",
        "localhost",
        "127.0.0.1",
        "::1",
        "10.24.0.8",
        "192.168.1.42",
        "router.osrm.svc",
        "postgres.internal",
        "cache.local",
    ] {
        assert!(
            client.should_bypass_proxy(host),
            "control-plane or private host `{host}` must bypass outbound proxies"
        );
    }

    // Keyless public GIS, federal, and OSM endpoints MUST route through configured egress
    for host in [
        "geocoding.geo.census.gov",
        "hazards.fema.gov",
        "msc.fema.gov",
        "services.arcgis.com",
        "epqs.nationalmap.gov",
        "earthquake.usgs.gov",
        "overpass-api.de",
        "overpass.private.coffee",
    ] {
        assert!(
            !client.should_bypass_proxy(host),
            "public GIS/federal host `{host}` must use the outbound egress pool"
        );
    }

    // Status telemetry must expose slot structure while redacting proxy credentials
    let status = client.egress_status_json();
    let status_text = status.to_string();
    assert!(
        !status_text.contains("secretpass")
            && !status_text.contains("topsecret")
            && !status_text.contains("user1")
            && !status_text.contains("ops:"),
        "proxy credentials leaked in egress_status_json: {status_text}"
    );
    assert_eq!(status["mode"], "static_proxy");
    assert_eq!(status["splitTrafficEnabled"], true);
    assert_eq!(status["strictProxyEgress"], false);

    let slots = status["slots"].as_array().expect("slots array");
    assert_eq!(
        slots.len(),
        3,
        "expected 2 static proxy slots + 1 direct fail-open slot"
    );
    assert_eq!(slots[0]["slot"], "http://127.0.0.1:18081");
    assert_eq!(slots[0]["direct"], false);
    assert_eq!(slots[1]["slot"], "http://127.0.0.1:18082");
    assert_eq!(slots[1]["direct"], false);
    assert_eq!(slots[2]["slot"], "direct");
    assert_eq!(slots[2]["direct"], true);
}

#[tokio::test]
async fn test_round_robin_selection_and_health_cooldown_scoring() {
    let bypass_hosts = vec![
        "api.openai.com".to_string(),
        "api.gateway.attomdata.com".to_string(),
        "places.googleapis.com".to_string(),
    ];
    let client = HttpClient::with_egress_config(
        None,
        16,
        256,
        "http://egress_user:secretpass@127.0.0.1:19101,http://127.0.0.1:19102",
        bypass_hosts,
        true,
    );

    // 1. Control-plane URLs always acquire a direct handle without touching proxy slots
    for _ in 0..4 {
        let h_llm = client
            .acquire_egress_handle("https://api.openai.com/v1/responses")
            .await
            .expect("direct handle for api.openai.com");
        assert!(h_llm.is_direct());
        assert_eq!(h_llm.proxy_label(), None);
    }
    let st_after_llm = client.egress_status_json();
    assert_eq!(st_after_llm["slots"][0]["requests"], 0);
    assert_eq!(st_after_llm["slots"][1]["requests"], 0);
    assert_eq!(st_after_llm["slots"][2]["requests"], 4);

    // 2. Public federal GIS URLs distribute evenly in round-robin order across healthy static slots
    let mut seen_labels = Vec::new();
    for url in [
        "https://geocoding.geo.census.gov/geocoder/benchmarks",
        "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
        "https://epqs.nationalmap.gov/v1/json",
        "https://earthquake.usgs.gov/ws/designmaps/asce7-22.json",
        "https://services.arcgis.com/P3ePLMYs2RVChkJx/ArcGIS/rest/services",
        "https://overpass-api.de/api/interpreter",
    ] {
        let h = client
            .acquire_egress_handle(url)
            .await
            .expect("egress handle for public GIS host");
        assert!(!h.is_direct());
        seen_labels.push(h.proxy_label().unwrap_or_default());
    }

    let p1_count = seen_labels
        .iter()
        .filter(|l| l.as_str() == "http://127.0.0.1:19101")
        .count();
    let p2_count = seen_labels
        .iter()
        .filter(|l| l.as_str() == "http://127.0.0.1:19102")
        .count();
    assert_eq!(p1_count, 3, "expected 3 requests on slot 0");
    assert_eq!(p2_count, 3, "expected 3 requests on slot 1");

    // 3. Transport failure cools down the broken proxy globally so traffic shifts to the healthy peer
    let h_first = client
        .acquire_egress_handle("https://geocoding.geo.census.gov/geocoder/benchmarks")
        .await
        .expect("handle before transport failure");
    let failed_label = h_first.proxy_label().expect("static proxy label");
    h_first.report_transport_failure();

    let st_cooled = client.egress_status_json();
    let failed_slot = st_cooled["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["slot"].as_str() == Some(failed_label.as_str()))
        .expect("failed slot in status");
    assert_eq!(failed_slot["healthy"], false);
    assert_eq!(failed_slot["failovers"], 1);

    for _ in 0..4 {
        let h_next = client
            .acquire_egress_handle("https://geocoding.geo.census.gov/geocoder/benchmarks")
            .await
            .expect("failover handle");
        assert_ne!(
            h_next.proxy_label().as_deref(),
            Some(failed_label.as_str()),
            "globally cooled-down proxy must be skipped while a healthy peer exists"
        );
    }

    // Reporting success clears global cooldown
    h_first.report_success();
    let st_recovered = client.egress_status_json();
    let recovered_slot = st_recovered["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["slot"].as_str() == Some(failed_label.as_str()))
        .expect("recovered slot in status");
    assert_eq!(recovered_slot["healthy"], true);

    // 4. Per-host HTTP 429 cooldown isolates only the affected origin host
    let h_fema = client
        .acquire_egress_handle(
            "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
        )
        .await
        .expect("fema handle");
    let fema_cooled_label = h_fema.proxy_label().expect("fema proxy label");
    h_fema.report_http_failure(429);

    for _ in 0..3 {
        let h_retry = client
            .acquire_egress_handle(
                "https://hazards.fema.gov/arcgis/rest/services/public/NFHL/MapServer/28",
            )
            .await
            .expect("fema retry handle");
        assert_ne!(
            h_retry.proxy_label().as_deref(),
            Some(fema_cooled_label.as_str()),
            "host-cooled proxy must be skipped for hazards.fema.gov"
        );
    }
    // Meanwhile, other hosts still see both static proxies as globally healthy
    let st_host_only = client.egress_status_json();
    assert_eq!(st_host_only["slots"][0]["healthy"], true);
    assert_eq!(st_host_only["slots"][1]["healthy"], true);
}

#[tokio::test]
async fn test_raw_tcp_connect_tunnel_and_dead_proxy_failover() {
    // 1. Verify raw RFC 7231 HTTP CONNECT byte-stream tunnel framing over loopback sockets
    // (echoing raw transport bytes only — zero synthetic GIS or LLM HTTP payloads).
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_listener.local_addr().unwrap();
    let echo_task = tokio::spawn(async move {
        if let Ok((mut sock, _)) = echo_listener.accept().await {
            let mut buf = [0u8; 64];
            if let Ok(n) = sock.read(&mut buf).await {
                let _ = sock.write_all(&buf[..n]).await;
            }
        }
    });

    let tunnel_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tunnel_addr = tunnel_listener.local_addr().unwrap();
    let tunnel_task = tokio::spawn(async move {
        if let Ok((mut client_sock, _)) = tunnel_listener.accept().await {
            let mut req_buf = vec![0u8; 512];
            let n = client_sock.read(&mut req_buf).await.unwrap_or(0);
            let req_head = String::from_utf8_lossy(&req_buf[..n]);
            assert!(
                req_head.starts_with(&format!("CONNECT {echo_addr} HTTP/1.1\r\n")),
                "unexpected CONNECT request header: {req_head}"
            );
            client_sock
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut upstream = TcpStream::connect(echo_addr).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client_sock, &mut upstream).await;
        }
    });

    let mut via_tunnel = TcpStream::connect(tunnel_addr).await.unwrap();
    let connect_wire = format!("CONNECT {echo_addr} HTTP/1.1\r\nHost: {echo_addr}\r\n\r\n");
    via_tunnel.write_all(connect_wire.as_bytes()).await.unwrap();
    let mut resp_buf = [0u8; 128];
    let rn = via_tunnel.read(&mut resp_buf).await.unwrap();
    let resp_line = String::from_utf8_lossy(&resp_buf[..rn]);
    assert!(
        resp_line.starts_with("HTTP/1.1 200"),
        "expected 200 tunnel establishment, got: {resp_line}"
    );
    let ping = b"OPEN_ESTATE-TUNNEL-PING";
    via_tunnel.write_all(ping).await.unwrap();
    let mut echoed = vec![0u8; ping.len()];
    via_tunnel.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, ping);
    drop(via_tunnel);
    let _ = tunnel_task.await;
    let _ = echo_task.await;

    // 2. Obtain a guaranteed-closed local TCP port by binding and immediately dropping a listener
    let closed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);

    // 3. Strict proxy egress with only a dead static proxy must fail closed (never leak direct)
    let strict_client = HttpClient::with_egress_config(
        None,
        8,
        64,
        &format!("http://127.0.0.1:{closed_port}"),
        vec!["api.openai.com".to_string()],
        true,
    )
    .with_strict_proxy_egress(true);

    let strict_res = strict_client
        .send_via_egress(
            "https://geocoding.geo.census.gov/geocoder/benchmarks?f=json",
            |c| c.get("https://geocoding.geo.census.gov/geocoder/benchmarks?f=json"),
        )
        .await;
    assert!(
        strict_res.is_err(),
        "strict_proxy_egress must fail closed when the configured proxy port is unreachable"
    );
    let strict_status = strict_client.egress_status_json();
    assert_eq!(strict_status["strictProxyEgress"], true);
    assert_eq!(strict_status["slots"][0]["healthy"], false);
    assert_eq!(strict_status["slots"][0]["failovers"], 1);

    // 4. Default non-strict fail-open mode marks the dead proxy unhealthy and fails over to `direct`
    // against the real keyless U.S. Census Geocoder benchmarks endpoint.
    let failopen_client = HttpClient::with_egress_config(
        None,
        8,
        64,
        &format!("http://127.0.0.1:{closed_port}"),
        vec!["api.openai.com".to_string()],
        true,
    );
    let census_url = "https://geocoding.geo.census.gov/geocoder/benchmarks?f=json";
    match failopen_client
        .send_via_egress(census_url, |c| {
            c.get(census_url).timeout(std::time::Duration::from_secs(6))
        })
        .await
    {
        Ok((resp, handle)) => {
            assert!(
                handle.is_direct(),
                "expected failover onto direct slot after dead static proxy"
            );
            assert!(
                resp.status().is_success(),
                "expected HTTP 200 from live Census benchmarks endpoint, got {}",
                resp.status()
            );
        }
        Err(e) => {
            eprintln!(
                "Note: live Census endpoint unreachable in offline environment after proxy failover ({e})"
            );
        }
    }
    let failopen_status = failopen_client.egress_status_json();
    assert_eq!(
        failopen_status["slots"][0]["healthy"], false,
        "dead static proxy slot must be marked unhealthy on transport failure"
    );
    assert!(
        failopen_status["slots"][0]["failovers"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "dead static proxy slot must increment failovers counter"
    );
}
