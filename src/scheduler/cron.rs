//! Pure, deterministic 5-field cron parser, timezone/DST resolver, and jittered next-run planner.
//!
//! Zero dependencies outside `chrono` and `sha2` already present in `Cargo.toml`.

use anyhow::{anyhow, bail, Result};
use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc, Weekday,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// Parsed schedule kind (`cron`, `interval`, or `once`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    #[default]
    Cron,
    Interval,
    Once,
}

impl ScheduleKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cron => "cron",
            Self::Interval => "interval",
            Self::Once => "once",
        }
    }

    pub fn from_str_strict(raw: &str) -> Result<Self> {
        let norm = raw.trim().to_ascii_lowercase().replace('-', "_");
        let stripped = norm.strip_prefix("schedule_kind_").unwrap_or(&norm);
        match stripped {
            "cron" => Ok(Self::Cron),
            "interval" => Ok(Self::Interval),
            "once" => Ok(Self::Once),
            other => {
                bail!("Invalid schedule_kind '{other}' (expected 'cron', 'interval', or 'once')")
            }
        }
    }
}

/// Complete validated schedule specification used for computing `next_run_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSpec {
    pub kind: ScheduleKind,
    #[serde(default)]
    pub cron_expr: Option<String>,
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub run_at_unix: Option<i64>,
    #[serde(default)]
    pub jitter_secs: u64,
    #[serde(default = "default_timezone_utc")]
    pub timezone: String,
}

fn default_timezone_utc() -> String {
    "UTC".to_string()
}

impl ScheduleSpec {
    pub fn cron(expr: impl Into<String>) -> Self {
        Self {
            kind: ScheduleKind::Cron,
            cron_expr: Some(expr.into()),
            interval_secs: None,
            run_at_unix: None,
            jitter_secs: 0,
            timezone: "UTC".to_string(),
        }
    }

    pub fn interval(secs: u64) -> Self {
        Self {
            kind: ScheduleKind::Interval,
            cron_expr: None,
            interval_secs: Some(secs),
            run_at_unix: None,
            jitter_secs: 0,
            timezone: "UTC".to_string(),
        }
    }

    pub fn once(run_at_unix: i64) -> Self {
        Self {
            kind: ScheduleKind::Once,
            cron_expr: None,
            interval_secs: None,
            run_at_unix: Some(run_at_unix),
            jitter_secs: 0,
            timezone: "UTC".to_string(),
        }
    }

    pub fn with_timezone(mut self, tz: impl Into<String>) -> Self {
        let t = tz.into().trim().to_string();
        if !t.is_empty() {
            self.timezone = t;
        }
        self
    }

    pub fn with_jitter(mut self, jitter_secs: u64) -> Self {
        self.jitter_secs = jitter_secs;
        self
    }

    /// Validates cron syntax, interval bounds, and timezone resolution.
    pub fn validate(&self) -> Result<()> {
        let _tz = SchedulerTimeZone::parse(&self.timezone)?;
        if self.jitter_secs > 86_400 * 7 {
            bail!(
                "jitter_secs {} exceeds maximum 604800s (7 days)",
                self.jitter_secs
            );
        }
        match self.kind {
            ScheduleKind::Cron => {
                let expr = self
                    .cron_expr
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("cron_expr is required when schedule_kind is 'cron'"))?;
                let _ = CronExpr::parse(expr)?;
            }
            ScheduleKind::Interval => {
                let secs = self.interval_secs.ok_or_else(|| {
                    anyhow!("interval_secs is required when schedule_kind is 'interval'")
                })?;
                if secs == 0 || secs > 365 * 86_400 {
                    bail!("interval_secs {secs} must be within 1..=31536000");
                }
            }
            ScheduleKind::Once => {
                let ts = self.run_at_unix.ok_or_else(|| {
                    anyhow!("run_at_unix is required when schedule_kind is 'once'")
                })?;
                if ts <= 0 {
                    bail!("run_at_unix must be a positive unix timestamp");
                }
            }
        }
        Ok(())
    }
}

/// Bitmask representation of a standard 5-field cron expression (`min hour dom month dow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    pub raw: String,
    minutes: u64,
    hours: u32,
    dom: u32,
    months: u16,
    dow: u8,
    dom_wildcard: bool,
    dow_wildcard: bool,
}

impl CronExpr {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("Cron expression cannot be empty");
        }
        let expanded = match trimmed.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            _ => trimmed,
        };
        let parts: Vec<&str> = expanded.split_whitespace().collect();
        if parts.len() != 5 {
            bail!(
                "Expected 5-field cron expression (minute hour dom month dow), got {} fields in '{}'",
                parts.len(),
                trimmed
            );
        }

        let minutes_mask = parse_field(parts[0], 0, 59, None)?;
        let hours_mask = parse_field(parts[1], 0, 23, None)? as u32;
        let dom_wildcard = parts[2] == "*";
        let dom_mask = parse_field(parts[2], 1, 31, None)? as u32;
        let months_mask = parse_field(parts[3], 1, 12, Some(parse_month_name))? as u16;
        let dow_wildcard = parts[4] == "*";
        let raw_dow_mask = parse_field(parts[4], 0, 7, Some(parse_dow_name))?;
        let mut dow_mask = (raw_dow_mask & 0b0111_1111) as u8;
        if (raw_dow_mask & (1 << 7)) != 0 {
            dow_mask |= 1 << 0; // Sunday = 7 normalizes to 0
        }

        let expr = Self {
            raw: trimmed.to_string(),
            minutes: minutes_mask,
            hours: hours_mask,
            dom: dom_mask,
            months: months_mask,
            dow: dow_mask,
            dom_wildcard,
            dow_wildcard,
        };
        if !expr.has_any_valid_calendar_day() {
            bail!(
                "Cron expression '{}' never matches any valid calendar day (e.g. Feb 31)",
                trimmed
            );
        }
        Ok(expr)
    }

    fn has_any_valid_calendar_day(&self) -> bool {
        if !self.dow_wildcard {
            return self.dow != 0 && self.months != 0;
        }
        for month in 1u32..=12 {
            if (self.months & (1 << month)) == 0 {
                continue;
            }
            let max_days = match month {
                2 => 29,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            for day in 1..=max_days {
                if (self.dom & (1 << day)) != 0 {
                    return true;
                }
            }
        }
        false
    }

    pub fn matches_month(&self, month: u32) -> bool {
        (1..=12).contains(&month) && (self.months & (1 << month)) != 0
    }

    pub fn matches_day(&self, date: NaiveDate) -> bool {
        let dom_match = (self.dom & (1 << date.day())) != 0;
        let dow_num = date.weekday().num_days_from_sunday();
        let dow_match = (self.dow & (1 << dow_num)) != 0;
        if !self.dom_wildcard && !self.dow_wildcard {
            dom_match || dow_match
        } else {
            dom_match && dow_match
        }
    }

    pub fn matches_hour(&self, hour: u32) -> bool {
        hour < 24 && (self.hours & (1 << hour)) != 0
    }

    pub fn matches_minute(&self, minute: u32) -> bool {
        minute < 60 && (self.minutes & (1 << minute)) != 0
    }

    fn next_minute_from(&self, min: u32) -> Option<u32> {
        (min..60).find(|&m| self.matches_minute(m))
    }

    fn next_hour_from(&self, hr: u32) -> Option<u32> {
        (hr..24).find(|&h| self.matches_hour(h))
    }

    fn next_month_from(&self, month: u32) -> Option<u32> {
        (month..=12).find(|&m| self.matches_month(m))
    }

    /// Computes the next UTC unix timestamp strictly after `after_unix` in timezone `tz`.
    pub fn next_run_after(&self, tz: &SchedulerTimeZone, after_unix: i64) -> Result<i64> {
        let start_utc = DateTime::<Utc>::from_timestamp(after_unix, 0)
            .ok_or_else(|| anyhow!("Invalid unix timestamp {after_unix}"))?;
        let local_after = tz.utc_to_local_naive(start_utc);
        // Strictly after `after_unix`, aligned to next whole minute.
        let mut cursor = NaiveDateTime::new(
            local_after.date(),
            NaiveTime::from_hms_opt(local_after.hour(), local_after.minute(), 0)
                .ok_or_else(|| anyhow!("Invalid local time"))?,
        ) + Duration::minutes(1);

        let max_year = cursor.year() + 9;
        let mut safety_steps = 0usize;
        while cursor.year() <= max_year && safety_steps < 20_000 {
            safety_steps += 1;

            let cur_month = cursor.month();
            if !self.matches_month(cur_month) {
                if let Some(next_m) = self.next_month_from(cur_month + 1) {
                    let d = NaiveDate::from_ymd_opt(cursor.year(), next_m, 1)
                        .ok_or_else(|| anyhow!("Invalid calendar date"))?;
                    cursor = NaiveDateTime::new(d, NaiveTime::MIN);
                } else {
                    let d = NaiveDate::from_ymd_opt(cursor.year() + 1, 1, 1)
                        .ok_or_else(|| anyhow!("Invalid calendar date"))?;
                    cursor = NaiveDateTime::new(d, NaiveTime::MIN);
                }
                continue;
            }

            if !self.matches_day(cursor.date()) {
                let next_d = cursor
                    .date()
                    .succ_opt()
                    .ok_or_else(|| anyhow!("Date overflow"))?;
                cursor = NaiveDateTime::new(next_d, NaiveTime::MIN);
                continue;
            }

            let cur_hour = cursor.hour();
            if !self.matches_hour(cur_hour) {
                if let Some(next_h) = self.next_hour_from(cur_hour + 1) {
                    let t = NaiveTime::from_hms_opt(next_h, 0, 0)
                        .ok_or_else(|| anyhow!("Invalid hour"))?;
                    cursor = NaiveDateTime::new(cursor.date(), t);
                } else {
                    let next_d = cursor
                        .date()
                        .succ_opt()
                        .ok_or_else(|| anyhow!("Date overflow"))?;
                    cursor = NaiveDateTime::new(next_d, NaiveTime::MIN);
                }
                continue;
            }

            let cur_min = cursor.minute();
            if !self.matches_minute(cur_min) {
                if let Some(next_m) = self.next_minute_from(cur_min + 1) {
                    let t = NaiveTime::from_hms_opt(cur_hour, next_m, 0)
                        .ok_or_else(|| anyhow!("Invalid minute"))?;
                    cursor = NaiveDateTime::new(cursor.date(), t);
                } else {
                    cursor += Duration::minutes((60 - cur_min) as i64);
                }
                continue;
            }

            match tz.local_naive_to_utc(cursor) {
                LocalResolution::Unique(ts) | LocalResolution::Ambiguous(ts, _) => {
                    if ts > after_unix {
                        return Ok(ts);
                    }
                    // On fall-back fold when `after_unix` is already past the first occurrence,
                    // advance past this minute so we never fire twice for the same local time.
                    cursor += Duration::minutes(1);
                }
                LocalResolution::NonExistent => {
                    // Spring-forward DST gap: skip forward 1 minute until local time exists again.
                    cursor += Duration::minutes(1);
                }
            }
        }

        bail!(
            "Failed to find next run for cron '{}' within 8 years after {}",
            self.raw,
            after_unix
        )
    }
}

fn parse_month_name(tok: &str) -> Option<u32> {
    match tok.to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

fn parse_dow_name(tok: &str) -> Option<u32> {
    match tok.to_ascii_lowercase().as_str() {
        "sun" => Some(0),
        "mon" => Some(1),
        "tue" => Some(2),
        "wed" => Some(3),
        "thu" => Some(4),
        "fri" => Some(5),
        "sat" => Some(6),
        _ => None,
    }
}

fn parse_atom(
    raw: &str,
    min: u32,
    max: u32,
    named: Option<fn(&str) -> Option<u32>>,
) -> Result<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Empty cron field token");
    }
    let val = if let Some(n) = named.and_then(|f| f(trimmed)) {
        n
    } else {
        trimmed
            .parse::<u32>()
            .map_err(|_| anyhow!("Invalid cron token '{trimmed}'"))?
    };
    if val < min || val > max {
        bail!("Cron value {val} out of range {min}..={max}");
    }
    Ok(val)
}

fn parse_field(
    field: &str,
    min: u32,
    max: u32,
    named: Option<fn(&str) -> Option<u32>>,
) -> Result<u64> {
    let mut mask = 0u64;
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            bail!("Empty segment in cron field '{field}'");
        }
        let (range_part, step) = if let Some((lhs, rhs)) = part.split_once('/') {
            let s = rhs
                .trim()
                .parse::<u32>()
                .map_err(|_| anyhow!("Invalid step '{rhs}' in cron field '{field}'"))?;
            if s == 0 {
                bail!("Step must be >= 1 in cron field '{field}'");
            }
            (lhs.trim(), s)
        } else {
            (part, 1u32)
        };

        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((a_raw, b_raw)) = range_part.split_once('-') {
            let a = parse_atom(a_raw, min, max, named)?;
            let b = parse_atom(b_raw, min, max, named)?;
            if a > b {
                bail!("Invalid descending cron range '{range_part}' in '{field}'");
            }
            (a, b)
        } else {
            let v = parse_atom(range_part, min, max, named)?;
            if part.contains('/') {
                (v, max)
            } else {
                (v, v)
            }
        };

        let mut cur = start;
        while cur <= end {
            mask |= 1u64 << cur;
            cur = cur.saturating_add(step);
        }
    }
    if mask == 0 {
        bail!("Cron field '{field}' matched no values");
    }
    Ok(mask)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalResolution {
    Unique(i64),
    Ambiguous(i64, i64),
    NonExistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DstRule {
    Fixed(i32),
    NorthAmerica2007 { std_offset: i32, dst_offset: i32 },
    EuropeEu { std_offset: i32, dst_offset: i32 },
}

#[derive(Debug, Clone)]
struct TzTransition {
    utc_secs: i64,
    offset_secs: i32,
}

/// Deterministic timezone representation supporting UTC, fixed offsets, North America & EU DST,
/// and system `/usr/share/zoneinfo` TZif v2/v3 files.
#[derive(Debug, Clone)]
pub struct SchedulerTimeZone {
    pub name: String,
    kind: TzImpl,
}

#[derive(Debug, Clone)]
enum TzImpl {
    Rule(DstRule),
    Tzif {
        transitions: Vec<TzTransition>,
        initial_offset: i32,
        all_offsets: Vec<i32>,
    },
}

impl SchedulerTimeZone {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let name = if trimmed.is_empty() { "UTC" } else { trimmed };

        if let Some(rule) = resolve_builtin_rule(name) {
            return Ok(Self {
                name: name.to_string(),
                kind: TzImpl::Rule(rule),
            });
        }

        if let Some(offset_secs) = parse_offset_expr(name) {
            return Ok(Self {
                name: name.to_string(),
                kind: TzImpl::Rule(DstRule::Fixed(offset_secs)),
            });
        }

        if let Some(tzif) = try_load_system_zoneinfo(name)? {
            return Ok(Self {
                name: name.to_string(),
                kind: tzif,
            });
        }

        bail!("Unknown or unsupported timezone '{name}'")
    }

    pub fn offset_at_utc(&self, utc_unix: i64) -> i32 {
        match &self.kind {
            TzImpl::Rule(rule) => match *rule {
                DstRule::Fixed(off) => off,
                DstRule::NorthAmerica2007 {
                    std_offset,
                    dst_offset,
                } => {
                    let dt = DateTime::<Utc>::from_timestamp(utc_unix, 0).unwrap_or_default();
                    let year = dt.year();
                    let (start_utc, end_utc) =
                        north_america_dst_bounds_utc(year, std_offset, dst_offset);
                    if utc_unix >= start_utc && utc_unix < end_utc {
                        dst_offset
                    } else {
                        std_offset
                    }
                }
                DstRule::EuropeEu {
                    std_offset,
                    dst_offset,
                } => {
                    let dt = DateTime::<Utc>::from_timestamp(utc_unix, 0).unwrap_or_default();
                    let year = dt.year();
                    let (start_utc, end_utc) = eu_dst_bounds_utc(year);
                    if utc_unix >= start_utc && utc_unix < end_utc {
                        dst_offset
                    } else {
                        std_offset
                    }
                }
            },
            TzImpl::Tzif {
                transitions,
                initial_offset,
                ..
            } => {
                let idx = transitions.partition_point(|t| t.utc_secs <= utc_unix);
                if idx == 0 {
                    *initial_offset
                } else {
                    transitions[idx - 1].offset_secs
                }
            }
        }
    }

    pub fn utc_to_local_naive(&self, utc_dt: DateTime<Utc>) -> NaiveDateTime {
        let off = self.offset_at_utc(utc_dt.timestamp());
        utc_dt.naive_utc() + Duration::seconds(off as i64)
    }

    pub fn local_naive_to_utc(&self, local: NaiveDateTime) -> LocalResolution {
        let mut candidate_offsets: Vec<i32> = match &self.kind {
            TzImpl::Rule(DstRule::Fixed(off)) => {
                return LocalResolution::Unique(local.and_utc().timestamp() - *off as i64)
            }
            TzImpl::Rule(DstRule::NorthAmerica2007 {
                std_offset,
                dst_offset,
            })
            | TzImpl::Rule(DstRule::EuropeEu {
                std_offset,
                dst_offset,
            }) => vec![*dst_offset, *std_offset],
            TzImpl::Tzif { all_offsets, .. } => {
                let approx_utc = local.and_utc().timestamp();
                let mut offs = vec![
                    self.offset_at_utc(approx_utc - 86_400),
                    self.offset_at_utc(approx_utc),
                    self.offset_at_utc(approx_utc + 86_400),
                ];
                offs.extend_from_slice(all_offsets);
                offs
            }
        };
        candidate_offsets.sort_unstable();
        candidate_offsets.dedup();

        let local_epoch_naive = local.and_utc().timestamp();
        let mut valid_utc = Vec::new();
        for off in candidate_offsets {
            let cand_utc = local_epoch_naive - off as i64;
            if self.offset_at_utc(cand_utc) == off {
                valid_utc.push(cand_utc);
            }
        }
        valid_utc.sort_unstable();
        valid_utc.dedup();
        match valid_utc.as_slice() {
            [] => LocalResolution::NonExistent,
            [single] => LocalResolution::Unique(*single),
            [first, second, ..] => LocalResolution::Ambiguous(*first, *second),
        }
    }
}

fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, nth: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid first of month");
    let first_wd = first.weekday().num_days_from_sunday();
    let target_wd = weekday.num_days_from_sunday();
    let diff = (target_wd + 7 - first_wd) % 7;
    let day = 1 + diff + (nth - 1) * 7;
    NaiveDate::from_ymd_opt(year, month, day).expect("valid nth weekday")
}

fn last_weekday_of_month(year: i32, month: u32, weekday: Weekday) -> NaiveDate {
    let next_month_first = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1).expect("valid date")
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1).expect("valid date")
    };
    let last_day = next_month_first.pred_opt().expect("valid last day");
    let last_wd = last_day.weekday().num_days_from_sunday();
    let target_wd = weekday.num_days_from_sunday();
    let back = (last_wd + 7 - target_wd) % 7;
    last_day - Duration::days(back as i64)
}

fn north_america_dst_bounds_utc(year: i32, std_offset: i32, dst_offset: i32) -> (i64, i64) {
    // 2nd Sunday of March 02:00 local standard time
    let march_sun = nth_weekday_of_month(year, 3, Weekday::Sun, 2);
    let spring_local = NaiveDateTime::new(march_sun, NaiveTime::from_hms_opt(2, 0, 0).unwrap());
    let start_utc = spring_local.and_utc().timestamp() - std_offset as i64;

    // 1st Sunday of November 02:00 local daylight time
    let nov_sun = nth_weekday_of_month(year, 11, Weekday::Sun, 1);
    let fall_local = NaiveDateTime::new(nov_sun, NaiveTime::from_hms_opt(2, 0, 0).unwrap());
    let end_utc = fall_local.and_utc().timestamp() - dst_offset as i64;
    (start_utc, end_utc)
}

fn eu_dst_bounds_utc(year: i32) -> (i64, i64) {
    let mar_sun = last_weekday_of_month(year, 3, Weekday::Sun);
    let oct_sun = last_weekday_of_month(year, 10, Weekday::Sun);
    let start_utc = NaiveDateTime::new(mar_sun, NaiveTime::from_hms_opt(1, 0, 0).unwrap())
        .and_utc()
        .timestamp();
    let end_utc = NaiveDateTime::new(oct_sun, NaiveTime::from_hms_opt(1, 0, 0).unwrap())
        .and_utc()
        .timestamp();
    (start_utc, end_utc)
}

fn resolve_builtin_rule(name: &str) -> Option<DstRule> {
    let h = |hrs: i32| hrs * 3600;
    let hm = |hrs: i32, mins: i32| hrs * 3600 + if hrs < 0 { -mins * 60 } else { mins * 60 };
    match name {
        "UTC" | "utc" | "Z" | "z" | "GMT" | "gmt" | "Etc/UTC" | "Etc/GMT" | "Etc/Universal" => {
            Some(DstRule::Fixed(0))
        }
        "America/Los_Angeles" | "US/Pacific" | "PST8PDT" => Some(DstRule::NorthAmerica2007 {
            std_offset: h(-8),
            dst_offset: h(-7),
        }),
        "America/Denver" | "America/Boise" | "US/Mountain" | "MST7MDT" | "America/Edmonton" => {
            Some(DstRule::NorthAmerica2007 {
                std_offset: h(-7),
                dst_offset: h(-6),
            })
        }
        "America/Phoenix" | "US/Arizona" => Some(DstRule::Fixed(h(-7))),
        "America/Chicago" | "America/Winnipeg" | "US/Central" | "CST6CDT" => {
            Some(DstRule::NorthAmerica2007 {
                std_offset: h(-6),
                dst_offset: h(-5),
            })
        }
        "America/New_York"
        | "America/Detroit"
        | "America/Toronto"
        | "America/Indiana/Indianapolis"
        | "US/Eastern"
        | "EST5EDT" => Some(DstRule::NorthAmerica2007 {
            std_offset: h(-5),
            dst_offset: h(-4),
        }),
        "America/Vancouver" => Some(DstRule::NorthAmerica2007 {
            std_offset: h(-8),
            dst_offset: h(-7),
        }),
        "America/Halifax" => Some(DstRule::NorthAmerica2007 {
            std_offset: h(-4),
            dst_offset: h(-3),
        }),
        "America/St_Johns" => Some(DstRule::NorthAmerica2007 {
            std_offset: hm(-3, 30),
            dst_offset: hm(-2, 30),
        }),
        "America/Anchorage" | "America/Juneau" | "US/Alaska" => Some(DstRule::NorthAmerica2007 {
            std_offset: h(-9),
            dst_offset: h(-8),
        }),
        "America/Adak" | "US/Aleutian" => Some(DstRule::NorthAmerica2007 {
            std_offset: h(-10),
            dst_offset: h(-9),
        }),
        "Pacific/Honolulu" | "US/Hawaii" | "HST" => Some(DstRule::Fixed(h(-10))),
        "America/Puerto_Rico" | "AST" => Some(DstRule::Fixed(h(-4))),
        "PST" => Some(DstRule::Fixed(h(-8))),
        "PDT" => Some(DstRule::Fixed(h(-7))),
        "MST" => Some(DstRule::Fixed(h(-7))),
        "MDT" => Some(DstRule::Fixed(h(-6))),
        "CST" => Some(DstRule::Fixed(h(-6))),
        "CDT" => Some(DstRule::Fixed(h(-5))),
        "EST" => Some(DstRule::Fixed(h(-5))),
        "EDT" => Some(DstRule::Fixed(h(-4))),
        "Europe/London" | "Europe/Dublin" | "Europe/Lisbon" => Some(DstRule::EuropeEu {
            std_offset: 0,
            dst_offset: h(1),
        }),
        "Europe/Berlin" | "Europe/Paris" | "Europe/Amsterdam" | "Europe/Brussels"
        | "Europe/Madrid" | "Europe/Rome" | "Europe/Zurich" | "Europe/Vienna"
        | "Europe/Stockholm" | "Europe/Warsaw" => Some(DstRule::EuropeEu {
            std_offset: h(1),
            dst_offset: h(2),
        }),
        "Europe/Athens" | "Europe/Helsinki" | "Europe/Bucharest" => Some(DstRule::EuropeEu {
            std_offset: h(2),
            dst_offset: h(3),
        }),
        "Asia/Tokyo" | "Asia/Seoul" => Some(DstRule::Fixed(h(9))),
        "Asia/Shanghai" | "Asia/Hong_Kong" | "Asia/Taipei" | "Asia/Singapore" => {
            Some(DstRule::Fixed(h(8)))
        }
        "Asia/Kolkata" => Some(DstRule::Fixed(hm(5, 30))),
        "Asia/Dubai" => Some(DstRule::Fixed(h(4))),
        "Australia/Brisbane" => Some(DstRule::Fixed(h(10))),
        _ => None,
    }
}

fn parse_offset_expr(raw: &str) -> Option<i32> {
    let s = raw.trim();
    let rest = s
        .strip_prefix("UTC")
        .or_else(|| s.strip_prefix("utc"))
        .or_else(|| s.strip_prefix("GMT"))
        .or_else(|| s.strip_prefix("gmt"))
        .unwrap_or(s);
    if rest.is_empty() {
        return None;
    }
    let (sign, digits) = if let Some(r) = rest.strip_prefix('+') {
        (1i32, r)
    } else if let Some(r) = rest.strip_prefix('-') {
        (-1i32, r)
    } else {
        return None;
    };
    let (hrs, mins) = if let Some((h_str, m_str)) = digits.split_once(':') {
        (h_str.parse::<i32>().ok()?, m_str.parse::<i32>().ok()?)
    } else if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_digit()) {
        (
            digits[..2].parse::<i32>().ok()?,
            digits[2..].parse::<i32>().ok()?,
        )
    } else {
        (digits.parse::<i32>().ok()?, 0)
    };
    if !(0..=14).contains(&hrs) || !(0..=59).contains(&mins) {
        return None;
    }
    let total = sign * (hrs * 3600 + mins * 60);
    if total.abs() > 14 * 3600 {
        return None;
    }
    Some(total)
}

fn try_load_system_zoneinfo(name: &str) -> Result<Option<TzImpl>> {
    if name.contains("..")
        || name.starts_with('/')
        || name.ends_with('/')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
    {
        return Ok(None);
    }
    let path = Path::new("/usr/share/zoneinfo").join(name);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    parse_tzif(&bytes).map(Some)
}

fn read_be_u32(data: &[u8], off: usize) -> Result<u32> {
    let slice = data
        .get(off..off + 4)
        .ok_or_else(|| anyhow!("Unexpected EOF in TZif u32"))?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_be_i32(data: &[u8], off: usize) -> Result<i32> {
    let slice = data
        .get(off..off + 4)
        .ok_or_else(|| anyhow!("Unexpected EOF in TZif i32"))?;
    Ok(i32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_be_i64(data: &[u8], off: usize) -> Result<i64> {
    let slice = data
        .get(off..off + 8)
        .ok_or_else(|| anyhow!("Unexpected EOF in TZif i64"))?;
    Ok(i64::from_be_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

fn parse_tzif(bytes: &[u8]) -> Result<TzImpl> {
    if bytes.len() < 44 || &bytes[..4] != b"TZif" {
        bail!("Invalid TZif header");
    }
    let version = bytes[4];
    let v1_isutcnt = read_be_u32(bytes, 20)? as usize;
    let v1_isstdcnt = read_be_u32(bytes, 24)? as usize;
    let v1_leapcnt = read_be_u32(bytes, 28)? as usize;
    let v1_timecnt = read_be_u32(bytes, 32)? as usize;
    let v1_typecnt = read_be_u32(bytes, 36)? as usize;
    let v1_charcnt = read_be_u32(bytes, 40)? as usize;

    let v1_data_len = v1_timecnt * 4
        + v1_timecnt
        + v1_typecnt * 6
        + v1_charcnt
        + v1_leapcnt * 8
        + v1_isstdcnt
        + v1_isutcnt;

    let use_v2 = matches!(version, b'2' | b'3' | b'4') && bytes.len() >= 44 + v1_data_len + 44;
    let (hdr_off, is_64) =
        if use_v2 && bytes.get(44 + v1_data_len..44 + v1_data_len + 4) == Some(b"TZif") {
            (44 + v1_data_len, true)
        } else {
            (0, false)
        };

    let leapcnt = read_be_u32(bytes, hdr_off + 28)? as usize;
    let timecnt = read_be_u32(bytes, hdr_off + 32)? as usize;
    let typecnt = read_be_u32(bytes, hdr_off + 36)? as usize;
    if typecnt == 0 {
        bail!("TZif has zero local time types");
    }

    let time_size = if is_64 { 8 } else { 4 };
    let times_off = hdr_off + 44;
    let types_idx_off = times_off + timecnt * time_size;
    let ttinfo_off = types_idx_off + timecnt;
    let _after_ttinfo = ttinfo_off + typecnt * 6 + leapcnt * (time_size + 4);

    let mut tt_offsets = Vec::with_capacity(typecnt);
    for i in 0..typecnt {
        let off = read_be_i32(bytes, ttinfo_off + i * 6)?;
        tt_offsets.push(off);
    }

    let mut transitions = Vec::with_capacity(timecnt);
    for i in 0..timecnt {
        let ts = if is_64 {
            read_be_i64(bytes, times_off + i * 8)?
        } else {
            read_be_i32(bytes, times_off + i * 4)? as i64
        };
        let type_idx = *bytes
            .get(types_idx_off + i)
            .ok_or_else(|| anyhow!("Missing TZif type index"))? as usize;
        let offset_secs = *tt_offsets
            .get(type_idx)
            .ok_or_else(|| anyhow!("Invalid TZif type index"))?;
        transitions.push(TzTransition {
            utc_secs: ts,
            offset_secs,
        });
    }

    let initial_offset = tt_offsets[0];
    let mut all_offsets = tt_offsets;
    all_offsets.sort_unstable();
    all_offsets.dedup();
    Ok(TzImpl::Tzif {
        transitions,
        initial_offset,
        all_offsets,
    })
}

/// Deterministic bounded jitter in `0..=max_jitter_secs` derived from `(seed_key, base_unix)`.
pub fn deterministic_jitter_secs(seed_key: &str, base_unix: i64, max_jitter_secs: u64) -> u64 {
    if max_jitter_secs == 0 {
        return 0;
    }
    let mut hasher = Sha256::new();
    hasher.update(seed_key.as_bytes());
    hasher.update(b":");
    hasher.update(base_unix.to_le_bytes());
    let digest = hasher.finalize();
    let raw = u64::from_le_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ]);
    raw % (max_jitter_secs + 1)
}

/// Computes the next run unix timestamp after `after_unix` for `spec`, applying deterministic
/// bounded jitter `[0..=spec.jitter_secs]` keyed by `seed_key` (typically `job.id`).
pub fn compute_next_run_at(
    spec: &ScheduleSpec,
    after_unix: i64,
    seed_key: &str,
) -> Result<Option<i64>> {
    spec.validate()?;
    let base_opt = match spec.kind {
        ScheduleKind::Cron => {
            let tz = SchedulerTimeZone::parse(&spec.timezone)?;
            let expr = CronExpr::parse(spec.cron_expr.as_deref().unwrap_or_default())?;
            Some(expr.next_run_after(&tz, after_unix)?)
        }
        ScheduleKind::Interval => {
            let iv = spec.interval_secs.unwrap_or(60).max(1) as i64;
            Some(after_unix.saturating_add(iv))
        }
        ScheduleKind::Once => spec.run_at_unix,
    };
    let Some(base) = base_opt else {
        return Ok(None);
    };
    let jitter = deterministic_jitter_secs(seed_key, base, spec.jitter_secs) as i64;
    Ok(Some(base.saturating_add(jitter)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(hh, mm, ss)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    #[test]
    fn unit_cron_parser_standard_expressions_and_validation() {
        let e = CronExpr::parse("*/15 9-17 * * mon-fri").unwrap();
        assert!(e.matches_minute(0));
        assert!(e.matches_minute(45));
        assert!(!e.matches_minute(10));
        assert!(e.matches_hour(9));
        assert!(e.matches_hour(17));
        assert!(!e.matches_hour(18));

        let sun7 = CronExpr::parse("0 12 * * 7").unwrap();
        assert!(sun7.matches_day(NaiveDate::from_ymd_opt(2026, 11, 15).unwrap())); // Sunday
        assert!(!sun7.matches_day(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap())); // Monday

        // Vixie POSIX DOM OR DOW rule when both are restricted
        let dom_or_dow = CronExpr::parse("0 0 15 * 1").unwrap();
        assert!(dom_or_dow.matches_day(NaiveDate::from_ymd_opt(2026, 11, 15).unwrap())); // 15th (Sun)
        assert!(dom_or_dow.matches_day(NaiveDate::from_ymd_opt(2026, 11, 16).unwrap())); // Mon 16th
        assert!(!dom_or_dow.matches_day(NaiveDate::from_ymd_opt(2026, 11, 17).unwrap())); // Tue 17th

        assert!(CronExpr::parse("* * * *").is_err());
        assert!(CronExpr::parse("60 * * * *").is_err());
        assert!(CronExpr::parse("*/0 * * * *").is_err());
        assert!(CronExpr::parse("0 0 31 2 *").is_err());
    }

    #[test]
    fn unit_cron_next_run_utc_leap_year_and_dst_transitions() {
        let utc = SchedulerTimeZone::parse("UTC").unwrap();
        let expr = CronExpr::parse("30 14 1,15 * *").unwrap();
        assert_eq!(
            expr.next_run_after(&utc, ts(2026, 11, 1, 14, 30, 0))
                .unwrap(),
            ts(2026, 11, 15, 14, 30, 0)
        );

        // Leap year Feb 29
        let leap = CronExpr::parse("0 8 29 2 *").unwrap();
        assert_eq!(
            leap.next_run_after(&utc, ts(2026, 1, 1, 0, 0, 0)).unwrap(),
            ts(2028, 2, 29, 8, 0, 0)
        );

        // US Pacific Spring Forward (2026-03-08 02:00 PST -> 03:00 PDT)
        let la = SchedulerTimeZone::parse("America/Los_Angeles").unwrap();
        // 02:30 local does not exist on 2026-03-08, so `30 2 * * *` skips to 2026-03-09 02:30 PDT (-07:00 = 09:30 UTC)
        let spring_cron = CronExpr::parse("30 2 * * *").unwrap();
        let before_spring = ts(2026, 3, 8, 9, 0, 0); // 01:00 PST
        assert_eq!(
            spring_cron.next_run_after(&la, before_spring).unwrap(),
            ts(2026, 3, 9, 9, 30, 0)
        );

        // US Pacific Fall Back (2026-11-01 01:30 occurs twice: 08:30Z PDT and 09:30Z PST)
        // Must fire at first occurrence (08:30Z), and after 08:30Z advance to next day (Nov 2 01:30 PST = 09:30Z)
        let fall_cron = CronExpr::parse("30 1 * * *").unwrap();
        assert_eq!(
            fall_cron
                .next_run_after(&la, ts(2026, 11, 1, 8, 0, 0))
                .unwrap(),
            ts(2026, 11, 1, 8, 30, 0)
        );
        assert_eq!(
            fall_cron
                .next_run_after(&la, ts(2026, 11, 1, 8, 30, 0))
                .unwrap(),
            ts(2026, 11, 2, 9, 30, 0)
        );
    }

    #[test]
    fn unit_next_run_with_deterministic_jitter() {
        let spec = ScheduleSpec::cron("0 * * * *")
            .with_timezone("UTC")
            .with_jitter(120);
        let after = ts(2026, 11, 16, 4, 15, 0);
        let base = ts(2026, 11, 16, 5, 0, 0);
        let next1 = compute_next_run_at(&spec, after, "job_alpha")
            .unwrap()
            .unwrap();
        let next2 = compute_next_run_at(&spec, after, "job_alpha")
            .unwrap()
            .unwrap();
        assert_eq!(next1, next2);
        assert!((base..=base + 120).contains(&next1));
    }
}
