//! Public address policy for the cast LAN listener. The request-serving paths
//! are covered by unit tests in `src/cast_listener.rs`, which can inject peer
//! addresses; a public request needs a `PlaybackSourceRequest`, which only
//! `jellyfin-api` can build.

use std::net::IpAddr;

use jellyfin_playback_relay::cast_listener::{
    CastListenerError, CastRefusal, check_cast_bind_address, is_private_lan_address,
};

#[test]
fn private_and_link_local_addresses_are_accepted_for_bind_and_peer() {
    for text in [
        "10.0.0.5",
        "172.16.4.9",
        "172.31.255.254",
        "192.168.1.20",
        "169.254.10.1",
        "fd00::20",
        "fe80::20",
        "::ffff:192.168.1.20",
    ] {
        let address: IpAddr = text.parse().expect("test address");
        assert!(is_private_lan_address(address), "{text} should be LAN");
        assert_eq!(check_cast_bind_address(address), Ok(()), "{text}");
    }
}

#[test]
fn loopback_public_unspecified_and_shared_addresses_are_refused() {
    for text in [
        "127.0.0.1",
        "::1",
        "0.0.0.0",
        "::",
        "8.8.8.8",
        "172.32.0.1",
        "100.64.0.1",
        "169.255.0.1",
        "224.0.0.1",
        "2001:4860:4860::8888",
        "::ffff:8.8.8.8",
    ] {
        let address: IpAddr = text.parse().expect("test address");
        assert!(!is_private_lan_address(address), "{text} should be refused");
        assert_eq!(
            check_cast_bind_address(address),
            Err(CastListenerError::NotLanAddress),
            "{text}"
        );
    }
}

#[test]
fn capability_refusal_describes_session_scope_not_single_use() {
    let text = CastRefusal::CapabilityRejected.to_string();
    assert!(text.contains("session"), "{text}");
    assert!(!text.contains("used"), "{text}");
    assert!(!text.contains("once"), "{text}");
}

#[test]
fn every_refusal_reason_is_free_of_urls_and_paths() {
    for reason in [
        CastRefusal::PeerNotOnLan,
        CastRefusal::CapabilityRejected,
        CastRefusal::UnsupportedMedia,
        CastRefusal::UpstreamUnavailable,
    ] {
        let text = reason.to_string();
        assert!(!text.is_empty());
        assert!(!text.contains("http"), "{text}");
        assert!(!text.contains('/'), "{text}");
    }
}
