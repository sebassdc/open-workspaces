use anyhow::{Context, Result, ensure};
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

/// A machine address: `name`, or `owner@example.com/name` for a machine shared with you.
pub fn address(address: &str) -> Result<()> {
    match address.split_once('/') {
        None => identifier(address),
        Some((owner, name)) => {
            email(owner)?;
            identifier(name)
        }
    }
}

pub fn email(email: &str) -> Result<()> {
    ensure!(
        email.len() <= 254
            && email
                .split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty()
                    && !domain.is_empty()
                    && !domain.contains('@'))
            && email
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b)),
        "invalid email address"
    );
    Ok(())
}

#[cfg(test)]
#[test]
fn addresses() {
    for ok in ["box", "a@b.test/box", "a.b+c@x-y.test/my_box"] {
        assert!(address(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "a/b",
        "@b/box",
        "a@/box",
        "a@b@c/box",
        "a@b/../x",
        "a@b/c/d",
        "a b@c/x",
        "a@b/",
    ] {
        assert!(address(bad).is_err(), "{bad}");
    }
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
/// Admission budget, not a CPU quota or a guarantee of dedicated cores.
pub fn vcpu_budget(host_cpus: u32, ratio: u32) -> Result<u32> {
    ensure!(matches!(ratio, 1 | 2), "vCPU overcommit must be 1 or 2");
    ensure!(host_cpus > 0, "host CPU count must be positive");
    Ok(host_cpus.saturating_mul(ratio).min(HOST_CPUS))
}

#[cfg(test)]
mod overcommit_tests {
    use super::*;

    #[test]
    fn ratio_scales_only_cpu_and_keeps_the_protocol_ceiling() {
        assert_eq!(vcpu_budget(8, 1).unwrap(), 8);
        assert_eq!(vcpu_budget(8, 2).unwrap(), 16);
        assert_eq!(vcpu_budget(48, 2).unwrap(), 64);
        assert_eq!(vcpu_budget(u32::MAX, 2).unwrap(), 64);
        for ratio in [0, 3, u32::MAX] {
            assert!(vcpu_budget(8, ratio).is_err());
        }
        assert!(vcpu_budget(0, 1).is_err());
    }
}
pub const DEFAULT_DISK_GIB: u32 = 32;
pub const MAX_DISK_GIB: u32 = 16384;
pub const GIB: u64 = 1024 * 1024 * 1024;

/// CLI sizes are whole GiB, optionally suffixed with G or GiB.
pub fn parse_disk_size(value: &str) -> Result<u32> {
    let size = value
        .strip_suffix("GiB")
        .or_else(|| value.strip_suffix('G'))
        .unwrap_or(value);
    let n: u32 = size
        .parse()
        .context("disk size must be whole GiB (for example 64GiB)")?;
    ensure!(
        (1..=MAX_DISK_GIB).contains(&n),
        "disk must be 1–{MAX_DISK_GIB} GiB"
    );
    Ok(n)
}

/// Validate before durable intent; JSON accepts integer GiB, never strings/null.
pub fn requested_disk(request: &serde_json::Value) -> Result<Option<u32>> {
    request
        .get("disk_gib")
        .map(|v| {
            let n = v.as_u64().context("disk_gib must be an integer")?;
            ensure!(
                (1..=u64::from(MAX_DISK_GIB)).contains(&n),
                "disk_gib out of range"
            );
            Ok(n as u32)
        })
        .transpose()
}
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

/// Host-runner protocol this binary speaks, and the oldest a controller built from it
/// accepts. Controllers upgrade first; see docs/UPGRADES.md for when to bump.
pub const PROTOCOL: u64 = 1;
pub const PROTOCOL_MIN: u64 = 1;
/// Linux runtimes a controller admits. A new Firecracker is added here one release
/// before any runner reports it.
pub const RUNTIMES: &[&str] = &["firecracker-v1.17.0"];
/// The runtime this release's worker runs and resumes captures with.
pub const RUNTIME: &str = "firecracker-v1.17.0";
/// Agent-worker protocol (control.sock) and the oldest worker the agent still drives.
pub const WORKER_PROTOCOL: u64 = 1;
pub const WORKER_PROTOCOL_MIN: u64 = 1;

/// Whether this agent can drive a worker: its status's `worker_protocol` (absent from
/// workers before 0.5.0, which speak 1) inside WORKER_PROTOCOL_MIN..=WORKER_PROTOCOL.
pub fn worker_compatible(status: &serde_json::Value) -> Result<()> {
    let protocol = status
        .get("worker_protocol")
        .map_or(Some(1), |v| v.as_u64())
        .unwrap_or(0);
    ensure!(
        (WORKER_PROTOCOL_MIN..=WORKER_PROTOCOL).contains(&protocol),
        "worker protocol {protocol} unsupported; this agent drives {WORKER_PROTOCOL_MIN}..{WORKER_PROTOCOL}"
    );
    Ok(())
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
    ensure!(v["type"] == "heartbeat", "invalid heartbeat");
    let protocol = v["protocol"].as_u64().unwrap_or(0);
    ensure!(
        (PROTOCOL_MIN..=PROTOCOL).contains(&protocol),
        "protocol {protocol} unsupported; this controller accepts {PROTOCOL_MIN}..{PROTOCOL}"
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
        && v["runtime"].as_str().is_some_and(|r| RUNTIMES.contains(&r));
    ensure!(
        mac || linux,
        "unsupported backend/architecture/runtime tuple"
    );
    ensure!(
        // Linux runners add golden images (`ow image build`) to the built-in profiles.
        v["images"]
            .as_array()
            .is_some_and(|a| a.len() <= if mac { 3 } else { 3 + 32 }
                && a.iter().all(|i| if mac {
                    i == "ubuntu-arm64"
                } else {
                    i.as_str()
                        .is_some_and(|i| i != "ubuntu-arm64" && crate::store::image_name(i).is_ok())
                })),
        "incompatible prepared image"
    );
    if let Some(digests) = v.get("image_digests").filter(|v| !v.is_null()) {
        ensure!(
            digests.as_array().is_some_and(|a| a.len() <= 32
                && a.iter().all(|i| i["name"]
                    .as_str()
                    .is_some_and(|name| crate::store::image_name(name).is_ok())
                    && i["digest"]
                        .as_str()
                        .is_some_and(|digest| crate::store::valid_digest(digest).is_ok()))),
            "invalid image digest inventory"
        );
    }
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
        let mut golden = legacy.clone();
        golden["images"] = json!(["alpine", "ubuntu", "devtools"]);
        assert!(node_capabilities(&golden).is_ok());
        golden["images"] = json!(["tools@1"]);
        golden["image_digests"] = json!([{"name":"tools@1","digest":"a".repeat(64)}]);
        assert!(node_capabilities(&golden).is_ok());
        golden["image_digests"][0]["digest"] = json!("bad");
        assert!(node_capabilities(&golden).is_err());
        golden.as_object_mut().unwrap().remove("image_digests");
        for images in [json!(["ubuntu-arm64"]), json!(["../x"]), json!([1])] {
            golden["images"] = images;
            assert!(node_capabilities(&golden).is_err());
        }
        assert!(node_operation(&legacy, "fork").is_ok());
        for (status, compatible) in [
            (json!({}), true),
            (json!({"worker_protocol": WORKER_PROTOCOL}), true),
            (json!({"worker_protocol": WORKER_PROTOCOL + 1}), false),
            (json!({"worker_protocol": 0}), false),
            (json!({"worker_protocol": "1"}), false),
        ] {
            assert_eq!(worker_compatible(&status).is_ok(), compatible, "{status}");
        }
        // Protocol window, runtime admission, and optional fields an older peer adds.
        let mut beat = legacy.clone();
        for protocol in [PROTOCOL_MIN, PROTOCOL] {
            beat["protocol"] = json!(protocol);
            assert!(node_capabilities(&beat).is_ok());
        }
        for protocol in [json!(0), json!(PROTOCOL + 1), json!("1"), json!(null)] {
            beat["protocol"] = protocol;
            let error = node_capabilities(&beat).unwrap_err().to_string();
            assert!(error.contains("unsupported"), "{error}");
        }
        beat = legacy.clone();
        beat["runtime"] = json!("firecracker-v9.9.9");
        assert!(node_capabilities(&beat).is_err());
        beat = legacy.clone();
        beat["version"] = json!("0.5.0");
        beat["worker"] = json!({"version":"0.5.0","protocol":1});
        beat["drain"] = json!(true);
        beat["future_field"] = json!({"any":"shape"});
        assert!(node_capabilities(&beat).is_ok());
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

#[cfg(test)]
mod disk_size_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn sizes_are_bounded_whole_gib() {
        for text in ["64", "64G", "64GiB"] {
            assert_eq!(parse_disk_size(text).unwrap(), 64);
        }
        for text in [
            "0",
            "-1",
            "1.5G",
            "64GB",
            "16MiB",
            "16385",
            "999999999999999999999",
        ] {
            assert!(parse_disk_size(text).is_err(), "{text}");
        }
        assert_eq!(requested_disk(&json!({})).unwrap(), None);
        assert_eq!(requested_disk(&json!({"disk_gib":32})).unwrap(), Some(32));
        for bad in [
            json!(0),
            json!(null),
            json!(-1),
            json!(1.5),
            json!("64"),
            json!(u64::MAX),
        ] {
            assert!(requested_disk(&json!({"disk_gib":bad})).is_err());
        }
    }
}
