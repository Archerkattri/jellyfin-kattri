use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use jellyfin_api::{
    DISCOVERY_PORT, DISCOVERY_REQUEST, DiscoveryError, DiscoveryOptions, MAX_DISCOVERY_CANDIDATES,
    MAX_DISCOVERY_DATAGRAM_BYTES, MAX_DISCOVERY_DATAGRAMS, MAX_DISCOVERY_TIMEOUT,
    discover_lan_servers,
};

fn loopback_socket() -> UdpSocket {
    UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback UDP socket")
}

fn options(target: SocketAddr) -> DiscoveryOptions {
    DiscoveryOptions::default()
        .with_target(target)
        .with_timeout(Duration::from_secs(1))
}

fn response(address: &str, id: &str, name: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "Address": address,
        "Id": id,
        "Name": name,
        "EndpointAddress": null
    }))
    .expect("serialize discovery response")
}

#[test]
fn sends_the_jellyfin_probe_and_returns_a_redacted_candidate() {
    let responder = loopback_socket();
    responder
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set responder timeout");
    let target = responder.local_addr().expect("responder address");
    let responder_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 128];
        let (length, client) = responder.recv_from(&mut buffer).expect("receive probe");
        assert_eq!(&buffer[..length], DISCOVERY_REQUEST);
        responder
            .send_to(
                &response(
                    "https://jellyfin.example/library",
                    "private-server-id",
                    "private-server-name",
                ),
                client,
            )
            .expect("send response");
        responder
            .send_to(
                &response("http://second.example", "second-id", "second-name"),
                client,
            )
            .expect("send additional response");
    });

    let cancelled = AtomicBool::new(false);
    let candidates = discover_lan_servers(
        options(target).with_max_datagrams(4).with_max_candidates(1),
        &cancelled,
    )
    .expect("discovery succeeds");
    responder_thread.join().expect("responder exits");

    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(
        candidate.address().as_str(),
        "https://jellyfin.example/library"
    );
    assert_eq!(candidate.id(), "private-server-id");
    assert_eq!(candidate.name(), "private-server-name");
    assert!(candidate.source().ip().is_loopback());

    let debug = format!("{candidate:?}");
    assert!(!debug.contains("jellyfin.example"));
    assert!(!debug.contains("private-server-id"));
    assert!(!debug.contains("private-server-name"));
}

#[test]
fn rejects_malformed_and_unsafe_advertised_origins() {
    let responder = loopback_socket();
    responder
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set responder timeout");
    let target = responder.local_addr().expect("responder address");
    let responder_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 128];
        let (_, client) = responder.recv_from(&mut buffer).expect("receive probe");
        for address in [
            "ftp://jellyfin.example",
            "http://user:password@jellyfin.example",
            "http://@jellyfin.example",
            "http://jellyfin.example/?credential=fixture",
            "https://jellyfin.example/#fragment",
            "http:///missing-host",
            " http://jellyfin.example",
            "http://jellyfin.example\\repaired-path",
            "http://jellyfin.example/\nrepaired-path",
        ] {
            responder
                .send_to(&response(address, "untrusted-id", "untrusted-name"), client)
                .expect("send unsafe response");
        }
        responder
            .send_to(
                &response("http://jellyfin.example/base", "safe-id", "safe-name"),
                client,
            )
            .expect("send safe response");
    });

    let cancelled = AtomicBool::new(false);
    let candidates = discover_lan_servers(
        options(target)
            .with_max_datagrams(10)
            .with_max_candidates(1),
        &cancelled,
    )
    .expect("discovery succeeds");
    responder_thread.join().expect("responder exits");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id(), "safe-id");
    assert_eq!(
        candidates[0].address().as_str(),
        "http://jellyfin.example/base"
    );
}

#[test]
fn ignores_oversized_datagrams_and_stops_at_the_datagram_cap() {
    let responder = loopback_socket();
    responder
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set responder timeout");
    let target = responder.local_addr().expect("responder address");
    let responder_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 128];
        let (_, client) = responder.recv_from(&mut buffer).expect("receive probe");
        let oversized = vec![b'x'; MAX_DISCOVERY_DATAGRAM_BYTES + 1];
        responder
            .send_to(&oversized, client)
            .expect("send oversized packet");
        responder
            .send_to(b"not-json", client)
            .expect("send malformed packet");
        responder
            .send_to(
                &response("http://127.0.0.1:8096", "third-id", "third-name"),
                client,
            )
            .expect("send packet beyond cap");
    });

    let cancelled = AtomicBool::new(false);
    let candidates = discover_lan_servers(
        options(target)
            .with_max_datagrams(2)
            .with_max_candidates(MAX_DISCOVERY_CANDIDATES),
        &cancelled,
    )
    .expect("discovery succeeds");
    responder_thread.join().expect("responder exits");

    assert!(candidates.is_empty());
}

#[test]
fn cancellation_interrupts_an_active_loopback_scan() {
    let responder = loopback_socket();
    responder
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set responder timeout");
    let target = responder.local_addr().expect("responder address");
    let (received_probe, wait_for_probe) = mpsc::channel();
    let responder_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 128];
        let (length, _) = responder.recv_from(&mut buffer).expect("receive probe");
        assert_eq!(&buffer[..length], DISCOVERY_REQUEST);
        received_probe.send(()).expect("signal probe receipt");
    });

    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let worker =
        thread::spawn(move || discover_lan_servers(options(target), worker_cancelled.as_ref()));
    wait_for_probe
        .recv_timeout(Duration::from_secs(1))
        .expect("discovery sent the probe");
    cancelled.store(true, Ordering::Release);

    assert_eq!(
        worker.join().expect("discovery worker exits"),
        Err(DiscoveryError::Cancelled)
    );
    responder_thread.join().expect("responder exits");
}

#[test]
fn scan_finishes_at_its_configured_deadline_without_responses() {
    let responder = loopback_socket();
    responder
        .set_read_timeout(Some(Duration::from_secs(1)))
        .expect("set responder timeout");
    let target = responder.local_addr().expect("responder address");
    let responder_thread = thread::spawn(move || {
        let mut buffer = [0_u8; 128];
        let (length, _) = responder.recv_from(&mut buffer).expect("receive probe");
        assert_eq!(&buffer[..length], DISCOVERY_REQUEST);
    });

    let started = Instant::now();
    let candidates = discover_lan_servers(
        options(target).with_timeout(Duration::from_millis(40)),
        &AtomicBool::new(false),
    )
    .expect("discovery succeeds");
    responder_thread.join().expect("responder exits");

    assert!(candidates.is_empty());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn validates_caps_and_local_destination_before_sending() {
    let cancelled = AtomicBool::new(false);
    for invalid in [
        DiscoveryOptions::default().with_timeout(MAX_DISCOVERY_TIMEOUT + Duration::from_millis(1)),
        DiscoveryOptions::default().with_timeout(Duration::ZERO),
        DiscoveryOptions::default().with_max_datagrams(0),
        DiscoveryOptions::default().with_max_datagrams(MAX_DISCOVERY_DATAGRAMS + 1),
        DiscoveryOptions::default().with_max_candidates(0),
        DiscoveryOptions::default().with_max_candidates(MAX_DISCOVERY_CANDIDATES + 1),
        DiscoveryOptions::default().with_target("8.8.8.8:7359".parse().expect("public target")),
        DiscoveryOptions::default().with_target("[::1]:7359".parse().expect("IPv6 target")),
    ] {
        assert_eq!(
            discover_lan_servers(invalid, &cancelled),
            Err(DiscoveryError::InvalidOptions)
        );
    }

    let already_cancelled = AtomicBool::new(true);
    assert_eq!(
        discover_lan_servers(DiscoveryOptions::default(), &already_cancelled),
        Err(DiscoveryError::Cancelled)
    );
}

#[test]
fn default_destination_uses_the_official_discovery_port() {
    let options = DiscoveryOptions::default();
    let formatted = format!("{options:?}");
    assert!(formatted.contains(&format!("{DISCOVERY_PORT}")));
}
