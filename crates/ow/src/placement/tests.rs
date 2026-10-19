use super::*;
use serde_json::json;
fn node(id: &str) -> Candidate {
    Candidate {
        id: id.into(),
        online: true,
        active: true,
        pool: "default".into(),
        joined: 1,
        free: Capacity {
            memory_mib: 4096,
            cpus: 4,
            slots: 2,
            disk_bytes: 64 * crate::common::GIB,
        },
        capabilities: json!({"backend":"firecracker","images":["tools@1"],"max_guest_memory_mib":4096,"max_guest_vcpus":4,"disk_resize_v1":true,"disk_observed_unix_s":100,"worker":{"protocol":2},"placement_fencing_v1":true,"cache_eviction_v1":true,"cache_inventory_v1":true,"cache_observed_unix_s":100,"compat":{"runtime":"r","cpu":"c","kernel":"k"}}),
    }
}
static COMPAT: std::sync::LazyLock<Value> =
    std::sync::LazyLock::new(|| json!({"runtime":"r","cpu":"c","kernel":"k"}));
fn demand() -> Demand<'static> {
    Demand {
        requested: None,
        pool: "default",
        memory_mib: 1024,
        cpus: 2,
        disk_bytes: 32 * crate::common::GIB,
        image: "tools@1",
        image_digest: Some("image"),
        parent_digest: Some("parent"),
        capture_digest: Some("capture"),
        compatibility: Some(&COMPAT),
        managed: true,
        disk_sizing: true,
        now: 100,
    }
}
#[test]
fn placement_filters_before_cache_ranking_and_never_admits_legacy_managed_work() {
    let d = demand();
    for reason in 0..16 {
        let good = node("good");
        let mut bad = node("bad");
        bad.capabilities["cache_entries"] = json!([{"digest":"capture"}]);
        match reason {
            0 => bad.online = false,
            1 => bad.active = false,
            2 => bad.pool = "other".into(),
            3 => bad.capabilities["drain"] = json!(true),
            4 => bad.capabilities["controller_drain"] = json!(true),
            5 => bad.free.memory_mib = 512,
            6 => bad.free.cpus = 1,
            7 => bad.free.slots = 0,
            8 => bad.free.disk_bytes = 1,
            9 => bad.capabilities["disk_observed_unix_s"] = json!(80),
            10 => bad.capabilities["images"] = json!([]),
            11 => bad.capabilities["max_guest_vcpus"] = json!(1),
            12 => bad.capabilities["placement_fencing_v1"] = json!(false),
            13 => bad.capabilities["cache_inventory_v1"] = json!(null),
            14 => bad.capabilities["disk_resize_v1"] = json!(false),
            _ => bad.capabilities["operations"] = json!(["status"]),
        }
        assert_eq!(
            choose(&[bad, good], &d).unwrap().id,
            "good",
            "reason {reason}"
        );
    }
    let mut n = node("wrong");
    n.capabilities["compat"] = json!({"runtime":"wrong"});
    let compat = json!({"runtime":"r","cpu":"c","kernel":"k"});
    assert!(
        choose(
            &[n],
            &Demand {
                compatibility: Some(&compat),
                ..d
            }
        )
        .is_none()
    );
}
#[test]
fn placement_capture_then_image_or_parent_then_best_fit_age_and_id() {
    let d = demand();
    let mut a = node("a");
    let mut z = node("z");
    z.capabilities["cache_entries"] = json!([{"digest":"image"}]);
    assert_eq!(choose(&[a.clone(), z.clone()], &d).unwrap().id, "z");
    a.capabilities["cache_entries"] = json!([{"digest":"capture"}]);
    assert_eq!(choose(&[z.clone(), a.clone()], &d).unwrap().id, "a");
    a.capabilities["cache_entries"] = json!([{"digest":"parent"}]);
    a.free.memory_mib = 2048;
    assert_eq!(choose(&[z.clone(), a.clone()], &d).unwrap().id, "a");
    z.free.memory_mib = 2048;
    z.joined = 0;
    assert_eq!(choose(&[a.clone(), z.clone()], &d).unwrap().id, "z");
    z.joined = 1;
    for nodes in [[a.clone(), z.clone()], [z.clone(), a.clone()]] {
        assert_eq!(choose(&nodes, &d).unwrap().id, "a");
    }
}
#[test]
fn placement_stale_or_future_cache_hints_are_cold_and_cold_capable_node_can_pull() {
    let d = demand();
    let a = node("a");
    let mut z = node("z");
    z.capabilities["cache_entries"] = json!([{"digest":"capture"}]);
    for observed in [0, 84, 101] {
        z.capabilities["cache_observed_unix_s"] = json!(observed);
        assert_eq!(choose(&[z.clone(), a.clone()], &d).unwrap().id, "a");
    }
    z.capabilities["cache_entries"] = json!([]);
    z.capabilities["images"] = json!([]);
    z.capabilities["image_store_v1"] = json!(true);
    assert!(choose(&[z], &d).is_some());
}
