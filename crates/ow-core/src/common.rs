use anyhow::{Result, ensure};
use std::{fs::File, io::Read};
pub fn identifier(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 32
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "IDs must contain 1–32 ASCII letters, digits, hyphens or underscores"
    );
    Ok(())
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn nonce() -> Result<String> {
    let mut bytes = [0u8; 12];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// Shared admission ceilings; defaults remain deliberately smaller.
pub const HOST_MEMORY_MIB: u32 = 65536;
pub const HOST_CPUS: u32 = 64;
pub const HOST_SLOTS: u32 = 8;
pub const SHAPES_MIB: [u32; 7] = [256, 512, 1024, 2048, 4096, 8192, 16384];
pub fn host_limits(memory: u32, slots: u32, cpus: u32) -> Result<()> {
    ensure!(
        (256..=HOST_MEMORY_MIB).contains(&memory)
            && (1..=HOST_SLOTS).contains(&slots)
            && (1..=HOST_CPUS).contains(&cpus),
        "host limits out of range (256–65536 MiB, 1–8 slots, 1–64 vCPU)"
    );
    Ok(())
}
pub fn resources(memory: u32, cpus: u32) -> Result<()> {
    ensure!(
        SHAPES_MIB.contains(&memory),
        "memory must be 256, 512, 1024, 2048, 4096, 8192 or 16384 MiB"
    );
    ensure!((1..=16).contains(&cpus), "CPU count must be 1–16");
    Ok(())
}
/// Missing fields use defaults; explicitly invalid/null fields fail before durable intent.
pub fn requested_resources(request: &serde_json::Value) -> Result<(u32, u32)> {
    fn field(v: &serde_json::Value, key: &str, default: u32) -> Result<u32> {
        match v.get(key) {
            None => Ok(default),
            Some(n) => Ok(u32::try_from(
                n.as_u64().ok_or_else(|| anyhow::anyhow!("invalid {key}"))?,
            )?),
        }
    }
    let memory = field(request, "memory_mib", 256)?;
    let cpus = field(request, "vcpu_count", 1)?;
    resources(memory, cpus)?;
    Ok((memory, cpus))
}

/// Protocol v1 peers without optional shape fields retain the original worker ceiling.
pub fn guest_limits(v: &serde_json::Value) -> Result<(u64, u64)> {
    fn limit(v: &serde_json::Value, key: &str, fallback: u64, ceiling: u64) -> Result<u64> {
        let n = match v.get(key) {
            None => fallback,
            Some(n) => n.as_u64().ok_or_else(|| anyhow::anyhow!("invalid {key}"))?,
        };
        ensure!((1..=ceiling).contains(&n), "invalid {key}");
        Ok(n)
    }
    Ok((
        limit(v, "max_guest_memory_mib", 2048, 16384)?,
        limit(v, "max_guest_vcpus", 4, 16)?,
    ))
}
pub fn guest_shape(v: &serde_json::Value, memory: u64, cpus: u64) -> Result<()> {
    if v["backend"] == "apple-virtualization" {
        ensure!(
            [512, 1024, 2048].contains(&memory) && (1..=2).contains(&cpus),
            "unsupported Mac guest shape"
        );
    }
    let (ram, cpu) = guest_limits(v)?;
    ensure!(
        memory <= ram && cpus <= cpu,
        "shape exceeds worker per-guest ceiling"
    );
    Ok(())
}
#[cfg(test)]
mod capacity_tests {
    use super::*;
    #[test]
    fn mixed_version_guest_limits() {
        use serde_json::json;
        let legacy = json!({});
        assert_eq!(guest_limits(&legacy).unwrap(), (2048, 4));
        assert!(guest_shape(&legacy, 2048, 4).is_ok());
        assert!(guest_shape(&legacy, 4096, 2).is_err());
        assert!(guest_shape(&legacy, 256, 5).is_err());
        let new = json!({"max_guest_memory_mib":16384,"max_guest_vcpus":16});
        assert!(guest_shape(&new, 4096, 2).is_ok());
        for bad in [
            json!({"max_guest_memory_mib":null}),
            json!({"max_guest_vcpus":"16"}),
            json!({"max_guest_vcpus":17}),
        ] {
            assert!(guest_limits(&bad).is_err());
        }
    }
    #[test]
    fn ceilings_and_shapes_are_bounded() {
        for memory in SHAPES_MIB {
            assert!(resources(memory, 16).is_ok());
        }
        for (ram, cpu) in [
            (0, 1),
            (65536, 1),
            (4097, 1),
            (4096, 0),
            (4096, 17),
            (u32::MAX, 1),
        ] {
            assert!(resources(ram, cpu).is_err());
        }
        assert!(host_limits(65536, 8, 64).is_ok());
        for (ram, slots, cpu) in [
            (65537, 8, 64),
            (65536, 9, 64),
            (65536, 8, 65),
            (u32::MAX, 1, 1),
        ] {
            assert!(host_limits(ram, slots, cpu).is_err());
        }
    }
}

/// Explicit Mac v1 slice; absence preserves legacy Linux protocol semantics.
pub const MAC_OPERATIONS: &[&str] = &[
    "status", "stats", "list", "inspect", "create", "start", "stop", "exec", "put", "get",
    "terminal",
];
pub fn node_operation(v: &serde_json::Value, op: &str) -> Result<()> {
    if let Some(operations) = v.get("operations") {
        ensure!(
            operations
                .as_array()
                .is_some_and(|a| a.iter().any(|i| i.as_str() == Some(op))),
            "node capability does not support {op}"
        );
    } else {
        ensure!(
            v["backend"] != "apple-virtualization",
            "Mac operations capability required"
        );
    }
    Ok(())
}
pub fn node_capabilities(v: &serde_json::Value) -> Result<()> {
    guest_limits(v)?;
    ensure!(
        v["type"] == "heartbeat" && v["protocol"] == 1,
        "invalid heartbeat protocol"
    );
    ensure!(
        (256..=HOST_MEMORY_MIB as u64).contains(&v["memory_mib"].as_u64().unwrap_or(0))
            && (1..=HOST_SLOTS as u64).contains(&v["slots"].as_u64().unwrap_or(0))
            && (1..=HOST_CPUS as u64).contains(&v["vcpus"].as_u64().unwrap_or(0)),
        "invalid node budgets"
    );
    let mac = v["backend"] == "apple-virtualization"
        && v["arch"] == "aarch64"
        && v["runtime"] == "apple-vz-v1";
    let linux = v["backend"] == "firecracker"
        && v["arch"] == "x86_64"
        && v["runtime"] == "firecracker-v1.17.0";
    ensure!(
        mac || linux,
        "unsupported backend/architecture/runtime tuple"
    );
    ensure!(
        v["images"].as_array().is_some_and(|a| a.len() <= 3
            && a.iter().all(|i| if mac {
                i == "ubuntu-arm64"
            } else {
                matches!(i.as_str(), Some("alpine" | "arch" | "ubuntu"))
            })),
        "incompatible prepared image"
    );
    if mac {
        ensure!(
            v["memory_mib"].as_u64().unwrap() <= 2048
                && v["slots"] == 1
                && v["vcpus"].as_u64().unwrap() <= 2
                && v["guest_ssh_v1"] == false,
            "Mac slice ceiling/SSH violation"
        );
        ensure!(guest_limits(v)? == (2048, 2), "invalid Mac guest ceiling");
        ensure!(
            v["operations"]
                .as_array()
                .is_some_and(|a| a.len() == MAC_OPERATIONS.len()
                    && MAC_OPERATIONS
                        .iter()
                        .all(|op| a.iter().any(|i| i.as_str() == Some(op)))),
            "invalid Mac operations"
        );
    }
    Ok(())
}
#[cfg(test)]
mod backend_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn tuples_and_legacy_operations() {
        let legacy = json!({"type":"heartbeat","protocol":1,"backend":"firecracker","arch":"x86_64","runtime":"firecracker-v1.17.0","memory_mib":1024,"slots":2,"vcpus":2,"images":["ubuntu"]});
        assert!(node_capabilities(&legacy).is_ok());
        assert!(node_operation(&legacy, "fork").is_ok());
        let mac = json!({"type":"heartbeat","protocol":1,"backend":"apple-virtualization","arch":"aarch64","runtime":"apple-vz-v1","memory_mib":2048,"slots":1,"vcpus":2,"images":["ubuntu-arm64"],"max_guest_memory_mib":2048,"max_guest_vcpus":2,"guest_ssh_v1":false,"operations":MAC_OPERATIONS});
        assert!(node_capabilities(&mac).is_ok());
        assert!(node_operation(&mac, "terminal").is_ok());
        for op in [
            "snapshot",
            "hibernate",
            "fork",
            "restore",
            "ssh",
            "ssh-info",
            "ssh-keys",
            "resize",
        ] {
            assert!(node_operation(&mac, op).is_err());
        }
        for (k, val) in [
            ("arch", json!("x86_64")),
            ("images", json!(["ubuntu"])),
            ("guest_ssh_v1", json!(true)),
            ("slots", json!(2)),
        ] {
            let mut bad = mac.clone();
            bad[k] = val;
            assert!(node_capabilities(&bad).is_err());
        }
    }
}
