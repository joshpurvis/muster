//! Codex's plan usage, read from Codex's own session logs. Each turn's `token_count` event
//! carries `rate_limits` (a `primary` and `secondary` window, each with `used_percent`,
//! `window_minutes` and `resets_at`). Muster sends no request for it: the figure is whatever the
//! newest Codex session last saw, with the time it saw it (`codex_usage.get`).

use serde::Serialize;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CodexWindow {
    pub window_minutes: i64,
    pub used_percentage: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CodexUsage {
    /// Unix ms of the event that carried the figures.
    pub observed_at: i64,
    pub windows: Vec<CodexWindow>,
    pub plan: Option<String>,
}

/// Only the tail of a log is read: the newest event is near its end.
const TAIL_BYTES: u64 = 512 * 1024;
/// Logs read, newest first by modification time. Every log is ranked, not only recent day
/// folders: a resumed session keeps writing to the log in the folder of the day it started.
const FILES: usize = 6;

/// `$CODEX_HOME`, else `~/.codex` (as the Codex CLI resolves it).
pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::home_dir().join(".codex"))
}

/// The newest figures any recent session log holds, or None when there are none.
pub fn read(home: &Path) -> Option<CodexUsage> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = day_dirs(&home.join("sessions"))
        .into_iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.iter().take(FILES).filter_map(|(_, p)| last_in(p)).max_by_key(|u| u.observed_at)
}

/// Every sessions/YYYY/MM/DD folder (only their files' metadata is read).
fn day_dirs(root: &Path) -> Vec<PathBuf> {
    let dirs = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()
    };
    dirs(root).iter().flat_map(|y| dirs(y)).flat_map(|m| dirs(&m)).collect()
}

fn last_in(path: &Path) -> Option<CodexUsage> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES))).ok()?;
    let mut buf = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut buf).ok()?;
    // A cut-off first line simply fails to parse.
    String::from_utf8_lossy(&buf).lines().rev().filter(|l| l.contains("\"rate_limits\"")).find_map(parse_line)
}

/// One log line, if it is an event with Codex's own plan limits.
fn parse_line(line: &str) -> Option<CodexUsage> {
    let v: Value = serde_json::from_str(line).ok()?;
    let limits = v.get("payload")?.get("rate_limits")?;
    // Other limit ids (e.g. "premium") are separate allowances, not the plan's.
    if limits.get("limit_id").and_then(Value::as_str).is_some_and(|id| id != "codex") {
        return None;
    }
    let observed_at = parse_ts(v.get("timestamp")?.as_str()?)?;
    let windows: Vec<CodexWindow> = ["primary", "secondary"]
        .iter()
        .filter_map(|k| {
            let w = limits.get(*k)?;
            let minutes = w.get("window_minutes")?.as_i64().filter(|m| *m > 0)?;
            let resets_at = w.get("resets_at").and_then(Value::as_i64).or_else(|| {
                w.get("resets_in_seconds").and_then(Value::as_i64).map(|s| observed_at / 1000 + s)
            });
            Some(CodexWindow { window_minutes: minutes, used_percentage: w.get("used_percent")?.as_f64()?, resets_at })
        })
        .collect();
    if windows.is_empty() {
        return None;
    }
    let plan = limits.get("plan_type").and_then(Value::as_str).map(str::to_string);
    Some(CodexUsage { observed_at, windows, plan })
}

/// "2026-11-11T07:48:58.963Z" as Unix ms (UTC only, as Codex writes it).
fn parse_ts(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let ms = format!("{:0<3}", frac.get(..frac.len().min(3))?).parse::<i64>().ok()?;
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"timestamp":"2026-11-11T07:48:58.963Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":12.5,"window_minutes":10080,"resets_at":1794984412},"secondary":null,"plan_type":"prolite"}}}"#;

    #[test]
    fn timestamps_parse_as_utc_ms() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2000-02-29T00:00:00.5Z"), Some(951_782_400_500));
        assert_eq!(parse_ts("2026-11-11T07:48:58.963Z"), Some(1_794_383_338_963));
        assert_eq!(parse_ts("2026-11-11T07:48:58+01:00"), None);
        assert_eq!(parse_ts("2026-13-11T07:48:58Z"), None);
    }

    #[test]
    fn a_token_count_line_gives_the_plan_windows() {
        let u = parse_line(LINE).unwrap();
        assert_eq!(u.observed_at, 1_794_383_338_963);
        assert_eq!(u.plan.as_deref(), Some("prolite"));
        assert_eq!(u.windows, vec![CodexWindow { window_minutes: 10080, used_percentage: 12.5, resets_at: Some(1794984412) }]);
    }

    #[test]
    fn other_limits_and_empty_windows_are_skipped() {
        assert_eq!(parse_line(&LINE.replace("\"codex\"", "\"premium\"")), None);
        assert_eq!(parse_line(&LINE.replace("10080", "0")), None);
        let older = LINE.replace(",\"resets_at\":1794984412", ",\"resets_in_seconds\":60");
        assert_eq!(parse_line(&older).unwrap().windows[0].resets_at, Some(1_794_383_338 + 60));
    }

    #[test]
    fn the_newest_observation_in_recent_logs_wins() {
        let home = std::env::temp_dir().join(format!("muster-codex-usage-{}", std::process::id()));
        let day = home.join("sessions/2026/11/11");
        std::fs::create_dir_all(&day).unwrap();
        let newer = LINE.replace("07:48:58", "08:00:00").replace("12.5", "40");
        std::fs::write(day.join("a.jsonl"), format!("{{\"cut line\n{LINE}\n{{\"timestamp\":\"x\"}}\n")).unwrap();
        std::fs::write(day.join("b.jsonl"), format!("{newer}\n")).unwrap();
        std::fs::write(day.join("c.txt"), &newer).unwrap();
        assert_eq!(read(&home).unwrap().windows[0].used_percentage, 40.0);
        // A session started long ago and resumed today writes to its old day folder.
        let old_day = home.join("sessions/2026/01/02");
        std::fs::create_dir_all(&old_day).unwrap();
        let resumed = LINE.replace("07:48:58", "09:00:00").replace("12.5", "55");
        std::fs::write(old_day.join("r.jsonl"), format!("{resumed}\n")).unwrap();
        assert_eq!(read(&home).unwrap().windows[0].used_percentage, 55.0);
        assert_eq!(read(&home.join("missing")), None);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
