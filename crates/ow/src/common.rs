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
