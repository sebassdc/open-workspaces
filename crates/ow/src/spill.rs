//! Spill (docs/MULTINODE.md#spill): a pool whose own runners are full may place up to
//! `max_boxes` boxes on a named runner of another pool, while that runner keeps
//! `placement.spill_keep_free_mib` of memory for its own pool. Both are controller
//! settings, changed with `ow config set` and read on every create.
use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

pub(crate) const RULES: &str = "placement.spill";
pub(crate) const KEEP_FREE: &str = "placement.spill_keep_free_mib";
const MAX_RULES: usize = 8;
const MAX_BOXES: u64 = 64;

/// One rule: `pool` may borrow up to `max_boxes` boxes on runner `node`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    pub pool: String,
    pub node: String,
    pub max_boxes: u64,
}

/// The string grammar: comma-separated `pool:node:max_boxes`; empty means no rule.
pub(crate) fn parse(text: &str) -> Result<Vec<Rule>> {
    let mut rules: Vec<Rule> = vec![];
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let fields: Vec<&str> = part.split(':').collect();
        let [pool, node, max] = fields[..] else {
            bail!("each rule is pool:node:max_boxes, not {part:?}");
        };
        crate::common::identifier(pool)
            .map_err(|_| anyhow::anyhow!("pool {pool:?} is not an identifier"))?;
        // Ordinary creates record no pool, so `default` cannot borrow (nor be counted).
        ensure!(
            pool != "default",
            "pool default cannot spill; name another pool"
        );
        crate::common::identifier(node)
            .map_err(|_| anyhow::anyhow!("node {node:?} is not an identifier"))?;
        let max_boxes: u64 = max
            .parse()
            .map_err(|_| anyhow::anyhow!("max_boxes {max:?} is not a whole number"))?;
        ensure!(
            max_boxes <= MAX_BOXES,
            "max_boxes must be within 0..={MAX_BOXES}"
        );
        ensure!(
            !rules.iter().any(|r| r.pool == pool && r.node == node),
            "rule {pool}:{node} appears twice"
        );
        rules.push(Rule {
            pool: pool.into(),
            node: node.into(),
            max_boxes,
        });
    }
    ensure!(rules.len() <= MAX_RULES, "at most {MAX_RULES} rules");
    Ok(rules)
}

/// The stored string form of typed rules.
pub(crate) fn text(rules: &Value) -> String {
    serde_json::from_value::<Vec<Rule>>(rules.clone())
        .unwrap_or_default()
        .iter()
        .map(|r| format!("{}:{}:{}", r.pool, r.node, r.max_boxes))
        .collect::<Vec<_>>()
        .join(",")
}

/// Typed JSON rules (as `config.set` takes them) in the string grammar, for one parser.
pub(crate) fn from_json(value: &Value) -> Result<String> {
    let rules: Vec<Rule> = serde_json::from_value(value.clone()).map_err(|e| {
        anyhow::anyhow!("expected a list of {{\"pool\", \"node\", \"max_boxes\"}} ({e})")
    })?;
    Ok(text(&json!(rules)))
}

fn table(db: &Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

/// A node's pool, budget and whether it is enrolled and not revoked.
fn node_row(db: &Connection, node: &str) -> Result<Option<(String, u64, bool)>> {
    Ok(db
        .query_row(
            "SELECT COALESCE((SELECT pool FROM node_placement_policy WHERE node=n.id),'default'),memory_mib,credential_hash IS NOT NULL AND revoked=0 FROM nodes n WHERE id=?1",
            [node],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?)),
        )
        .optional()?)
}

/// Images every online runner of `pool` has: what its creates will ask for.
fn pool_images(inventory: &Value, pool: &str) -> Option<Vec<Value>> {
    let homes: Vec<&Value> = inventory
        .as_array()?
        .iter()
        .filter(|n| n["capabilities"]["pool"] == pool && n["online"] == true)
        .collect();
    let first = homes.first()?["capabilities"]["images"].as_array()?.clone();
    Some(
        first
            .into_iter()
            .filter(|i| {
                homes.iter().all(|n| {
                    n["capabilities"]["images"]
                        .as_array()
                        .is_some_and(|a| a.contains(i))
                })
            })
            .collect(),
    )
}

fn missing_images(db: &Connection, inventory: &Value, pool: &str, node: &str) -> Vec<String> {
    let Some(wanted) = pool_images(inventory, pool) else {
        return vec![];
    };
    let lender = inventory
        .as_array()
        .into_iter()
        .flatten()
        .find(|n| n["id"] == node);
    let has = lender
        .and_then(|n| n["capabilities"]["images"].as_array().cloned())
        .unwrap_or_default();
    // A runner with the image store pulls an image the catalog has, as placement allows.
    let store = lender.is_some_and(|n| n["capabilities"]["image_store_v1"] == true);
    wanted
        .iter()
        .filter(|i| !has.contains(i))
        .filter_map(|i| i.as_str().map(str::to_owned))
        .filter(|i| !(store && in_catalog(db, i)))
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn in_catalog(db: &Connection, image: &str) -> bool {
    crate::image_catalog::resolve(db, image).is_ok()
}
#[cfg(not(any(target_os = "linux", test)))]
fn in_catalog(_: &Connection, _: &str) -> bool {
    false
}

/// Checks a new setting against the runners it names. `Err` is the reason it is refused;
/// `Ok` lists what is valid but will not do what it reads like.
pub(crate) fn check(
    db: &Connection,
    inventory: &Value,
    key: &str,
    value: &Value,
    settings: &Value,
) -> std::result::Result<Vec<String>, String> {
    let (rules, keep_free) = match key {
        RULES => (
            serde_json::from_value::<Vec<Rule>>(value.clone()).unwrap_or_default(),
            settings[KEEP_FREE].as_u64().unwrap_or(0),
        ),
        KEEP_FREE => (
            serde_json::from_value::<Vec<Rule>>(settings[RULES].clone()).unwrap_or_default(),
            value.as_u64().unwrap_or(0),
        ),
        _ => return Ok(vec![]),
    };
    let mut warnings = vec![];
    for rule in &rules {
        let Some((own, budget, enrolled)) = node_row(db, &rule.node).map_err(|e| e.to_string())?
        else {
            if key == RULES {
                return Err(format!("node {} is not enrolled", rule.node));
            }
            continue;
        };
        if key == RULES && !enrolled {
            return Err(format!("node {} is not enrolled or is revoked", rule.node));
        }
        if key == RULES && own == rule.pool {
            return Err(format!(
                "node {} is already in pool {}",
                rule.node, rule.pool
            ));
        }
        if keep_free > budget {
            warnings.push(format!(
                "{KEEP_FREE} {keep_free} is above {}'s memory budget {budget}; {} will take no spilled box",
                rule.node, rule.node
            ));
        }
        for image in missing_images(db, inventory, &rule.pool, &rule.node) {
            warnings.push(format!(
                "{} does not have image {image}, which pool {}'s runners have",
                rule.node, rule.pool
            ));
        }
    }
    Ok(warnings)
}

/// Boxes of `pool` on `node` and how many of them are still pending, with their names.
fn usage(db: &Connection, node: &str, pool: &str) -> Result<(u64, u64, Vec<String>)> {
    if !table(db, "resources")? || !table(db, "box_pools")? {
        return Ok((0, 0, vec![]));
    }
    let rows: Vec<(String, String)> = db
        .prepare("SELECT r.name,COALESCE(json_extract(r.metadata,'$.state'),'') FROM box_pools b JOIN resources r ON r.kind='machine' AND r.physical=b.physical WHERE r.node=?1 AND b.pool=?2 ORDER BY r.name")?
        .query_map([node, pool], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let pending = rows.iter().filter(|(_, state)| state == "pending").count() as u64;
    Ok((
        rows.len() as u64,
        pending,
        rows.into_iter().map(|(name, _)| name).collect(),
    ))
}

/// A runner's free memory and slots after the boxes and transfers reserved on it.
fn free(db: &Connection, n: &Value) -> Result<(u64, u64)> {
    let id = n["id"].as_str().unwrap_or_default();
    let (mut used, mut count) = (0u64, 0u64);
    if table(db, "resources")?
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('resources') WHERE name='reserved_mib')",
            [],
            |r| r.get::<_, bool>(0),
        )?
    {
        (used, count) = db.query_row(
            "SELECT COALESCE(SUM(reserved_mib),0),COALESCE(SUM(CASE WHEN reserved_mib>0 THEN 1 ELSE 0 END),0) FROM resources WHERE kind='machine' AND node=?1",
            [id],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?;
    }
    if table(db, "node_usage")? {
        let extra: Option<(i64, i64)> = db
            .query_row(
                "SELECT extra_mib,extra_slots FROM node_usage WHERE node=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((mib, slots)) = extra {
            used += mib as u64;
            count += slots as u64;
        }
    }
    if table(db, "transfer_reservations")? {
        let (mib, slots): (i64, i64) = db.query_row(
            "SELECT COALESCE(SUM(memory_mib),0),COUNT(*) FROM transfer_reservations WHERE node=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        used += mib as u64;
        count += slots as u64;
    }
    Ok((
        n["memory_mib"].as_u64().unwrap_or(0).saturating_sub(used),
        n["slots"].as_u64().unwrap_or(0).saturating_sub(count),
    ))
}

fn draining(n: &Value) -> bool {
    let c = &n["capabilities"];
    c["drain"] == true
        || c["controller_drain"] == true
        || c["drain_state"].as_str().is_some_and(|s| s != "active")
}

/// A pool has a live home: a runner of its own that is online, active and not draining.
/// Without one nothing spills, so taking a pool's runners out of it stops its spill too.
pub(crate) fn home_live(inventory: &Value, pool: &str) -> bool {
    inventory.as_array().into_iter().flatten().any(|n| {
        n["capabilities"]["pool"] == pool
            && n["online"] == true
            && n["capabilities"]["active"] != false
            && !draining(n)
    })
}

/// `placement.status`: the rules and how full each home and spill runner is.
pub(crate) fn status(root: &Path, only: Option<&str>) -> Result<Value> {
    let db = crate::nodes::db(root)?;
    let settings = crate::telemetry::settings(&db)?;
    let rules: Vec<Rule> = serde_json::from_value(settings[RULES].clone()).unwrap_or_default();
    let keep_free = settings[KEEP_FREE].as_u64().unwrap_or(0);
    let inventory = crate::nodes::inventory(root)?;
    let nodes = inventory.as_array().cloned().unwrap_or_default();
    let mut pools: Vec<String> = rules.iter().map(|r| r.pool.clone()).collect();
    if let Some(pool) = only {
        pools.retain(|p| p == pool);
        if pools.is_empty() {
            pools.push(pool.to_owned());
        }
    }
    pools.sort();
    pools.dedup();
    let mut out = vec![];
    for pool in pools {
        let mut homes = vec![];
        for n in nodes
            .iter()
            .filter(|n| n["capabilities"]["pool"] == pool.as_str())
        {
            let id = n["id"].as_str().unwrap_or_default();
            let (boxes, pending, _) = usage(&db, id, &pool)?;
            let (memory, slots) = free(&db, n)?;
            homes.push(json!({"node":id,"online":n["online"]==true,"active":n["capabilities"]["active"]!=false,
                "draining":draining(n),"boxes":boxes,"pending":pending,"free_slots":slots,"free_memory_mib":memory}));
        }
        let live = home_live(&inventory, &pool);
        let mut spill = vec![];
        for rule in rules.iter().filter(|r| r.pool == pool) {
            let n = nodes.iter().find(|n| n["id"] == rule.node.as_str());
            let (boxes, pending, names) = usage(&db, &rule.node, &pool)?;
            let (memory, slots) = match n {
                Some(n) => free(&db, n)?,
                None => (0, 0),
            };
            let state = match n {
                _ if rule.max_boxes == 0 => "off",
                None => "inert_revoked",
                Some(n) if n["revoked"] == true => "inert_revoked",
                Some(n) if n["online"] != true => "inert_offline",
                Some(n) if draining(n) || n["capabilities"]["active"] == false => "inert_draining",
                Some(_) if !missing_images(&db, &inventory, &pool, &rule.node).is_empty() => {
                    "inert_image_missing"
                }
                _ if !live => "no_home",
                _ if boxes >= rule.max_boxes => "full",
                _ if memory <= keep_free || slots < 2 => "reserve",
                _ => "available",
            };
            spill.push(json!({"node":rule.node,"max_boxes":rule.max_boxes,"boxes":boxes,"pending":pending,"box_names":names,
                "free_memory_mib":memory,"free_memory_after_reserve_mib":memory.saturating_sub(keep_free),"state":state}));
        }
        out.push(json!({"pool":pool,"home_nodes":homes,"spill":spill}));
    }
    Ok(json!({"version":settings["version"],"keep_free_mib":keep_free,"pools":out}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_and_typed_form_agree() {
        assert_eq!(parse("").unwrap(), vec![]);
        let rules = parse("ci:n2:2, ci:n3:0").unwrap();
        assert_eq!(
            rules[0],
            Rule {
                pool: "ci".into(),
                node: "n2".into(),
                max_boxes: 2
            }
        );
        assert_eq!(text(&json!(rules)), "ci:n2:2,ci:n3:0");
        assert_eq!(
            from_json(&json!([{"pool":"ci","node":"n2","max_boxes":2}])).unwrap(),
            "ci:n2:2"
        );
        assert_eq!(from_json(&json!([])).unwrap(), "");
        assert!(
            parse("default:n2:1").is_err(),
            "default records no pool, so it cannot spill"
        );
        for bad in [
            "ci:n2",
            "ci:n2:x",
            "ci:n2:65",
            "ci:n2:1,ci:n2:2",
            "c i:n2:1",
            "ci:n2:1:1",
            "a:n:1,b:n:1,c:n:1,d:n:1,e:n:1,f:n:1,g:n:1,h:n:1,i:n:1",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(from_json(&json!([{"pool":"ci","node":"n2"}])).is_err());
        assert!(from_json(&json!([{"pool":"ci","node":"n2","max_boxes":1,"x":1}])).is_err());
        assert!(from_json(&json!("ci:n2:2")).is_err());
    }
}
