//! Bounded, public projection of authenticated placement data; never expose host paths or tenant rows.
use crate::common;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Reservations include all owners and untracked runner usage, just as admission does.
pub fn node(inventory: &Value, reserved: Value, now: u64) -> Value {
    let cap = &inventory["capabilities"];
    let online = inventory["online"] == true && inventory["revoked"] != true;
    let draining = cap["drain"] == true;
    let limits = json!({"memory_mib":inventory["memory_mib"],"vcpus":cap["vcpus"],"slots":inventory["slots"]});
    let mut available = json!(null);
    if online && !draining {
        available = json!({});
        for key in ["memory_mib", "vcpus", "slots"] {
            available[key] = limits[key]
                .as_u64()
                .zip(reserved[key].as_u64())
                .map(|(limit, used)| json!(limit.saturating_sub(used)))
                .unwrap_or(json!(null));
        }
    }
    let (ram, cpus) = common::guest_limits(cap).unwrap_or((0, 0));
    let disk_observed = cap["disk_observed_unix_s"].as_u64();
    let disk_fresh = online && disk_observed.is_some_and(|t| t <= now && now - t <= 30);
    let disk = json!({
        "configured_gib":cap["max_disk_gib"].as_u64(),
        "reserved_bytes":cap["reserved_disk_bytes"].as_u64(),
        "host_reserve_gib":cap["min_free_gib"].as_u64(),
        "available_bytes":if disk_fresh && !draining {cap["available_disk_bytes"].as_u64()} else {None},
        "observed_unix_s":disk_observed,
        "fresh":disk_fresh
    });
    let workspace_headroom = if disk_fresh && !draining {
        cap["max_workspaces"]
            .as_u64()
            .zip(cap["workspace_count"].as_u64())
            .map(|(limit, used)| limit.saturating_sub(used))
    } else {
        None
    };
    let mut capabilities = json!({});
    for key in ["backend", "arch", "runtime"] {
        capabilities[key] = cap[key].clone();
    }
    capabilities["protocol"] = json!(cap["protocol"].as_u64());
    for key in ["guest_ssh_v1", "disk_resize_v1"] {
        capabilities[key] = json!(cap[key].as_bool());
    }
    capabilities["operations"] = cap["operations"]
        .as_array()
        .map(|ops| {
            json!(
                ops.iter()
                    .filter(|op| op.as_str().is_some_and(|op| [
                        "create",
                        "start",
                        "stop",
                        "inspect",
                        "exec",
                        "put",
                        "get",
                        "terminal",
                        "hibernate",
                        "list",
                        "stats",
                        "status",
                        "snapshot",
                        "snapshots",
                        "restore",
                        "fork",
                        "resize",
                        "ssh-info",
                        "ssh-keys"
                    ]
                    .contains(&op)))
                    .collect::<Vec<_>>()
            )
        })
        .unwrap_or(json!(null));
    json!({"id":inventory["id"],"online":online,"draining":draining,
        "last_seen_unix_s":inventory["last_seen_unix_s"],
        "configured":{"memory_mib":inventory["owner_limits"]["memory"],"vcpus":inventory["owner_limits"]["cpus"],"slots":inventory["owner_limits"]["slots"]},"effective":limits,"reserved":reserved,"available":available,
        "per_guest":{"memory_mib":common::SHAPES_MIB.iter().copied().filter(|m| u64::from(*m)<=ram && (cap["backend"]!="apple-virtualization" || [512,1024,2048].contains(m))).collect::<Vec<_>>(),"max_vcpus":cpus},
        "disk":disk,"workspaces":{"configured":cap["max_workspaces"].as_u64(),"observed":cap["workspace_count"].as_u64(),"available":workspace_headroom},"capabilities":capabilities,"images":cap["images"].as_array().cloned().unwrap_or_default()})
}

fn fits(node: &Value, memory: u64, cpus: u64) -> bool {
    node["workspaces"]["available"].as_u64() != Some(0)
        && node["available"]["memory_mib"]
            .as_u64()
            .is_some_and(|n| n >= memory)
        && node["available"]["vcpus"]
            .as_u64()
            .is_some_and(|n| n >= cpus)
        && node["available"]["slots"].as_u64().is_some_and(|n| n > 0)
        && node["per_guest"]["memory_mib"]
            .as_array()
            .is_some_and(|a| a.contains(&json!(memory)))
        && node["per_guest"]["max_vcpus"]
            .as_u64()
            .is_some_and(|n| n >= cpus)
}

pub fn images(nodes: &[Value]) -> Value {
    let mut names: BTreeSet<String> = ["alpine", "arch", "ubuntu", "ubuntu-arm64"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for node in nodes {
        if let Some(images) = node["images"].as_array() {
            names.extend(images.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    json!(names.into_iter().map(|name| {
        let mut choices = Vec::new();
        for node in nodes {
            let prepared = node["images"].as_array().is_some_and(|a| a.contains(&json!(name)));
            let mac = node["capabilities"]["backend"] == "apple-virtualization";
            let disk_capable = mac || node["capabilities"]["disk_resize_v1"] == true;
            let create_capable = node["capabilities"]["operations"].is_null()
                || node["capabilities"]["operations"].as_array().is_some_and(|a| a.contains(&json!("create")));
            let disk_fits = mac || node["disk"]["available_bytes"].as_u64().is_none_or(|n| n >= u64::from(common::DEFAULT_DISK_GIB)*common::GIB);
            let availability = if !prepared {"missing_image"} else if node["online"] != true {"offline"}
                else if node["draining"] == true {"draining"} else if !disk_capable || !create_capable {"unsupported_capability"}
                else if !fits(node, if mac {512} else {256}, 1) || !disk_fits {"insufficient_capacity"} else {"available"};
            let development_choice = if availability == "available" && matches!(name.as_str(), "ubuntu" | "arch" | "ubuntu-arm64") && fits(node, 2048, 2) {
                let mut choice = json!({"image":name,"memory_mib":2048,"vcpu_count":2,"placement":"automatic"});
                if !mac {choice["disk_gib"] = json!(common::DEFAULT_DISK_GIB);}
                choice
            } else {json!(null)};
            choices.push(json!({"node":node["id"],"availability":availability,"disk_capacity_known":node["disk"]["available_bytes"].is_u64(),"development_choice":development_choice}));
        }
        let available = choices.iter().any(|c| c["availability"] == "available");
        json!({"name":name,"available":available,"nodes":choices})
    }).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runner(online: bool, memory: u64) -> Value {
        node(
            &json!({"id":"runner","online":online,"last_seen_unix_s":100,"memory_mib":4096,"slots":2,"owner_limits":{"memory":4096,"slots":2,"cpus":4},"capabilities":{"backend":"firecracker","vcpus":4,"max_guest_memory_mib":4096,"max_guest_vcpus":4,"images":["ubuntu"],"disk_resize_v1":true,"max_disk_gib":256,"reserved_disk_bytes":32*common::GIB,"available_disk_bytes":64*common::GIB,"disk_observed_unix_s":100,"secret":"never public"}}),
            json!({"memory_mib":memory,"vcpus":0,"slots":0}),
            100,
        )
    }
    #[test]
    fn discovery_distinguishes_offline_full_missing_and_usable() {
        let online = runner(true, 1024);
        let output = images(&[online.clone()]);
        let ubuntu = output
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "ubuntu")
            .unwrap();
        assert_eq!(ubuntu["nodes"][0]["development_choice"]["memory_mib"], 2048);
        assert_eq!(online["available"]["memory_mib"], 3072);
        assert!(!online.to_string().contains("never public"));
        for (node, expected) in [
            (runner(false, 0), "offline"),
            (runner(true, 4096), "insufficient_capacity"),
        ] {
            let output = images(&[node.clone()]);
            let ubuntu = output
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["name"] == "ubuntu")
                .unwrap();
            assert_eq!(ubuntu["available"], false);
            assert_eq!(ubuntu["nodes"][0]["availability"], expected);
            if expected == "offline" {
                assert!(node["available"].is_null());
                assert!(node["disk"]["available_bytes"].is_null());
            }
        }
        assert_eq!(
            output
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["name"] == "arch")
                .unwrap()["nodes"][0]["availability"],
            "missing_image"
        );
    }
    #[test]
    fn mac_choices_keep_backend_shape_and_disk_restrictions() {
        let n = node(
            &json!({"id":"mac","online":true,"memory_mib":2048,"slots":1,"capabilities":{"backend":"apple-virtualization","arch":"aarch64","memory_mib":2048,"vcpus":2,"max_guest_memory_mib":2048,"max_guest_vcpus":2,"images":["ubuntu-arm64"],"operations":["create","exec","host-secret"]}}),
            json!({"memory_mib":0,"vcpus":0,"slots":0}),
            100,
        );
        assert_eq!(n["per_guest"]["memory_mib"], json!([512, 1024, 2048]));
        assert!(!n.to_string().contains("host-secret"));
        let output = images(&[n]);
        let image = output
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "ubuntu-arm64")
            .unwrap();
        assert_eq!(image["available"], true);
        assert_eq!(image["nodes"][0]["disk_capacity_known"], false);
        assert!(image["nodes"][0]["development_choice"]["disk_gib"].is_null());
        assert_eq!(image["nodes"][0]["development_choice"]["memory_mib"], 2048);
    }

    #[test]
    fn stale_observations_and_draining_never_offer_fresh_headroom() {
        let mut raw = json!({"id":"runner","online":true,"memory_mib":4096,"slots":2,"capabilities":{"vcpus":4,"disk_observed_unix_s":60,"available_disk_bytes":64*common::GIB}});
        let reserved = json!({"memory_mib":0,"vcpus":0,"slots":0});
        assert!(node(&raw, reserved.clone(), 100)["disk"]["available_bytes"].is_null());
        raw["capabilities"]["disk_observed_unix_s"] = json!(101);
        assert!(node(&raw, reserved.clone(), 100)["disk"]["available_bytes"].is_null());
        raw["capabilities"]["drain"] = json!(true);
        assert!(node(&raw, reserved, 100)["available"].is_null());
        let mut n = runner(true, 0);
        n["workspaces"]["available"] = json!(0);
        assert!(
            !images(&[n])
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["available"] == true)
        );
    }

    #[test]
    fn disk_and_capabilities_bound_choices_without_inventing_legacy_capacity() {
        let mut n = runner(true, 0);
        n["disk"]["available_bytes"] = json!(common::GIB);
        assert!(
            !images(&[n.clone()])
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["available"] == true)
        );
        n["disk"]["available_bytes"] = json!(null);
        let out = images(&[n.clone()]);
        let ubuntu = out
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "ubuntu")
            .unwrap();
        assert_eq!(ubuntu["nodes"][0]["disk_capacity_known"], false);
        n["capabilities"]["disk_resize_v1"] = json!(false);
        assert_eq!(
            images(&[n])
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["name"] == "ubuntu")
                .unwrap()["nodes"][0]["availability"],
            "unsupported_capability"
        );
    }
}
