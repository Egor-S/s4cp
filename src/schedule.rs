//! 5-field Unix cron expressions evaluated in a timezone (UTC by default).
//!
//! The `cron` crate wants a leading seconds field and numbers days of week 1–7 starting on Sunday,
//! so we parse the Unix day-of-week field ourselves (0–7, 0 and 7 = Sunday, names, ranges, steps)
//! and hand it over as names. Like Vixie cron, when both day-of-month and day-of-week are
//! restricted, a time matches if *either* matches.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::bail;
use crate::util::{Context, Result};

const DOW_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

#[derive(Debug, Clone)]
pub struct Cron {
    expr: String,
    tz: Tz,
    /// One schedule, or two (dom-only, dow-only) for Vixie OR semantics.
    parts: Vec<cron::Schedule>,
}

fn parse_dow_value(s: &str) -> Result<u32> {
    if let Ok(n) = s.parse::<u32>() {
        if n > 7 {
            bail!("day of week {n} out of range 0-7");
        }
        return Ok(n % 7);
    }
    let up = s.to_ascii_uppercase();
    match DOW_NAMES
        .iter()
        .position(|d| up.starts_with(d) && (up.len() == 3 || is_full_name(&up)))
    {
        Some(i) => Ok(i as u32),
        None => bail!("invalid day of week {s:?}"),
    }
}

fn is_full_name(s: &str) -> bool {
    [
        "SUNDAY",
        "MONDAY",
        "TUESDAY",
        "WEDNESDAY",
        "THURSDAY",
        "FRIDAY",
        "SATURDAY",
    ]
    .contains(&s)
}

/// Unix day-of-week field → set of days (0 = Sunday).
fn parse_dow(field: &str) -> Result<[bool; 7]> {
    let mut days = [false; 7];
    for item in field.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => (
                r,
                s.parse::<u32>()
                    .ok()
                    .filter(|&s| s > 0)
                    .context(format!("bad step in {item:?}"))?,
            ),
            None => (item, 1),
        };
        let (lo, hi) = if range == "*" {
            (0, 6)
        } else if let Some((a, b)) = range.split_once('-') {
            let lo = parse_dow_value(a)?;
            // `x-7` means "through Sunday": keep 7 as the upper bound.
            let hi = if b == "7" { 7 } else { parse_dow_value(b)? };
            if hi < lo {
                bail!("invalid day-of-week range {range:?}");
            }
            (lo, hi)
        } else {
            let v = parse_dow_value(range)?;
            (v, if step > 1 { 6 } else { v })
        };
        let mut d = lo;
        while d <= hi {
            days[(d % 7) as usize] = true;
            d += step;
        }
    }
    Ok(days)
}

impl Cron {
    pub fn parse(expr: &str, tz: Option<&str>) -> Result<Cron> {
        let tz: Tz = match tz {
            None => Tz::UTC,
            Some(t) => {
                Tz::from_str(t).map_err(|_| crate::util::msg(format!("unknown timezone {t:?}")))?
            }
        };
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            bail!(
                "cron expression {expr:?} must have 5 fields (minute hour day-of-month month day-of-week)"
            );
        }
        let days = parse_dow(fields[4]).with_context(|| format!("in cron expression {expr:?}"))?;
        let dow_any = fields[4] == "*" || days.iter().all(|&d| d);
        let dow = if dow_any {
            "*".to_string()
        } else {
            (0..7)
                .filter(|&i| days[i])
                .map(|i| DOW_NAMES[i])
                .collect::<Vec<_>>()
                .join(",")
        };
        let dom_any = fields[2] == "*";
        let mk = |dom: &str, dow: &str| -> Result<cron::Schedule> {
            let s = format!(
                "0 {} {} {} {} {}",
                fields[0], fields[1], dom, fields[3], dow
            );
            cron::Schedule::from_str(&s)
                .map_err(|e| crate::util::msg(format!("invalid cron expression {expr:?}: {e}")))
        };
        let parts = if !dom_any && !dow_any {
            vec![mk(fields[2], "*")?, mk("*", &dow)?]
        } else {
            vec![mk(fields[2], &dow)?]
        };
        Ok(Cron {
            expr: expr.to_string(),
            tz,
            parts,
        })
    }

    pub fn expr(&self) -> &str {
        &self.expr
    }

    /// The first fire time strictly after `t`.
    pub fn next_after(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let local = t.with_timezone(&self.tz);
        self.parts
            .iter()
            .filter_map(|p| p.after(&local).next())
            .map(|d| d.with_timezone(&Utc))
            .min()
    }

    /// The latest fire time at or before `t`.
    pub fn prev_at_or_before(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let local = (t + chrono::Duration::seconds(1)).with_timezone(&self.tz);
        self.parts
            .iter()
            .filter_map(|p| p.after(&local).next_back())
            .map(|d| d.with_timezone(&Utc))
            .max()
    }

    /// Whether the schedule fired in `(after, upto]`.
    pub fn fired_between(&self, after: DateTime<Utc>, upto: DateTime<Utc>) -> bool {
        self.next_after(after).is_some_and(|n| n <= upto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone, Timelike, Weekday};

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn unix_day_of_week() {
        // 2026-10-16 is a Friday.
        let t = utc(2026, 10, 16, 12, 0);
        let mon = Cron::parse("0 3 * * 1", None)
            .unwrap()
            .next_after(t)
            .unwrap();
        assert_eq!(mon.weekday(), Weekday::Mon);
        assert_eq!((mon.hour(), mon.minute()), (3, 0));
        for s in ["0 3 * * 0", "0 3 * * 7", "0 3 * * sun"] {
            assert_eq!(
                Cron::parse(s, None)
                    .unwrap()
                    .next_after(t)
                    .unwrap()
                    .weekday(),
                Weekday::Sun,
                "{s}"
            );
        }
        let c = Cron::parse("0 3 * * 5-7", None).unwrap();
        let days: Vec<Weekday> = std::iter::successors(c.next_after(t), |d| c.next_after(*d))
            .take(3)
            .map(|d| d.weekday())
            .collect();
        assert_eq!(days, vec![Weekday::Sat, Weekday::Sun, Weekday::Fri]);
        let c = Cron::parse("0 0 * * 1-5", None).unwrap();
        assert_eq!(c.next_after(t).unwrap().weekday(), Weekday::Mon);
        let c = Cron::parse("0 0 * * */2", None).unwrap(); // Sun, Tue, Thu, Sat
        assert_eq!(c.next_after(t).unwrap().weekday(), Weekday::Sat);
    }

    #[test]
    fn vixie_or_semantics() {
        // Day 1 of the month OR Mondays.
        let c = Cron::parse("0 0 1 * 1", None).unwrap();
        let n = c.next_after(utc(2026, 10, 16, 12, 0)).unwrap();
        assert_eq!(n, utc(2026, 10, 19, 0, 0)); // Monday comes before Nov 1
    }

    #[test]
    fn timezone_and_prev() {
        let c = Cron::parse("30 2 * * *", Some("Europe/Berlin")).unwrap();
        let n = c.next_after(utc(2026, 10, 16, 12, 0)).unwrap();
        assert_eq!(n, utc(2026, 10, 17, 0, 30)); // CEST = UTC+2
        let c = Cron::parse("*/15 * * * *", None).unwrap();
        assert_eq!(
            c.prev_at_or_before(utc(2026, 10, 16, 12, 7)).unwrap(),
            utc(2026, 10, 16, 12, 0)
        );
        assert_eq!(
            c.prev_at_or_before(utc(2026, 10, 16, 12, 15)).unwrap(),
            utc(2026, 10, 16, 12, 15)
        );
        assert!(c.fired_between(utc(2026, 10, 16, 12, 1), utc(2026, 10, 16, 12, 15)));
        assert!(!c.fired_between(utc(2026, 10, 16, 12, 1), utc(2026, 10, 16, 12, 14)));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Cron::parse("0 0 * * * *", None).is_err());
        assert!(Cron::parse("0 0 * *", None).is_err());
        assert!(Cron::parse("0 0 * * 8", None).is_err());
        assert!(Cron::parse("61 0 * * *", None).is_err());
        assert!(Cron::parse("0 0 * * *", Some("Mars/Base")).is_err());
    }
}
