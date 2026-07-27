use molt_net::address::{IpAddr, Ipv4Addr, Ipv6Addr, MacAddress};
use molt_net::neighbor::Cache;

const V4: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2));
const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);
const V4_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 4]);
const V6_MAC: MacAddress = MacAddress::new([0x02, 0, 0, 0, 0, 6]);

#[test]
fn cache_keeps_both_families() {
    let mut cache = Cache::<2>::new();

    cache.learn(V4, V4_MAC);
    cache.learn(V6, V6_MAC);

    assert_eq!(cache.resolve(V4), Some(V4_MAC));
    assert_eq!(cache.resolve(V6), Some(V6_MAC));
}
