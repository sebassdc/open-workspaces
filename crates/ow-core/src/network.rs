//! Policy lives exclusively in the worker's disposable network namespace.
use anyhow::{Result, ensure};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn nft(text: &str) -> Result<()> {
    let mut child = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "network policy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
pub fn initialize() -> Result<()> {
    nft(r#"table inet ow {
 set guests { type ifname . ipv4_addr; }
 set hosts { type ifname . ipv4_addr; }
 set local_addresses { type ipv4_addr; }
 set protected { type ipv4_addr; flags interval; elements = { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 198.18.0.0/15, 224.0.0.0/4, 240.0.0.0/4 }; }
 chain validate { iifname . ip saddr @guests return; drop; }
 chain input { type filter hook input priority 0; policy accept;
  iifname "ow*" jump validate
  iifname "ow*" iifname . ip daddr @hosts accept
  iifname "ow*" drop
 }
 chain forward { type filter hook forward priority 0; policy drop;
  iifname "ow*" jump validate
  iifname "ow*" oifname "tap-egress" ip daddr 10.0.2.3 meta l4proto { tcp, udp } th dport 53 accept
  iifname "ow*" ip daddr @local_addresses drop
  iifname "ow*" ip daddr @protected drop
  iifname "ow*" oifname "tap-egress" meta l4proto { tcp, udp } accept
  iifname "tap-egress" oifname "ow*" ct state established,related accept
 }
 chain nat { type nat hook postrouting priority srcnat; policy accept;
  oifname "tap-egress" ip saddr 198.18.0.0/15 masquerade
 }
}
"#)
}
pub fn register(index: u32) -> Result<()> {
    ensure!(index < 16384, "invalid network index");
    nft(&format!(
        "add element inet ow guests {{ \"ow{index}\" . 198.18.{}.{} }}\nadd element inet ow hosts {{ \"ow{index}\" . 198.18.{}.{} }}",
        index / 64,
        index % 64 * 4 + 2,
        index / 64,
        index % 64 * 4 + 1
    ))
}
pub fn unregister(index: u32) {
    let _ = nft(&format!(
        "delete element inet ow guests {{ \"ow{index}\" . 198.18.{}.{} }}\ndelete element inet ow hosts {{ \"ow{index}\" . 198.18.{}.{} }}",
        index / 64,
        index % 64 * 4 + 2,
        index / 64,
        index % 64 * 4 + 1
    ));
}
pub fn enable(addresses: &serde_json::Value) -> Result<()> {
    for address in addresses
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing host address list"))?
    {
        let ip: std::net::Ipv4Addr = address
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid host IP"))?
            .parse()?;
        nft(&format!("add element inet ow local_addresses {{ {ip} }}"))?;
    }
    crate::vm::host(
        "ip",
        &[
            "route",
            "replace",
            "default",
            "via",
            "10.0.2.2",
            "dev",
            "tap-egress",
        ],
    )?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1")?;
    Ok(())
}
pub fn enabled() -> bool {
    std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward").is_ok_and(|s| s.trim() == "1")
}
