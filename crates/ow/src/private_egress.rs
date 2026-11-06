//! `network.private_egress` (docs/NETWORKING.md#private-destinations): per placement pool, the
//! exact private `ADDRESS:PORT` destinations its boxes may reach. A controller setting like
//! every other (`ow config set`); each runner gets its share with every heartbeat
//! acknowledgement and its agent makes the worker's set match.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const SETTING: &str = "network.private_egress";
const MAX_POOLS: usize = 16;

/// One pool's destinations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub pool: String,
    pub to: Vec<String>,
}

/// Parses and checks the setting's JSON text ("" or "[]": none). Destinations come back
/// sorted, without duplicates.
pub(crate) fn parse(text: &str) -> Result<Vec<Entry>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<Entry> = serde_json::from_str(text)
        .context("expected a list of {\"pool\", \"to\": [\"ADDRESS:PORT\", ...]}")?;
    ensure!(entries.len() <= MAX_POOLS, "at most {MAX_POOLS} pools");
    let mut pools = std::collections::HashSet::new();
    for entry in &mut entries {
        crate::common::identifier(&entry.pool)
            .map_err(|_| anyhow::anyhow!("pool {:?} is not an identifier", entry.pool))?;
        ensure!(
            pools.insert(entry.pool.clone()),
            "pool {} is listed twice",
            entry.pool
        );
        let list = ow_core::egress::destinations(&json!(entry.to))
            .with_context(|| format!("pool {}", entry.pool))?;
        entry.to = ow_core::egress::text(&list);
    }
    // A runner may get several pools' lists (its own and those spilling onto it): all of them
    // together must fit the worker's set.
    let all: Vec<&String> = entries.iter().flat_map(|e| &e.to).collect();
    ow_core::egress::destinations(&json!(all)).context("all pools together")?;
    Ok(entries)
}

/// The setting's typed form, for `config.set` and `config.get`.
pub(crate) fn typed(text: &str) -> Result<Value> {
    Ok(serde_json::to_value(parse(text)?)?)
}

/// `config.set`'s JSON value as the setting's text.
pub(crate) fn from_json(value: &Value) -> Result<String> {
    ensure!(value.is_array(), "expected a list of pools");
    Ok(serde_json::to_string(&parse(&value.to_string())?)?)
}

/// What a runner is given: the destinations of its own pool and of every pool that may spill
/// onto it (its boxes may run there too), sorted, without duplicates. Null when the catalog
/// cannot be read, so the runner keeps what it has.
pub(crate) fn for_node(db: &Connection, node: &str) -> Value {
    let read = || -> Result<Value> {
        let settings = crate::telemetry::settings(db)?;
        let entries: Vec<Entry> = serde_json::from_value(settings[SETTING].clone())?;
        if entries.is_empty() {
            return Ok(json!([]));
        }
        let own: String = db
            .query_row(
                "SELECT pool FROM node_placement_policy WHERE node=?1",
                [node],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "default".into());
        let spill: Vec<crate::spill::Rule> =
            serde_json::from_value(settings[crate::spill::RULES].clone()).unwrap_or_default();
        Ok(json!(share(&entries, &own, &spill, node)?))
    };
    read().unwrap_or(Value::Null)
}

/// [`for_node`]'s pure part. A spill rule with no room (`max_boxes` 0) is off and opens
/// nothing. In the worker's own order, so an unchanged list is never applied again.
fn share(
    entries: &[Entry],
    own: &str,
    spill: &[crate::spill::Rule],
    node: &str,
) -> Result<Vec<String>> {
    let to: Vec<&String> = entries
        .iter()
        .filter(|e| {
            e.pool == own
                || spill
                    .iter()
                    .any(|r| r.node == node && r.pool == e.pool && r.max_boxes > 0)
        })
        .flat_map(|e| &e.to)
        .collect();
    Ok(ow_core::egress::text(&ow_core::egress::destinations(
        &json!(to),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spill::Rule;

    #[test]
    fn the_setting_is_checked_and_normalized() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("[]").unwrap().is_empty());
        let parsed =
            parse(r#"[{"pool":"ci","to":["10.0.0.2:443","10.0.0.1:443","10.0.0.2:443"]}]"#)
                .unwrap();
        assert_eq!(parsed[0].to, ["10.0.0.1:443", "10.0.0.2:443"]);
        for bad in [
            r#"{"pool":"ci","to":[]}"#,
            r#"[{"pool":"ci"}]"#,
            r#"[{"pool":"ci","to":["10.0.0.1:443"],"extra":1}]"#,
            r#"[{"pool":"Not An Id","to":[]}]"#,
            r#"[{"pool":"ci","to":[]},{"pool":"ci","to":[]}]"#,
            r#"[{"pool":"ci","to":["8.8.8.8:53"]}]"#,
            r#"[{"pool":"ci","to":["169.254.169.254:80"]}]"#,
            r#"[{"pool":"ci","to":["10.0.0.0/8:443"]}]"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(from_json(&json!("ci")).is_err());
        // All pools together fit one runner's set.
        let list = |from: u32, n: u32| {
            (from..from + n)
                .map(|i| format!("10.0.1.{i}:443"))
                .collect::<Vec<_>>()
        };
        assert!(
            from_json(&json!([{"pool":"a","to":list(0, 33)},{"pool":"b","to":list(33, 32)}]))
                .is_err()
        );
        assert!(
            from_json(&json!([{"pool":"a","to":list(0, 33)},{"pool":"b","to":list(0, 64)}]))
                .is_ok()
        );
        // Numeric order, as the worker reports it.
        assert_eq!(
            parse(r#"[{"pool":"ci","to":["10.0.0.10:443","10.0.0.2:443"]}]"#).unwrap()[0].to,
            ["10.0.0.2:443", "10.0.0.10:443"]
        );
        assert_eq!(
            typed(&from_json(&json!([{"pool":"ci","to":["10.0.0.1:443"]}])).unwrap()).unwrap(),
            json!([{"pool":"ci","to":["10.0.0.1:443"]}])
        );
    }

    #[test]
    fn a_runner_gets_its_pool_and_the_pools_that_spill_onto_it() {
        let entries = parse(
            r#"[{"pool":"ci","to":["10.0.0.1:443","10.0.0.2:443"]},{"pool":"build","to":["10.0.0.2:443","10.0.0.3:5000"]}]"#,
        )
        .unwrap();
        let spill = [
            Rule {
                pool: "ci".into(),
                node: "people-1".into(),
                max_boxes: 2,
            },
            Rule {
                pool: "ci".into(),
                node: "people-3".into(),
                max_boxes: 0,
            },
        ];
        let share = |own: &str, node: &str| share(&entries, own, &spill, node).unwrap();
        assert_eq!(share("ci", "ci-1"), ["10.0.0.1:443", "10.0.0.2:443"]);
        // A runner of another pool that `ci` may spill onto.
        assert_eq!(
            share("default", "people-1"),
            ["10.0.0.1:443", "10.0.0.2:443"]
        );
        // A spill rule with no room is off: nothing opens there.
        assert!(share("default", "people-3").is_empty());
        // Any other runner: nothing.
        assert!(share("default", "people-2").is_empty());
        assert_eq!(
            share("build", "people-1"),
            ["10.0.0.1:443", "10.0.0.2:443", "10.0.0.3:5000"]
        );
    }
}
