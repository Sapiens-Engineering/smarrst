use std::net::IpAddr;
fn main() {
    for s in ["http://[::1]/x", "http://[::]/x", "http://[fc00::1]/x", "http://[fe80::1]/x"] {
        let parsed = url::Url::parse(s).unwrap();
        let host = parsed.host_str();
        let host_ip = parsed.host().map(|h| match h {
            url::Host::Ipv4(v) => Some(IpAddr::V4(v)),
            url::Host::Ipv6(v) => Some(IpAddr::V6(v)),
            url::Host::Domain(_) => None,
        });
        println!("{s} -> host_str={:?} host_ip={:?}", host, host_ip);
    }
}
