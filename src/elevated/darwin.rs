//! What the macOS helper says to and reads from system tools; plain text, so it is tested on every platform.

/// The interface of the default IPv4 route in `netstat -rn -f inet`, leaving out interface-scoped routes and `exclude`.
pub fn default_interface(netstat: &str, exclude: &str) -> Option<String> {
    let mut lines = netstat.lines().skip_while(|l| !l.starts_with("Destination"));
    let header: Vec<&str> = lines.next()?.split_whitespace().collect();
    let (flags, netif) = (header.iter().position(|h| *h == "Flags")?, header.iter().position(|h| *h == "Netif")?);
    lines.take_while(|l| !l.trim().is_empty()).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.first() == Some(&"default") && !f.get(flags)?.contains('I') && *f.get(netif)? != exclude).then(|| f[netif].to_owned())
    })
}

/// `pfctl -E` prints `Token : <n>`, needed to release our reference on pf again.
pub fn pf_token(output: &str) -> Option<String> {
    output.lines().find_map(|l| l.trim().strip_prefix("Token")?.trim_start().strip_prefix(':')).map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// Our pf anchor. Always: DNS goes only through the TUN (or from the core). Kill switch: nothing else leaves outside the
/// TUN but the core's own traffic (its group), DHCP and the LAN, like on Linux.
pub fn pf_rules(tun: &str, core_gid: u32, kill_switch: bool) -> String {
    let mut rules = format!(
        "pass quick on lo0 all\n\
         pass quick on {tun} all\n\
         pass out quick proto {{ tcp udp }} from any to any group {core_gid}\n\
         block drop out quick proto {{ tcp udp }} from any to any port 53\n"
    );
    if kill_switch {
        rules.push_str(
            "pass out quick inet proto udp from any port 68 to any port 67\n\
             pass out quick inet6 proto udp from any port 546 to any port 547\n\
             pass out quick inet from any to { 10.0.0.0/8 100.64.0.0/10 169.254.0.0/16 172.16.0.0/12 192.168.0.0/16 224.0.0.0/4 255.255.255.255 }\n\
             pass out quick inet6 from any to { fc00::/7 fe80::/10 ff00::/8 }\n\
             block drop out quick all\n",
        );
    }
    rules
}

/// `172.19.0.1/30` → (`172.19.0.1`, `172.19.0.2`, `255.255.255.252`): our end of the point-to-point utun, the other, the mask.
pub fn p2p(addr: &str) -> Option<(String, String, String)> {
    let (ip, bits) = addr.split_once('/')?;
    let ip: std::net::Ipv4Addr = ip.parse().ok()?;
    let bits: u32 = bits.parse().ok().filter(|b| *b <= 32)?;
    let mask = std::net::Ipv4Addr::from(u32::MAX.checked_shl(32 - bits).unwrap_or(0));
    Some((ip.to_string(), std::net::Ipv4Addr::from(u32::from(ip) + 1).to_string(), mask.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETSTAT: &str = "Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            link#22            UCSg                utun4
default            192.168.1.1        UGScg                 en0
default            10.0.0.1           UGScIg                en7
0/1                link#25            UCS               utun9
127                127.0.0.1          UCS                   lo0
";

    #[test]
    fn default_interface_skips_scoped_routes_and_ours() {
        assert_eq!(default_interface(NETSTAT, "utun9").as_deref(), Some("utun4"), "another VPN's default is the uplink");
        assert_eq!(default_interface(NETSTAT, "utun4").as_deref(), Some("en0"));
        let old = "Internet:\nDestination        Gateway            Flags        Refs      Use   Netif Expire\ndefault            192.168.1.1        UGSc           12        0     en1\n";
        assert_eq!(default_interface(old, "utun9").as_deref(), Some("en1"), "older netstat with Refs/Use columns");
        assert_eq!(default_interface("Internet:\nDestination Gateway Flags Netif Expire\n", "x"), None);
    }

    #[test]
    fn pf_text() {
        assert_eq!(pf_token("No ALTQ support in kernel\nALTQ related functions disabled\npf enabled\nToken : 7369846431616045091\n").as_deref(), Some("7369846431616045091"));
        assert_eq!(pf_token("pfctl: pf already enabled\n"), None);
        let base = pf_rules("utun9", 26869, false);
        assert!(base.contains("pass quick on utun9 all\n") && base.contains("group 26869") && base.contains("port 53"));
        assert!(!base.contains("block drop out quick all"));
        let kill = pf_rules("utun9", 26869, true);
        assert!(kill.ends_with("block drop out quick all\n") && kill.contains("192.168.0.0/16"));
        assert!(kill.find("port 53").unwrap() < kill.find("192.168.0.0/16").unwrap(), "LAN resolvers stay blocked under the kill switch");
    }

    #[test]
    fn utun_addresses() {
        assert_eq!(p2p("172.19.0.1/30"), Some(("172.19.0.1".into(), "172.19.0.2".into(), "255.255.255.252".into())));
        assert_eq!(p2p("fdfe::1/126"), None);
    }
}
