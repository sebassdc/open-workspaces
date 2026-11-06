//! Private egress exceptions (docs/NETWORKING.md#private-destinations): exact private IPv4
//! addresses and TCP ports that boxes may reach although the worker's policy drops private
//! ranges. The controller holds them per placement pool (`network.private_egress`); the
//! runner's worker applies the list it is given to its nftables set.
use anyhow::{Result, ensure};
use serde_json::Value;
use std::net::Ipv4Addr;

/// At most this many destinations per runner.
pub const MAX_DESTINATIONS: usize = 64;

/// One `ADDRESS:PORT`: an exact private IPv4 address (10/8, 172.16/12, 192.168/16 or
/// 100.64/10) and a TCP port. Loopback, link-local and metadata, the guests' own range,
/// multicast and reserved addresses never qualify, and public addresses are already allowed.
pub fn destination(text: &str) -> Result<(Ipv4Addr, u16)> {
    let parsed = text
        .split_once(':')
        .and_then(|(ip, port)| Some((ip.parse::<Ipv4Addr>().ok()?, port.parse::<u16>().ok()?)));
    let Some((ip, port)) = parsed else {
        anyhow::bail!("{text:?} is not ADDRESS:PORT (an IPv4 address and a TCP port)");
    };
    let [a, b, ..] = ip.octets();
    ensure!(
        a == 10
            || (a == 172 && (16..32).contains(&b))
            || (a == 192 && b == 168)
            || (a == 100 && (64..128).contains(&b)),
        "{ip} is not a private address (10/8, 172.16/12, 192.168/16 or 100.64/10)"
    );
    ensure!(port != 0, "{text}: the port is 1 to 65535");
    Ok((ip, port))
}

/// A list of `ADDRESS:PORT` strings, checked, sorted and without duplicates.
pub fn destinations(value: &Value) -> Result<Vec<(Ipv4Addr, u16)>> {
    let items = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected a list of ADDRESS:PORT strings"))?;
    let mut list = items
        .iter()
        .map(|item| {
            destination(
                item.as_str()
                    .ok_or_else(|| anyhow::anyhow!("expected ADDRESS:PORT strings"))?,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    list.sort();
    list.dedup();
    ensure!(
        list.len() <= MAX_DESTINATIONS,
        "at most {MAX_DESTINATIONS} private destinations"
    );
    Ok(list)
}

/// Their text form, as the worker reports them.
pub fn text(list: &[(Ipv4Addr, u16)]) -> Vec<String> {
    list.iter()
        .map(|(ip, port)| format!("{ip}:{port}"))
        .collect()
}

/// The nftables transaction that makes the worker's set exactly `list`.
pub fn nft(list: &[(Ipv4Addr, u16)]) -> String {
    let mut text = "flush set inet ow private_egress\n".to_owned();
    if !list.is_empty() {
        let elements = list
            .iter()
            .map(|(ip, port)| format!("{ip} . {port}"))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "add element inet ow private_egress {{ {elements} }}\n"
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_exact_private_addresses_with_a_port_qualify() {
        for ok in [
            "10.9.5.17:443",
            "172.16.0.1:5432",
            "172.31.255.1:1",
            "192.168.1.2:65535",
            "100.64.0.9:443",
        ] {
            assert!(destination(ok).is_ok(), "{ok}");
        }
        for bad in [
            "10.9.5.17",          // no port
            "10.9.5.17:0",        // port 0
            "10.9.5.17:70000",    // not a port
            "10.9.0.0/16:443",    // a range, not an address
            "8.8.8.8:53",         // public: already allowed
            "127.0.0.1:80",       // loopback
            "169.254.169.254:80", // the runner's instance metadata
            "198.18.0.1:22",      // the guests' own range
            "172.32.0.1:443",     // outside 172.16/12
            "100.128.0.1:443",    // outside 100.64/10
            "224.0.0.1:443",      // multicast
            "0.0.0.0:443",
            "host.internal:443",
        ] {
            assert!(destination(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lists_are_sorted_deduplicated_and_bounded() {
        let list = destinations(&json!(["10.0.0.2:443", "10.0.0.1:443", "10.0.0.2:443"])).unwrap();
        assert_eq!(text(&list), ["10.0.0.1:443", "10.0.0.2:443"]);
        assert!(destinations(&json!("10.0.0.1:443")).is_err());
        assert!(destinations(&json!([443])).is_err());
        let many: Vec<String> = (0..65).map(|i| format!("10.0.0.{i}:443")).collect();
        assert!(destinations(&json!(many)).is_err());
        assert!(destinations(&json!([])).unwrap().is_empty());
    }

    #[test]
    fn the_transaction_replaces_the_whole_set() {
        assert_eq!(nft(&[]), "flush set inet ow private_egress\n");
        let list = destinations(&json!(["10.0.0.1:443", "10.0.0.2:8443"])).unwrap();
        assert_eq!(
            nft(&list),
            "flush set inet ow private_egress\nadd element inet ow private_egress { 10.0.0.1 . 443, 10.0.0.2 . 8443 }\n"
        );
    }
}
