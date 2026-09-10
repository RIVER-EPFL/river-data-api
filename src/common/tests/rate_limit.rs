use super::*;

fn extractor() -> FallbackIpKeyExtractor {
    FallbackIpKeyExtractor::new(&default_trusted_proxies())
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn cidr_matches_on_the_prefix_only() {
    let net = IpCidr::parse("172.16.0.0/12").unwrap();
    assert!(net.contains(ip("172.16.0.1")));
    assert!(net.contains(ip("172.31.255.254")));
    assert!(!net.contains(ip("172.32.0.1")));
    assert!(!net.contains(ip("172.15.255.254")));
}

#[test]
fn a_bare_address_is_its_own_host_route() {
    let net = IpCidr::parse("10.1.2.3").unwrap();
    assert!(net.contains(ip("10.1.2.3")));
    assert!(!net.contains(ip("10.1.2.4")));
}

#[test]
fn a_v4_cidr_never_matches_a_v6_address() {
    assert!(!IpCidr::parse("10.0.0.0/8").unwrap().contains(ip("::1")));
}

#[test]
fn an_oversized_prefix_is_not_a_cidr() {
    assert!(IpCidr::parse("10.0.0.0/33").is_none());
    assert!(IpCidr::parse("not-an-address").is_none());
}

#[test]
fn a_spoofed_header_from_an_untrusted_peer_keys_on_the_peer() {
    let key = extractor().key_for(Some(ip("203.0.113.7")), Some("1.2.3.4"), Some("5.6.7.8"));
    assert_eq!(key, ip("203.0.113.7"));
}

#[test]
fn a_spoofed_prefix_from_a_trusted_peer_keys_on_what_the_proxy_appended() {
    // The client wrote "1.2.3.4"; the ingress appended the address it actually saw.
    let key = extractor().key_for(Some(ip("10.42.0.9")), Some("1.2.3.4, 203.0.113.7"), None);
    assert_eq!(key, ip("203.0.113.7"));
}

#[test]
fn a_forged_chain_of_private_addresses_does_not_shorten_the_walk() {
    let key = extractor().key_for(
        Some(ip("10.42.0.9")),
        Some("8.8.8.8, 10.0.0.1, 192.168.1.1, 203.0.113.7"),
        None,
    );
    assert_eq!(key, ip("203.0.113.7"));
}

#[test]
fn a_chain_of_only_trusted_hops_keys_on_the_peer() {
    let key = extractor().key_for(Some(ip("10.42.0.9")), Some("10.0.0.1, 192.168.1.1"), None);
    assert_eq!(key, ip("10.42.0.9"));
}

#[test]
fn real_ip_is_read_from_a_trusted_peer_when_the_chain_names_nobody() {
    let key = extractor().key_for(Some(ip("10.42.0.9")), None, Some("203.0.113.7"));
    assert_eq!(key, ip("203.0.113.7"));
}

#[test]
fn a_trusted_peer_with_no_headers_keys_on_itself() {
    assert_eq!(
        extractor().key_for(Some(ip("10.42.0.9")), None, None),
        ip("10.42.0.9")
    );
}

#[test]
fn unparseable_entries_are_walked_past_rather_than_ending_the_walk() {
    let key = extractor().key_for(
        Some(ip("10.42.0.9")),
        Some("203.0.113.7, unknown, _hidden"),
        None,
    );
    assert_eq!(key, ip("203.0.113.7"));
}

#[test]
fn no_connection_info_falls_back_to_localhost() {
    let key = extractor().key_for(None, Some("1.2.3.4"), None);
    assert_eq!(key, ip("127.0.0.1"));
}

#[test]
fn an_empty_trusted_set_believes_no_header() {
    let strict = FallbackIpKeyExtractor::new(&[]);
    assert_eq!(
        strict.key_for(Some(ip("10.42.0.9")), Some("1.2.3.4, 203.0.113.7"), None),
        ip("10.42.0.9")
    );
}
