//! Antigravity's plan quota, as agy itself reports it to the title callback Muster gives each
//! Antigravity tab (`muster hook antigravity state`, src/adapters/antigravity.rs). That callback
//! gets the same JSON as agy's status line, whose `quota` maps a bucket id to
//! `{remaining_fraction, reset_time, reset_in_seconds}` (checked against agy 1.5.13's real
//! callback input and its `/usage` output on 25 Nov 2026; `quota` is null while it signs in).
//!
//! A Google AI plan gives two pools, each with a 5-hour and a weekly limit: the Gemini models
//! (`gemini-5h`, `gemini-weekly`) and the Claude and GPT models (`3p-5h`, `3p-weekly`). A pool
//! nobody has used reads `remaining_fraction: 1` with `reset_time` 1970-01-01 (no window running).
//!
//! - The hook ([`hook_body`]) forwards only those four allowlisted buckets, each as a used
//!   percentage and a reset time, to `POST /usage/antigravity` with the run's session token. The
//!   payload's email, account, plan, paths and everything else never leave the hook.
//! - The daemon checks the shape again ([`parse_report`]) and keeps the newest reading of each
//!   bucket, with when it arrived, in `runtime_state` (so a restart keeps it). Every Antigravity
//!   tab on this machine shares one agy login, so the newest report wins.
//! - Muster sends no request for it: a pool is known once an Antigravity tab has reported it, and
//!   its age shows how long ago that was (`antigravity_usage.get`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The quota buckets Muster shows: (bucket id, pool id, window length in minutes).
pub const BUCKETS: &[(&str, &str, i64)] = &[
    ("gemini-5h", "gemini", 300),
    ("gemini-weekly", "gemini", 10080),
    ("3p-5h", "3p", 300),
    ("3p-weekly", "3p", 10080),
];

/// The pools, in display order: (pool id, name).
pub const POOLS: &[(&str, &str)] = &[("gemini", "Gemini models"), ("3p", "Claude and GPT models")];

/// Where the daemon keeps the readings (`runtime_state` key).
pub const STATE_KEY: &str = "antigravity-usage";

/// The hook's record of its last report (in the session folder), so an unchanged reading is sent
/// again only after [`RENEW_EVERY_MS`].
pub const LAST_REPORT: &str = "antigravity-usage-last";

/// An unchanged reading is still reported this often, so its age shows agy still reads it so.
pub const RENEW_EVERY_MS: u128 = 60 * 1000;

/// Upper bound for a reset time (Unix seconds, year 2286): anything later is not a time.
const MAX_RESET_S: i64 = 9_999_999_999;

/// What the title callback forwards from agy's JSON: the allowlisted buckets, each with its used
/// percentage (one decimal) and its reset time (Unix seconds; null when no window is running).
/// None when it carries none of them.
pub fn hook_body(payload: &Value) -> Option<Vec<u8>> {
    let quota = payload.get("quota")?.as_object()?;
    let mut out = Map::new();
    for (id, _, _) in BUCKETS {
        let Some(b) = quota.get(*id).and_then(Value::as_object) else { continue };
        if b.get("disabled").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(left) = b.get("remaining_fraction").and_then(Value::as_f64).filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
        else { continue };
        let used = ((1.0 - left) * 1000.0).round() / 10.0;
        let resets_at = b
            .get("reset_time")
            .and_then(Value::as_str)
            .and_then(crate::codex_usage::parse_ts)
            .map(|ms| ms / 1000)
            .filter(|s| *s > 0 && *s <= MAX_RESET_S);
        out.insert((*id).into(), json!({"used_percentage": used, "resets_at": resets_at}));
    }
    if out.is_empty() {
        return None;
    }
    serde_json::to_vec(&json!({ "quota": out })).ok()
}

/// One bucket's reading as the daemon accepts it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reported {
    pub bucket: &'static str,
    pub used_percentage: f64,
    pub resets_at: Option<i64>,
}

/// A `POST /usage/antigravity` body, checked again: only allowlisted buckets with a percentage
/// from 0 to 100 and an optional whole reset time. Anything else in it is ignored.
pub fn parse_report(body: &Value) -> Vec<Reported> {
    let Some(quota) = body.get("quota").and_then(Value::as_object) else { return Vec::new() };
    BUCKETS
        .iter()
        .filter_map(|(id, _, _)| {
            let b = quota.get(*id)?;
            let used = b.get("used_percentage")?.as_f64().filter(|u| u.is_finite() && (0.0..=100.0).contains(u))?;
            let resets_at = match b.get("resets_at") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_i64().filter(|s| *s > 0 && *s <= MAX_RESET_S)?),
            };
            Some(Reported { bucket: id, used_percentage: used, resets_at })
        })
        .collect()
}

/// One bucket as kept: what agy last reported and when it arrived (Unix ms).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub used_percentage: f64,
    pub resets_at: Option<i64>,
    pub observed_at: i64,
}

/// Every bucket's newest reading (`runtime_state` [`STATE_KEY`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stored {
    pub buckets: BTreeMap<String, Bucket>,
}

/// Take a report's readings: each replaces its bucket's, dated `now`. Returns whether anything
/// besides the dates changed.
pub fn merge(stored: &mut Stored, report: &[Reported], now: i64) -> bool {
    let mut changed = false;
    for r in report {
        let next = Bucket { used_percentage: r.used_percentage, resets_at: r.resets_at, observed_at: now };
        let old = stored.buckets.insert(r.bucket.to_string(), next);
        changed |= old.is_none_or(|o| o.used_percentage != r.used_percentage || o.resets_at != r.resets_at);
    }
    changed
}

/// One window of a pool, for `antigravity_usage.get`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WindowView {
    pub window_minutes: i64,
    pub used_percentage: f64,
    /// Unix seconds; null while no window is running (the pool is unused).
    pub resets_at: Option<i64>,
    /// Unix ms: when an Antigravity tab last reported it.
    pub observed_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PoolView {
    pub id: &'static str,
    pub name: &'static str,
    /// Shortest window first.
    pub windows: Vec<WindowView>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AntigravityUsage {
    pub pools: Vec<PoolView>,
}

/// The pools with any known window, or None when no Antigravity tab has reported any.
pub fn view(stored: &Stored) -> Option<AntigravityUsage> {
    let pools: Vec<PoolView> = POOLS
        .iter()
        .filter_map(|(pool, name)| {
            let windows: Vec<WindowView> = BUCKETS
                .iter()
                .filter(|(_, p, _)| p == pool)
                .filter_map(|(id, _, minutes)| {
                    let b = stored.buckets.get(*id)?;
                    Some(WindowView {
                        window_minutes: *minutes,
                        used_percentage: b.used_percentage,
                        resets_at: b.resets_at,
                        observed_at: b.observed_at,
                    })
                })
                .collect();
            (!windows.is_empty()).then_some(PoolView { id: pool, name, windows })
        })
        .collect();
    (!pools.is_empty()).then_some(AntigravityUsage { pools })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// agy 1.5.13's real title-callback input, shape as captured on 25 Nov 2026 (account fields,
    /// paths and numbers invented).
    const REAL: &str = include_str!("fixtures/antigravity_title_1.5.13.json");

    #[test]
    fn the_hook_forwards_only_the_four_buckets_from_the_real_input() {
        let body: Value = serde_json::from_slice(&hook_body(&serde_json::from_str(REAL).unwrap()).unwrap()).unwrap();
        assert_eq!(body, json!({"quota": {
            "gemini-5h": {"used_percentage": 29.9, "resets_at": 1_795_676_453},
            "gemini-weekly": {"used_percentage": 83.6, "resets_at": 1_796_069_072},
            "3p-5h": {"used_percentage": 0.0, "resets_at": null},
            "3p-weekly": {"used_percentage": 0.0, "resets_at": null},
        }}));
        let text = body.to_string();
        for private in ["example.invalid", "alpha", "/home", "Pro", "Gemini 4"] {
            assert!(!text.contains(private), "{private} in {text}");
        }
    }

    #[test]
    fn missing_partial_and_odd_quotas() {
        // Signing in: quota null. No quota at all, or not JSON-shaped.
        assert_eq!(hook_body(&json!({"agent_state": "authenticating", "quota": null})), None);
        assert_eq!(hook_body(&json!({"agent_state": "idle"})), None);
        assert_eq!(hook_body(&json!({"quota": []})), None);
        // Unknown buckets, disabled ones, amounts instead of fractions, out-of-range fractions.
        let odd = json!({"quota": {
            "credits": {"remaining_amount": 5, "reset_time": "2026-11-30T20:04:32Z"},
            "gemini-5h": {"remaining_fraction": 0.5, "disabled": true},
            "gemini-weekly": {"remaining_amount": 3},
            "3p-5h": {"remaining_fraction": 1.5},
            "3p-weekly": {"remaining_fraction": "0.5"},
        }});
        assert_eq!(hook_body(&odd), None);
        // One good bucket of a partial quota; a bad reset time leaves only the time unknown.
        let partial = json!({"quota": {"gemini-weekly": {"remaining_fraction": 0.25, "reset_time": "soon"}}});
        let body: Value = serde_json::from_slice(&hook_body(&partial).unwrap()).unwrap();
        assert_eq!(body, json!({"quota": {"gemini-weekly": {"used_percentage": 75.0, "resets_at": null}}}));
    }

    #[test]
    fn the_daemon_checks_the_report_again() {
        let body = json!({"quota": {
            "gemini-5h": {"used_percentage": 29.9, "resets_at": 1_795_676_453},
            "3p-weekly": {"used_percentage": 0, "resets_at": null},
            "3p-5h": {"used_percentage": 101, "resets_at": null},
            "gemini-weekly": {"used_percentage": 5, "resets_at": "tomorrow"},
            "email": {"used_percentage": 1, "resets_at": null},
        }, "email": "someone@example.invalid"});
        assert_eq!(parse_report(&body), vec![
            Reported { bucket: "gemini-5h", used_percentage: 29.9, resets_at: Some(1_795_676_453) },
            Reported { bucket: "3p-weekly", used_percentage: 0.0, resets_at: None },
        ]);
        assert!(parse_report(&json!({})).is_empty());
        assert!(parse_report(&json!({"quota": {"gemini-5h": {"used_percentage": 1, "resets_at": -5}}})).is_empty());
    }

    #[test]
    fn the_newest_reading_of_each_bucket_is_kept_and_shown_by_pool() {
        let mut s = Stored::default();
        assert_eq!(view(&s), None);
        let first = parse_report(&json!({"quota": {
            "gemini-weekly": {"used_percentage": 80, "resets_at": 2000},
            "gemini-5h": {"used_percentage": 30, "resets_at": 1000},
        }}));
        assert!(merge(&mut s, &first, 10));
        // The same figures again only renew the date.
        assert!(!merge(&mut s, &first, 20));
        assert_eq!(s.buckets["gemini-5h"].observed_at, 20);
        // A partial report updates its own bucket and keeps the others.
        assert!(merge(&mut s, &parse_report(&json!({"quota": {"3p-5h": {"used_percentage": 4, "resets_at": 3000}}})), 30));
        let v = view(&s).unwrap();
        assert_eq!(v.pools.len(), 2);
        assert_eq!((v.pools[0].id, v.pools[0].name), ("gemini", "Gemini models"));
        assert_eq!(v.pools[0].windows, vec![
            WindowView { window_minutes: 300, used_percentage: 30.0, resets_at: Some(1000), observed_at: 20 },
            WindowView { window_minutes: 10080, used_percentage: 80.0, resets_at: Some(2000), observed_at: 20 },
        ]);
        assert_eq!(v.pools[1].windows, vec![
            WindowView { window_minutes: 300, used_percentage: 4.0, resets_at: Some(3000), observed_at: 30 },
        ]);
        // A stored record survives a restart as JSON.
        let back: Stored = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
