//! 定时计划：把「工作日上午 9 点」这类说法算成下一次触发时间。
//!
//! 不引入 chrono：只需要"本地日 + 时分"的整数运算，目标板是 riscv64，
//! 少一个依赖就少一份交叉编译风险。代价是**只支持固定时区偏移**（不做 DST）——
//! 对中国时区（UTC+8、无夏令时）是精确的；换到有 DST 的地区要重做这段。

use serde_json::Value;

use crate::ids::Timestamp;

const DAY_MS: i64 = 86_400_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// 一次性：绝对时间戳（毫秒）
    Once { at: Timestamp },
    /// 每天 hh:mm（本地时间）
    Daily { hh: u8, mm: u8 },
    /// 周一到周五 hh:mm
    Weekdays { hh: u8, mm: u8 },
    /// 每周指定几天，0=周日 … 6=周六
    Weekly { days: Vec<u8>, hh: u8, mm: u8 },
}

impl Schedule {
    /// 严格晚于 `now` 的下一次触发时间；一次性任务过期后返回 None。
    pub fn next_after(&self, now: Timestamp, tz_min: i32) -> Option<Timestamp> {
        let tz = tz_min as i64 * 60_000;
        match self {
            Schedule::Once { at } => (*at > now).then_some(*at),
            Schedule::Daily { hh, mm } => {
                let mut day = local_day(now, tz);
                let mut at = local_ts(day, *hh, *mm, tz);
                if at <= now {
                    day += 1;
                    at = local_ts(day, *hh, *mm, tz);
                }
                Some(at)
            }
            Schedule::Weekdays { hh, mm } => {
                let mut day = local_day(now, tz);
                for _ in 0..8 {
                    let wd = weekday(day);
                    if (1..=5).contains(&wd) {
                        let at = local_ts(day, *hh, *mm, tz);
                        if at > now {
                            return Some(at);
                        }
                    }
                    day += 1;
                }
                None
            }
            Schedule::Weekly { days, hh, mm } => {
                let mut day = local_day(now, tz);
                for _ in 0..8 {
                    if days.contains(&weekday(day)) {
                        let at = local_ts(day, *hh, *mm, tz);
                        if at > now {
                            return Some(at);
                        }
                    }
                    day += 1;
                }
                None
            }
        }
    }

    /// 给人看的一句话，用于 `/reminders`。
    pub fn describe(&self) -> String {
        match self {
            Schedule::Once { at } => format!("一次性 @ {}", format_local(*at, 480)),
            Schedule::Daily { hh, mm } => format!("每天 {hh:02}:{mm:02}"),
            Schedule::Weekdays { hh, mm } => format!("工作日 {hh:02}:{mm:02}"),
            Schedule::Weekly { days, hh, mm } => {
                let names = ["日", "一", "二", "三", "四", "五", "六"];
                let d: Vec<&str> = days.iter().map(|d| names[(*d as usize) % 7]).collect();
                format!("每周{} {hh:02}:{mm:02}", d.join("/"))
            }
        }
    }
}

fn local_day(now: Timestamp, tz: i64) -> i64 {
    (now + tz).div_euclid(DAY_MS)
}

fn local_ts(day: i64, hh: u8, mm: u8, tz: i64) -> Timestamp {
    day * DAY_MS + (hh as i64 * 60 + mm as i64) * 60_000 - tz
}

/// 0=周日 … 6=周六（1970-01-01 是周四）
fn weekday(day: i64) -> u8 {
    (day + 4).rem_euclid(7) as u8
}

/// `2026-10-29 19:30（周四）`
pub fn format_with_weekday(ts: Timestamp, tz_min: i32) -> String {
    let names = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
    let day = (ts + tz_min as i64 * 60_000).div_euclid(DAY_MS);
    format!("{}（{}）", format_local(ts, tz_min), names[weekday(day) as usize])
}

/// 给模型看的"现在"：`2026-10-26 周一 17:40（UTC+08:00）`
pub fn now_line(ts: Timestamp, tz_min: i32) -> String {
    let names = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];
    let day = (ts + tz_min as i64 * 60_000).div_euclid(DAY_MS);
    let sign = if tz_min < 0 { '-' } else { '+' };
    let abs = tz_min.abs();
    format!(
        "{} {}（UTC{}{:02}:{:02}）",
        format_local(ts, tz_min),
        names[weekday(day) as usize],
        sign,
        abs / 60,
        abs % 60
    )
}

/// 反查：`timestamp` → 本地 (年, 月, 日, 时, 分)
pub fn format_local(ts: Timestamp, tz_min: i32) -> String {
    let tz = tz_min as i64 * 60_000;
    let local = ts + tz;
    let day = local.div_euclid(DAY_MS);
    let min_of_day = local.rem_euclid(DAY_MS) / 60_000;
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", min_of_day / 60, min_of_day % 60)
}

/// Howard Hinnant 的 civil_from_days
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `{"kind":"weekdays","time":"09:00"}` / `{"kind":"once","at":"2026-10-27T09:00"}`
pub fn parse(v: &Value, tz_min: i32) -> Result<Schedule, String> {
    let kind = v
        .get("kind")
        .and_then(|k| k.as_str())
        .ok_or("schedule 缺少 kind（once/daily/weekdays/weekly）")?;
    let time = |v: &Value| -> Result<(u8, u8), String> {
        let t = v.get("time").and_then(|t| t.as_str()).ok_or("缺少 time（HH:MM）")?;
        parse_hhmm(t)
    };
    match kind {
        "once" => {
            let at = v
                .get("at")
                .and_then(|a| a.as_str())
                .ok_or("once 需要 at（YYYY-MM-DDTHH:MM）")?;
            Ok(Schedule::Once { at: parse_local_dt(at, tz_min)? })
        }
        "daily" => {
            let (hh, mm) = time(v)?;
            Ok(Schedule::Daily { hh, mm })
        }
        "weekdays" => {
            let (hh, mm) = time(v)?;
            Ok(Schedule::Weekdays { hh, mm })
        }
        "weekly" => {
            let (hh, mm) = time(v)?;
            let days: Vec<u8> = v
                .get("days")
                .and_then(|d| d.as_array())
                .ok_or("weekly 需要 days（0=周日…6=周六）")?
                .iter()
                .filter_map(|d| d.as_u64())
                .map(|d| (d % 7) as u8)
                .collect();
            if days.is_empty() {
                return Err("weekly 的 days 不能为空".into());
            }
            Ok(Schedule::Weekly { days, hh, mm })
        }
        other => Err(format!("未知 kind：{other}")),
    }
}

fn parse_hhmm(t: &str) -> Result<(u8, u8), String> {
    let (h, m) = t.split_once(':').ok_or("时间格式应为 HH:MM")?;
    let hh: u8 = h.trim().parse().map_err(|_| "小时不合法")?;
    let mm: u8 = m.trim().parse().map_err(|_| "分钟不合法")?;
    if hh > 23 || mm > 59 {
        return Err("时间超出范围".into());
    }
    Ok((hh, mm))
}

fn parse_local_dt(s: &str, tz_min: i32) -> Result<Timestamp, String> {
    let (date, time) = s
        .split_once('T')
        .or_else(|| s.split_once(' '))
        .ok_or("时间应为 YYYY-MM-DDTHH:MM")?;
    let mut d = date.split('-');
    let y: i64 = d.next().ok_or("缺年份")?.parse().map_err(|_| "年份不合法")?;
    let m: u32 = d.next().ok_or("缺月份")?.parse().map_err(|_| "月份不合法")?;
    let day: u32 = d.next().ok_or("缺日期")?.parse().map_err(|_| "日期不合法")?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return Err("日期超出范围".into());
    }
    let (hh, mm) = parse_hhmm(time)?;
    Ok(local_ts(days_from_civil(y, m, day), hh, mm, tz_min as i64 * 60_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-26 是周一
    fn at(y: i64, m: u32, d: u32, hh: u8, mm: u8) -> Timestamp {
        local_ts(days_from_civil(y, m, d), hh, mm, 8 * 60 * 60_000)
    }

    fn fmt(ts: Timestamp) -> String {
        format_local(ts, 480)
    }

    #[test]
    fn weekday_names_are_right() {
        assert_eq!(weekday(days_from_civil(2026, 10, 26)), 1, "应为周一");
        assert_eq!(weekday(days_from_civil(2026, 10, 25)), 0, "应为周日");
    }

    #[test]
    fn weekdays_skips_weekend() {
        let s = Schedule::Weekdays { hh: 9, mm: 0 };
        // 周一 08:00 → 当天 09:00
        assert_eq!(fmt(s.next_after(at(2026, 10, 26, 8, 0), 480).unwrap()), "2026-10-26 09:00");
        // 周一 09:30 → 周二 09:00
        assert_eq!(fmt(s.next_after(at(2026, 10, 26, 9, 30), 480).unwrap()), "2026-10-27 09:00");
        // 周五 10:00 → 下周一 09:00
        assert_eq!(fmt(s.next_after(at(2026, 10, 30, 10, 0), 480).unwrap()), "2026-11-02 09:00");
        // 周日 12:00 → 周一 09:00
        assert_eq!(fmt(s.next_after(at(2026, 11, 1, 12, 0), 480).unwrap()), "2026-11-02 09:00");
    }

    #[test]
    fn daily_rolls_over_midnight() {
        let s = Schedule::Daily { hh: 0, mm: 5 };
        assert_eq!(fmt(s.next_after(at(2026, 10, 26, 23, 59), 480).unwrap()), "2026-10-27 00:05");
    }

    #[test]
    fn weekly_picks_allowed_day() {
        let s = Schedule::Weekly { days: vec![1, 3], hh: 18, mm: 0 };
        // 周一 19:00 → 周三 18:00
        assert_eq!(fmt(s.next_after(at(2026, 10, 26, 19, 0), 480).unwrap()), "2026-10-28 18:00");
    }

    #[test]
    fn once_in_the_past_is_none() {
        let s = Schedule::Once { at: at(2026, 1, 1, 9, 0) };
        assert!(s.next_after(at(2026, 10, 26, 8, 0), 480).is_none());
    }

    #[test]
    fn parses_json_forms() {
        let s = parse(&serde_json::json!({"kind":"weekdays","time":"09:00"}), 480).unwrap();
        assert_eq!(s, Schedule::Weekdays { hh: 9, mm: 0 });
        let s = parse(&serde_json::json!({"kind":"weekly","time":"18:30","days":[1,3]}), 480)
            .unwrap();
        assert_eq!(s, Schedule::Weekly { days: vec![1, 3], hh: 18, mm: 30 });
        assert!(parse(&serde_json::json!({"kind":"weekdays","time":"25:00"}), 480).is_err());
        assert!(parse(&serde_json::json!({"kind":"nope"}), 480).is_err());
    }

    #[test]
    fn now_line_has_weekday() {
        let t = at(2026, 10, 26, 9, 0);
        let line = now_line(t, 480);
        assert!(line.starts_with("2026-10-26 09:00 周一"), "{line}");
        assert!(line.contains("UTC+08:00"), "{line}");
    }

    #[test]
    fn local_formatting_round_trips() {
        let t = at(2026, 10, 26, 9, 0);
        assert_eq!(fmt(t), "2026-10-26 09:00");
        assert_eq!(parse_local_dt("2026-10-26T09:00", 480).unwrap(), t);
    }
}
