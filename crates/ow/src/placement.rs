//! Pure, deterministic ranking. Cache reports are hints and never supply capacity or authority.
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default)]
pub struct Capacity {
    pub memory_mib: u64,
    pub cpus: u64,
    pub slots: u64,
    pub disk_bytes: u64,
}
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub online: bool,
    pub active: bool,
    pub pool: String,
    pub capabilities: Value,
    /// Remaining effective budget after committed and in-flight logical reservations.
    pub free: Capacity,
    /// Stable enrollment order, never heartbeat order.
    pub joined: u64,
}
pub struct Demand<'a> {
    pub requested: Option<&'a str>,
    pub pool: &'a str,
    pub memory_mib: u64,
    pub cpus: u64,
    pub disk_bytes: u64,
    pub image: &'a str,
    pub image_digest: Option<&'a str>,
    pub parent_digest: Option<&'a str>,
    pub capture_digest: Option<&'a str>,
    pub compatibility: Option<&'a Value>,
    pub managed: bool,
    pub disk_sizing: bool,
    pub now: u64,
}
fn fresh(observed: Option<u64>, now: u64) -> bool {
    observed.is_some_and(|n| n <= now && now - n <= 15)
}
fn hit(cap: &Value, digest: Option<&str>, now: u64) -> bool {
    let Some(digest) = digest else { return false };
    cap["cache_inventory_v1"] == true
        && fresh(cap["cache_observed_unix_s"].as_u64(), now)
        && cap["cache_entries"]
            .as_array()
            .is_some_and(|a| a.iter().any(|i| i["digest"] == digest))
}
pub fn choose<'a>(nodes: &'a [Candidate], d: &Demand<'_>) -> Option<&'a Candidate> {
    nodes
        .iter()
        .filter(|n| {
            let c = &n.capabilities;
            n.online
                && n.active
                && n.pool == d.pool
                && d.requested.is_none_or(|id| id == n.id)
                && c["drain"] != true
                && c["controller_drain"] != true
                && crate::common::node_operation(c, "create").is_ok()
                && crate::common::guest_shape(c, d.memory_mib, d.cpus).is_ok()
                && (!d.managed
                    || (d.compatibility.is_some()
                        && crate::common::placement_capable(c).is_ok()
                        && c["cache_inventory_v1"] == true))
                && d.compatibility.is_none_or(|compat| c["compat"] == *compat)
                && (!d.disk_sizing || c["disk_resize_v1"] == true)
                && (c["images"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|i| i == d.image))
                    || (d.image_digest.is_some() && c["image_store_v1"] == true))
                && n.free.memory_mib >= d.memory_mib
                && n.free.cpus >= d.cpus
                && n.free.slots >= 1
                && (d.disk_bytes == 0
                    || (!d.managed && !d.disk_sizing && c["disk_resize_v1"] != true)
                    || (fresh(c["disk_observed_unix_s"].as_u64(), d.now)
                        && n.free.disk_bytes >= d.disk_bytes))
        })
        .min_by_key(|n| {
            let c = &n.capabilities;
            (
                !hit(c, d.capture_digest, d.now),
                !(hit(c, d.image_digest, d.now) || hit(c, d.parent_digest, d.now)),
                n.free.memory_mib - d.memory_mib,
                n.free.cpus - d.cpus,
                n.free.slots - 1,
                n.free.disk_bytes.saturating_sub(d.disk_bytes),
                n.joined,
                &n.id,
            )
        })
}
/// Spill, the second pass: runs only when [`choose`] found no runner in the pool and the
/// pool has a live home runner (`home_live`). Tries each rule of the pool in order: its
/// runner takes the box when it has fewer than `max_boxes` of the pool's boxes (`boxes`,
/// pending creates included), still has `keep_free_mib` of memory and a free slot after it,
/// and passes every check of the first pass.
pub fn spill<'a>(
    nodes: &'a [Candidate],
    d: &Demand<'_>,
    rules: &[crate::spill::Rule],
    keep_free_mib: u64,
    home_live: bool,
    boxes: impl Fn(&str) -> u64,
) -> Option<&'a Candidate> {
    if d.requested.is_some() || !home_live {
        return None;
    }
    rules
        .iter()
        .filter(|r| r.pool == d.pool && r.max_boxes > 0)
        .find_map(|rule| {
            let n = nodes
                .iter()
                .find(|n| n.id == rule.node && n.pool != d.pool)?;
            if boxes(&n.id) >= rule.max_boxes
                || n.free.memory_mib < d.memory_mib.saturating_add(keep_free_mib)
                || n.free.slots < 2
            {
                return None;
            }
            let mut borrowed = n.clone();
            borrowed.pool = d.pool.to_owned();
            choose(std::slice::from_ref(&borrowed), d).map(|_| n)
        })
}
#[cfg(test)]
mod tests;
