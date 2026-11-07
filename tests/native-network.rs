use firecracker_server::host::network::{self, Plan};

#[test]
fn native_network_rules_preserve_the_existing_isolation_contract() {
    let plan: Plan = serde_json::from_str(include_str!("fixtures/network-plan.json")).unwrap();
    assert_eq!(network::host_rules(&plan), include_str!("fixtures/network-host.nft"));
    assert_eq!(
        network::namespace_rules(&plan.slots[0]),
        include_str!("fixtures/network-namespace.nft")
    );
}

#[test]
fn subnet_checks_cover_default_host_and_private_ranges() {
    assert!(network::contains("10.203.0.0/16", "10.203.255.254".parse().unwrap()).unwrap());
    assert!(!network::contains("10.203.0.0/16", "10.204.0.1".parse().unwrap()).unwrap());
    assert!(network::contains("0.0.0.0/0", "192.168.31.10".parse().unwrap()).unwrap());
    assert!(network::contains("10.204.0.1/32", "10.204.0.1".parse().unwrap()).unwrap());
    assert!(network::contains("10.0.0.0/33", "10.0.0.1".parse().unwrap()).is_err());
}
