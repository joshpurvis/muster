//! Five-field cron lines (`minute hour day-of-month month day-of-week`), their next fire time in
//! a named time zone, and a plain-English reading. No crate in the lockfile parses cron, and the
//! ones that do pull in chrono; this is the small subset schedules need, on top of jiff (which
//! does the time-zone and daylight-saving maths).
//!
//! Syntax: `*`, numbers, ranges `a-b`, steps `*/n` and `a-b/n`, lists `a,b`, month names
//! (`jan`…`dec`) and day names (`sun`…`sat`; 0 and 7 are Sunday), and the macros `@hourly`,
//! `@daily`, `@midnight`, `@weekly`, `@monthly`, `@yearly`, `@annually`. As in Vixie cron, when
//! both day fields are restricted a day matches either of them; only a bare `*` leaves a day
//! field unrestricted (a step such as `*/2` restricts it).
//!
//! Daylight saving: a time that doesn't exist that day (the clocks went forward) fires at the
//! same wall-clock offset after the change (02:30 becomes 03:30); a time that happens twice (the
//! clocks went back) fires once, the first time. So 09:00 stays 09:00 all year.

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    /// Bits 1..=31.
    dom: u32,
    /// Bits 1..=12.
    months: u16,
    /// Bits 0..=6, Sunday = 0.
    dow: u8,
    dom_star: bool,
    dow_star: bool,
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];
const DAY_NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// How far `next_after` looks ahead: long enough for February 29th on a given weekday.
const HORIZON_DAYS: i32 = 366 * 30;

fn value(s: &str, min: u32, max: u32, names: &[&str], name_base: u32) -> Result<u32, String> {
    let lower = s.to_ascii_lowercase();
    if let Some(i) = names.iter().position(|n| *n == lower) {
        return Ok(i as u32 + name_base);
    }
    let v: u32 = s.parse().map_err(|_| format!("`{s}` isn't a number"))?;
    if v < min || v > max {
        return Err(format!("{v} is out of range ({min}-{max})"));
    }
    Ok(v)
}

/// One field as a bit set of the values it allows.
fn field(s: &str, min: u32, max: u32, names: &[&str], name_base: u32) -> Result<u64, String> {
    let mut bits = 0u64;
    for item in s.split(',') {
        if item.is_empty() {
            return Err("empty list item".into());
        }
        let (range, step) = match item.split_once('/') {
            Some((r, st)) => {
                let st: u32 = st.parse().map_err(|_| format!("bad step `{st}`"))?;
                if st == 0 || st > max {
                    return Err(format!("bad step {st}"));
                }
                (r, st)
            }
            None => (item, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (a, b) = (value(a, min, max, names, name_base)?, value(b, min, max, names, name_base)?);
            if a > b {
                return Err(format!("range {a}-{b} goes backwards"));
            }
            (a, b)
        } else {
            let a = value(range, min, max, names, name_base)?;
            // `5/15`: from 5 to the end, every 15.
            (a, if item.contains('/') { max } else { a })
        };
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

fn bits_list(bits: u64, lo: u32, hi: u32) -> Vec<u32> {
    (lo..=hi).filter(|v| bits & (1 << v) != 0).collect()
}

impl Cron {
    pub fn parse(line: &str) -> Result<Cron, String> {
        let line = line.trim();
        let expanded = match line.to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *".to_string(),
            "@daily" | "@midnight" => "0 0 * * *".to_string(),
            "@weekly" => "0 0 * * 0".to_string(),
            "@monthly" => "0 0 1 * *".to_string(),
            "@yearly" | "@annually" => "0 0 1 1 *".to_string(),
            s if s.starts_with('@') => return Err(format!("`{line}` isn't supported (use @hourly, @daily, @weekly, @monthly or @yearly)")),
            _ => line.to_string(),
        };
        let parts: Vec<&str> = expanded.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "a cron line has 5 fields (minute hour day-of-month month day-of-week); `{line}` has {}",
                parts.len()
            ));
        }
        let at = |i: usize, what: &str, r: Result<u64, String>| r.map_err(|e| format!("{what} field `{}`: {e}", parts[i]));
        let minutes = at(0, "minute", field(parts[0], 0, 59, &[], 0))?;
        let hours = at(1, "hour", field(parts[1], 0, 23, &[], 0))? as u32;
        let dom = at(2, "day-of-month", field(parts[2], 1, 31, &[], 0))? as u32;
        let months = at(3, "month", field(parts[3], 1, 12, &MONTHS, 1))? as u16;
        let mut dow = at(4, "day-of-week", field(parts[4], 0, 7, &DAYS, 0))?;
        if dow & (1 << 7) != 0 {
            dow = (dow & !(1 << 7)) | 1;
        }
        let c = Cron {
            minutes,
            hours,
            dom,
            months,
            dow: dow as u8,
            // Only a bare `*` leaves a day field open: `*/2` is a restriction like any other
            // (so `0 9 */2 * 1` is every other day of the month, or a Monday).
            dom_star: parts[2] == "*",
            dow_star: parts[4] == "*",
        };
        // `0 0 31 2 *` never happens: say so now rather than never firing.
        if !c.dom_star && c.dow_star {
            let possible = (1..=12u32).filter(|m| c.months & (1 << m) != 0).any(|m| {
                let len = match m {
                    2 => 29,
                    4 | 6 | 9 | 11 => 30,
                    _ => 31,
                };
                (1..=len).any(|d| c.dom & (1 << d) != 0)
            });
            if !possible {
                return Err(format!("`{line}` names a day that never comes in those months"));
            }
        }
        Ok(c)
    }

    fn day_matches(&self, d: Date) -> bool {
        if self.months & (1 << d.month() as u32) == 0 {
            return false;
        }
        let dom = self.dom & (1 << d.day() as u32) != 0;
        let dow = self.dow & (1 << d.weekday().to_sunday_zero_offset() as u32) != 0;
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// Matching wall-clock times from `start` (inclusive), in order, for `HORIZON_DAYS`.
    fn civil_from(&self, start: DateTime) -> impl Iterator<Item = DateTime> + '_ {
        let hours = bits_list(self.hours as u64, 0, 23);
        let minutes = bits_list(self.minutes, 0, 59);
        let first = start.date();
        let (h0, m0) = (start.hour() as u32, start.minute() as u32);
        (0..HORIZON_DAYS)
            .scan(Some(first), |d, _| {
                let cur = (*d)?;
                *d = cur.tomorrow().ok();
                Some(cur)
            })
            .filter(|d| self.day_matches(*d))
            .flat_map(move |d| {
                let first_day = d == first;
                let hours = hours.clone();
                let minutes = minutes.clone();
                hours.into_iter().filter(move |h| !first_day || *h >= h0).flat_map(move |h| {
                    let minutes = minutes.clone();
                    minutes
                        .into_iter()
                        .filter(move |m| !(first_day && h == h0 && *m < m0))
                        .map(move |m| d.at(h as i8, m as i8, 0, 0))
                })
            })
    }

    /// The first fire time strictly after `after_ms` (unix ms), in `tz`.
    pub fn next_after(&self, tz: &TimeZone, after_ms: i64) -> Option<i64> {
        let after = Timestamp::from_millisecond(after_ms).ok()?;
        // Start a few hours early: around a daylight-saving change, a wall-clock time can map to
        // an instant before an earlier wall-clock time's (the 02:30 that becomes 03:30).
        let start = after.checked_sub(3.hours()).ok()?.to_zoned(tz.clone()).datetime();
        let mut best: Option<Timestamp> = None;
        let mut first_civil: Option<DateTime> = None;
        for dt in self.civil_from(start) {
            if let Some(fc) = first_civil
                && dt > fc.checked_add(3.hours()).ok()?
            {
                break;
            }
            let Ok(ts) = tz.to_ambiguous_timestamp(dt).compatible() else { continue };
            if ts > after {
                first_civil.get_or_insert(dt);
                if best.is_none_or(|b| ts < b) {
                    best = Some(ts);
                }
            }
        }
        best.map(|t| t.as_millisecond())
    }

    /// The shortest gap between two of the next `n` fire times after `from_ms` (None: fewer than
    /// two fire times).
    pub fn min_gap_ms(&self, tz: &TimeZone, from_ms: i64, n: usize) -> Option<i64> {
        let mut prev = self.next_after(tz, from_ms)?;
        let mut best: Option<i64> = None;
        for _ in 1..n {
            let Some(next) = self.next_after(tz, prev) else { break };
            let gap = next - prev;
            best = Some(best.map_or(gap, |b: i64| b.min(gap)));
            prev = next;
        }
        best
    }

    /// A plain-English reading: "At 09:00, Monday to Friday", "Every 15 minutes".
    pub fn describe(&self) -> String {
        let minutes = bits_list(self.minutes, 0, 59);
        let hours = bits_list(self.hours as u64, 0, 23);
        let all_hours = hours.len() == 24;
        let step = |v: &[u32], period: u32| -> Option<u32> {
            if v.len() < 2 || v[0] != 0 {
                return None;
            }
            let s = v[1] - v[0];
            (period % s == 0 && v.windows(2).all(|w| w[1] - w[0] == s) && v.len() as u32 == period / s).then_some(s)
        };
        let time = |h: u32, m: u32| format!("{h:02}:{m:02}");
        let when = if minutes.len() == 60 && all_hours {
            "Every minute".to_string()
        } else if let (Some(s), true) = (step(&minutes, 60), all_hours) {
            format!("Every {s} minutes")
        } else if minutes.len() == 1 && all_hours {
            if minutes[0] == 0 { "Every hour, on the hour".to_string() } else { format!("Every hour at :{:02}", minutes[0]) }
        } else if let (1, Some(s)) = (minutes.len(), step(&hours, 24)) {
            format!("Every {s} hours at :{:02}", minutes[0])
        } else if minutes.len() == 1 && hours.len() <= 6 {
            format!("At {}", join_and(&hours.iter().map(|h| time(*h, minutes[0])).collect::<Vec<_>>()))
        } else if minutes.len() <= 4 && hours.len() == 1 {
            format!("At {}", join_and(&minutes.iter().map(|m| time(hours[0], *m)).collect::<Vec<_>>()))
        } else {
            let mins = if minutes.len() == 60 { "every minute".to_string() } else { format!("minute {}", ranges(&minutes)) };
            if all_hours { format!("At {mins}") } else { format!("At {mins} past hour {}", ranges(&hours)) }
        };
        let days = {
            let dow = self.dow_phrase();
            let dom = self.dom_phrase();
            match (self.dom_star, self.dow_star) {
                (true, true) => "every day".to_string(),
                (true, false) => dow,
                (false, true) => dom,
                (false, false) => format!("{dom} or {dow}"),
            }
        };
        let months = bits_list(self.months as u64, 1, 12);
        let in_months = if months.len() == 12 {
            String::new()
        } else {
            format!(", in {}", join_and(&months.iter().map(|m| MONTH_NAMES[*m as usize - 1].to_string()).collect::<Vec<_>>()))
        };
        format!("{when}, {days}{in_months}")
    }

    fn dow_phrase(&self) -> String {
        // Monday first, Sunday last.
        let order = [1u32, 2, 3, 4, 5, 6, 0];
        let on: Vec<u32> = order.iter().copied().filter(|d| self.dow & (1 << d) != 0).collect();
        let name = |d: u32| DAY_NAMES[d as usize].to_string();
        if on.len() == 7 {
            return "every day".into();
        }
        if on == [6, 0] {
            return "on Saturday and Sunday".into();
        }
        // A run of consecutive days (in Monday-first order) of three or more: "Monday to Friday".
        let idx: Vec<usize> = on.iter().map(|d| order.iter().position(|x| x == d).unwrap()).collect();
        if on.len() >= 3 && idx.windows(2).all(|w| w[1] == w[0] + 1) {
            return format!("{} to {}", name(on[0]), name(*on.last().unwrap()));
        }
        format!("on {}", join_and(&on.iter().map(|d| name(*d)).collect::<Vec<_>>()))
    }

    fn dom_phrase(&self) -> String {
        let days = bits_list(self.dom as u64, 1, 31);
        if days.len() == 1 {
            format!("on day {} of the month", days[0])
        } else {
            format!("on days {} of the month", ranges(&days))
        }
    }
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `1,2,3,7` -> "1-3 and 7".
fn ranges(v: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        parts.push(if j > i + 1 { format!("{}-{}", v[i], v[j]) } else if j == i + 1 { format!("{}, {}", v[i], v[j]) } else { v[i].to_string() });
        i = j + 1;
    }
    join_and(&parts)
}

/// A named time zone, or an error saying what's wrong.
pub fn zone(name: &str) -> Result<TimeZone, String> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err("give a time zone name, e.g. Europe/London".into());
    }
    TimeZone::get(name).map_err(|_| format!("unknown time zone `{name}` (use a name like Europe/London or America/New_York)"))
}

/// This machine's time zone by name (UTC when it has none).
pub fn local_zone_name() -> String {
    TimeZone::system().iana_name().unwrap_or("UTC").to_string()
}

/// Minutes since midnight of `ms` in `tz`.
pub fn minute_of_day(tz: &TimeZone, ms: i64) -> Option<u32> {
    let z = Timestamp::from_millisecond(ms).ok()?.to_zoned(tz.clone());
    Some(z.hour() as u32 * 60 + z.minute() as u32)
}

/// `ms` in `tz` with a strftime pattern (e.g. `%Y-%m-%d %H:%M`).
pub fn format(tz: &TimeZone, ms: i64, pattern: &str) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(t) => t.to_zoned(tz.clone()).strftime(pattern).to_string(),
        Err(_) => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(tz: &TimeZone, s: &str) -> i64 {
        let dt: DateTime = s.parse().unwrap();
        tz.to_ambiguous_timestamp(dt).compatible().unwrap().as_millisecond()
    }

    fn fmt(tz: &TimeZone, ms: i64) -> String {
        format(tz, ms, "%Y-%m-%d %H:%M %Z")
    }

    #[test]
    fn parses_fields_and_refuses_nonsense() {
        assert!(Cron::parse("0 9 * * 1-5").is_ok());
        assert!(Cron::parse("*/15 * * * *").is_ok());
        assert!(Cron::parse("0 9 * * mon-fri").is_ok());
        assert!(Cron::parse("0 9 1 jan,jul *").is_ok());
        assert!(Cron::parse("@daily").is_ok());
        assert_eq!(Cron::parse("0 9 * * 7").unwrap(), Cron::parse("0 9 * * 0").unwrap());
        for bad in ["", "* * * *", "60 * * * *", "0 24 * * *", "0 0 0 * *", "0 0 * 13 *", "*/0 * * * *", "5-1 * * * *",
            "0 0 31 2 *", "@reboot", "a b c d e", "1,,2 * * * *"] {
            assert!(Cron::parse(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn readings() {
        let r = |s: &str| Cron::parse(s).unwrap().describe();
        assert_eq!(r("0 9 * * 1-5"), "At 09:00, Monday to Friday");
        assert_eq!(r("0 9 * * *"), "At 09:00, every day");
        assert_eq!(r("30 8 * * 1"), "At 08:30, on Monday");
        assert_eq!(r("0 9 * * 6,0"), "At 09:00, on Saturday and Sunday");
        assert_eq!(r("0 9 * * 1,3,5"), "At 09:00, on Monday, Wednesday and Friday");
        assert_eq!(r("*/15 * * * *"), "Every 15 minutes, every day");
        assert_eq!(r("0 * * * *"), "Every hour, on the hour, every day");
        assert_eq!(r("5 * * * 1-5"), "Every hour at :05, Monday to Friday");
        assert_eq!(r("0 */6 * * *"), "Every 6 hours at :00, every day");
        assert_eq!(r("0 9,17 * * *"), "At 09:00 and 17:00, every day");
        assert_eq!(r("0 9 1 * *"), "At 09:00, on day 1 of the month");
        assert_eq!(r("0 9 1,15 1,7 *"), "At 09:00, on days 1 and 15 of the month, in January and July");
        assert_eq!(r("0 9 1 * 1"), "At 09:00, on day 1 of the month or on Monday");
        assert_eq!(r("0-10 9 * * *"), "At minute 0-10 past hour 9, every day");
    }

    #[test]
    fn next_runs_on_weekdays() {
        let tz = zone("Europe/London").unwrap();
        let c = Cron::parse("0 9 * * 1-5").unwrap();
        // Friday 2026-10-16 10:00 -> Monday 09:00.
        let n = c.next_after(&tz, ms(&tz, "2026-10-16T10:00")).unwrap();
        assert_eq!(fmt(&tz, n), "2026-10-19 09:00 BST");
        // Exactly at a fire time: the next one, never the same.
        let again = c.next_after(&tz, n).unwrap();
        assert_eq!(fmt(&tz, again), "2026-10-20 09:00 BST");
    }

    #[test]
    fn nine_stays_nine_across_daylight_saving() {
        let tz = zone("Europe/London").unwrap();
        let c = Cron::parse("0 9 * * *").unwrap();
        // Clocks go back on Sunday 2026-10-25.
        let sat = c.next_after(&tz, ms(&tz, "2026-10-24T08:00")).unwrap();
        let sun = c.next_after(&tz, sat).unwrap();
        let mon = c.next_after(&tz, sun).unwrap();
        assert_eq!(fmt(&tz, sat), "2026-10-24 09:00 BST");
        assert_eq!(fmt(&tz, sun), "2026-10-25 09:00 GMT");
        assert_eq!(fmt(&tz, mon), "2026-10-26 09:00 GMT");
        assert_eq!(sun - sat, 25 * 3_600_000, "the day the clocks go back is 25 hours long");
        // And forward on Sunday 2026-03-29.
        let ny = zone("America/New_York").unwrap();
        let a = c.next_after(&ny, ms(&ny, "2026-03-06T12:00")).unwrap();
        let b = c.next_after(&ny, a).unwrap();
        assert_eq!(fmt(&ny, b), "2026-03-08 09:00 EDT");
        assert_eq!(b - a, 23 * 3_600_000);
    }

    #[test]
    fn a_skipped_hour_fires_after_the_change_and_a_repeated_hour_fires_once() {
        let tz = zone("Europe/London").unwrap();
        // 01:30 doesn't exist on 2026-03-29 (01:00 -> 02:00): it fires at 02:30 BST.
        let c = Cron::parse("30 1 * * *").unwrap();
        let n = c.next_after(&tz, ms(&tz, "2026-03-28T12:00")).unwrap();
        assert_eq!(fmt(&tz, n), "2026-03-29 02:30 BST");
        // 01:30 happens twice on 2026-10-25: once, the first time.
        let n = c.next_after(&tz, ms(&tz, "2026-10-24T12:00")).unwrap();
        assert_eq!(fmt(&tz, n), "2026-10-25 01:30 BST");
        let next = c.next_after(&tz, n).unwrap();
        assert_eq!(fmt(&tz, next), "2026-10-26 01:30 GMT");
        // Every 15 minutes through the repeated hour: never twice for one wall-clock time, and
        // never less than 15 minutes apart.
        let q = Cron::parse("*/15 * * * *").unwrap();
        let mut t = ms(&tz, "2026-10-24T23:00");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            let n = q.next_after(&tz, t).unwrap();
            assert!(n - t >= 15 * 60_000 || t == ms(&tz, "2026-10-24T23:00"), "gap {} at {}", n - t, fmt(&tz, n));
            assert!(seen.insert(format(&tz, n, "%H:%M")) || format(&tz, n, "%H") != "01", "01:xx repeated at {}", fmt(&tz, n));
            t = n;
        }
        // Through the skipped hour: the times stay in order.
        let mut t = ms(&tz, "2026-03-28T23:50");
        for _ in 0..20 {
            let n = q.next_after(&tz, t).unwrap();
            assert!(n > t);
            t = n;
        }
    }

    #[test]
    fn a_stepped_day_field_restricts_the_days() {
        let tz = zone("UTC").unwrap();
        // Every other day of the month (odd days), not every day.
        let c = Cron::parse("0 9 */2 * *").unwrap();
        let a = c.next_after(&tz, ms(&tz, "2026-10-20T10:00")).unwrap();
        assert_eq!(fmt(&tz, a), "2026-10-21 09:00 UTC");
        assert_eq!(fmt(&tz, c.next_after(&tz, a).unwrap()), "2026-10-23 09:00 UTC");
        // Both restricted: either matches (a Monday, or an odd day).
        let c = Cron::parse("0 9 */2 * 1").unwrap();
        assert_eq!(fmt(&tz, c.next_after(&tz, ms(&tz, "2026-10-23T10:00")).unwrap()), "2026-10-25 09:00 UTC");
        assert_eq!(fmt(&tz, c.next_after(&tz, ms(&tz, "2026-10-25T10:00")).unwrap()), "2026-10-26 09:00 UTC");
    }

    #[test]
    fn rare_days_are_found() {
        let tz = zone("UTC").unwrap();
        // February 29th: the next leap year.
        let c = Cron::parse("0 0 29 2 *").unwrap();
        let n = c.next_after(&tz, ms(&tz, "2026-10-20T00:00")).unwrap();
        assert_eq!(fmt(&tz, n), "2028-02-29 00:00 UTC");
    }

    #[test]
    fn gaps() {
        let tz = zone("UTC").unwrap();
        let from = ms(&tz, "2026-10-20T00:00");
        assert_eq!(Cron::parse("*/15 * * * *").unwrap().min_gap_ms(&tz, from, 50), Some(15 * 60_000));
        assert_eq!(Cron::parse("0,5 9 * * *").unwrap().min_gap_ms(&tz, from, 50), Some(5 * 60_000));
        assert_eq!(Cron::parse("0 9 * * 1-5").unwrap().min_gap_ms(&tz, from, 20), Some(86_400_000));
    }
}
