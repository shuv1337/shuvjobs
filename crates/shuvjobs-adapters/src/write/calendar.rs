//! Pure schedule conversions between cron, systemd, and launchd.
//!
//! Nothing here touches a host: these are total functions from one
//! textual schedule to another, so they are cheap to pin with tests.
//!
//! The one semantic trap is day-of-month vs day-of-week. Vixie cron
//! *ORs* them ("the 1st, or any Monday"), while systemd `OnCalendar=`
//! *ANDs* them ("a Monday that is also the 1st"). There is no single
//! `OnCalendar=` expression for the OR case, so a cron expression that
//! restricts both is refused rather than silently converted into a
//! different schedule.

use std::time::Duration;

use plist::Value;
use shuvjobs_core::{Error, Result};

/// systemd day-of-week names, indexed by cron's numbering.
const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Most `StartCalendarInterval` arrays are a handful of entries; a cron
/// expression that expands past this is a mistake, not a schedule.
const MAX_CALENDAR_ENTRIES: usize = 64;

fn invalid(msg: impl Into<String>) -> Error {
    Error::Validation(msg.into())
}

/// The five-field expression an `@alias` stands for, or `None`.
fn alias_to_fields(expr: &str) -> Option<&'static str> {
    Some(match expr {
        "@hourly" => "0 * * * *",
        "@daily" | "@midnight" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        "@yearly" | "@annually" => "0 0 1 1 *",
        _ => return None,
    })
}

fn reboot_error() -> Error {
    invalid("`@reboot` has no calendar equivalent: schedule it with OnBootSec= instead")
}

fn split_fields(expr: &str) -> Result<[&str; 5]> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    match fields.len() {
        5 => Ok([fields[0], fields[1], fields[2], fields[3], fields[4]]),
        n => Err(invalid(format!(
            "cron expression `{expr}` has {n} fields, expected 5"
        ))),
    }
}

/// Bounds cron accepts for a field, for a friendlier error than the
/// scheduler's own.
#[derive(Clone, Copy)]
struct Bounds {
    name: &'static str,
    min: u32,
    max: u32,
}

const MINUTE: Bounds = Bounds {
    name: "minute",
    min: 0,
    max: 59,
};
const HOUR: Bounds = Bounds {
    name: "hour",
    min: 0,
    max: 23,
};
const DOM: Bounds = Bounds {
    name: "day of month",
    min: 1,
    max: 31,
};
const MONTH: Bounds = Bounds {
    name: "month",
    min: 1,
    max: 12,
};
const DOW: Bounds = Bounds {
    name: "day of week",
    min: 0,
    max: 7,
};

fn parse_number(text: &str, bounds: Bounds) -> Result<u32> {
    let n: u32 = text
        .parse()
        .map_err(|_| invalid(format!("`{text}` is not a valid {}", bounds.name)))?;
    if n < bounds.min || n > bounds.max {
        return Err(invalid(format!(
            "{} `{text}` is out of range {}-{}",
            bounds.name, bounds.min, bounds.max
        )));
    }
    Ok(n)
}

/// Render one cron field as its `OnCalendar=` component.
///
/// `*` stays `*`, `a-b` becomes `a..b`, `*/n` stays `*/n` (systemd reads
/// it as "every n from the smallest value", the same as cron), and plain
/// numbers are zero-padded to `width` so the result looks like the
/// canonical `systemd-analyze calendar` output.
fn convert_field(field: &str, bounds: Bounds, width: usize) -> Result<String> {
    let mut parts = Vec::new();
    for part in field.split(',') {
        let (body, step) = match part.split_once('/') {
            Some((body, step)) => {
                let step: u32 = step
                    .parse()
                    .map_err(|_| invalid(format!("`{part}` has an invalid step")))?;
                if step == 0 {
                    return Err(invalid(format!("`{part}` has a zero step")));
                }
                (body, Some(step))
            }
            None => (part, None),
        };
        let rendered = if body == "*" {
            "*".to_string()
        } else if let Some((from, to)) = body.split_once('-') {
            let from = parse_number(from, bounds)?;
            let to = parse_number(to, bounds)?;
            if from > to {
                return Err(invalid(format!(
                    "{} range `{body}` runs backwards",
                    bounds.name
                )));
            }
            format!("{from:0width$}..{to:0width$}")
        } else {
            format!("{:0width$}", parse_number(body, bounds)?)
        };
        parts.push(match step {
            Some(step) => format!("{rendered}/{step}"),
            None => rendered,
        });
    }
    Ok(parts.join(","))
}

fn day_name(n: u32) -> &'static str {
    // cron accepts both 0 and 7 for Sunday.
    DAY_NAMES[(n % 7) as usize]
}

/// Render the day-of-week field as systemd weekday names, or `None` when
/// it is unrestricted.
fn convert_dow(field: &str) -> Result<Option<String>> {
    if field == "*" {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for part in field.split(',') {
        if part.contains('/') {
            return Err(invalid(format!(
                "day-of-week step `{part}` is not supported; list the days instead"
            )));
        }
        if let Some((from, to)) = part.split_once('-') {
            let from = parse_number(from, DOW)?;
            let to = parse_number(to, DOW)?;
            if from % 7 > to % 7 {
                return Err(invalid(format!(
                    "day-of-week range `{part}` wraps around the week; list the days instead"
                )));
            }
            parts.push(format!("{}..{}", day_name(from), day_name(to)));
        } else {
            parts.push(day_name(parse_number(part, DOW)?).to_string());
        }
    }
    Ok(Some(parts.join(",")))
}

/// A cron expression as a systemd `OnCalendar=` value.
pub fn cron_to_oncalendar(expr: &str) -> Result<String> {
    let expr = expr.trim();
    if expr == "@reboot" {
        return Err(reboot_error());
    }
    // systemd has the same shorthands, spelled without the `@`.
    if let Some(shorthand) = match expr {
        "@hourly" => Some("hourly"),
        "@daily" | "@midnight" => Some("daily"),
        "@weekly" => Some("weekly"),
        "@monthly" => Some("monthly"),
        "@yearly" | "@annually" => Some("yearly"),
        _ => None,
    } {
        return Ok(shorthand.to_string());
    }
    if expr.starts_with('@') {
        return Err(invalid(format!("unknown cron shorthand `{expr}`")));
    }

    let [minute, hour, dom, month, dow] = split_fields(expr)?;
    if dom != "*" && dow != "*" {
        return Err(invalid(
            "cron runs a job on the day of month *or* the day of week, while systemd \
             requires both to match; split it into two timers"
                .to_string(),
        ));
    }

    let minute = convert_field(minute, MINUTE, 2)?;
    let hour = convert_field(hour, HOUR, 2)?;
    let dom = convert_field(dom, DOM, 2)?;
    let month = convert_field(month, MONTH, 2)?;
    let dow = convert_dow(dow)?;

    let body = format!("*-{month}-{dom} {hour}:{minute}:00");
    Ok(match dow {
        Some(days) => format!("{days} {body}"),
        None => body,
    })
}

/// A duration as systemd spells it, in the largest unit that divides it
/// exactly so it round-trips through the reader's parser.
pub fn format_systemd_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs == 0 {
        return "0s".to_string();
    }
    if secs.is_multiple_of(86_400) {
        format!("{}d", secs / 86_400)
    } else if secs.is_multiple_of(3_600) {
        format!("{}h", secs / 3_600)
    } else if secs.is_multiple_of(60) {
        format!("{}min", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn integer(n: u32) -> Value {
    Value::Integer(i64::from(n).into())
}

/// The values a launchd `StartCalendarInterval` key takes, or `None` for
/// an unrestricted field (launchd omits the key entirely).
fn calendar_values(field: &str, bounds: Bounds) -> Result<Option<Vec<u32>>> {
    if field == "*" {
        return Ok(None);
    }
    if field.contains('/') || field.contains('-') {
        return Err(invalid(format!(
            "launchd calendar entries take single values or comma lists, not `{field}`"
        )));
    }
    let mut values = Vec::new();
    for part in field.split(',') {
        values.push(parse_number(part, bounds)?);
    }
    Ok(Some(values))
}

/// A cron expression as a launchd `StartCalendarInterval` value: one
/// dictionary, or an array of them when the expression lists several
/// times.
pub fn cron_to_calendar_interval(expr: &str) -> Result<Value> {
    let expr = expr.trim();
    if expr == "@reboot" {
        return Err(invalid(
            "`@reboot` has no calendar equivalent: use RunAtLoad instead".to_string(),
        ));
    }
    let expanded = match alias_to_fields(expr) {
        Some(fields) => fields,
        None if expr.starts_with('@') => {
            return Err(invalid(format!("unknown cron shorthand `{expr}`")))
        }
        None => expr,
    };
    let [minute, hour, dom, month, dow] = split_fields(expanded)?;

    let minutes = calendar_values(minute, MINUTE)?;
    let hours = calendar_values(hour, HOUR)?;
    let days = calendar_values(dom, DOM)?;
    let months = calendar_values(month, MONTH)?;
    // launchd, like cron, treats 0 and 7 as Sunday, but only stores 0-6.
    let weekdays = calendar_values(dow, DOW)?.map(|v| v.into_iter().map(|d| d % 7).collect());

    // A key with no restriction contributes one "unset" slot, so the
    // product below is over the restricted fields only.
    let slots: [(&str, &Option<Vec<u32>>); 5] = [
        ("Minute", &minutes),
        ("Hour", &hours),
        ("Day", &days),
        ("Month", &months),
        ("Weekday", &weekdays),
    ];

    let combos: usize = slots
        .iter()
        .map(|(_, v)| v.as_ref().map_or(1, Vec::len))
        .product();
    if combos > MAX_CALENDAR_ENTRIES {
        return Err(invalid(format!(
            "`{expr}` expands to {combos} launchd calendar entries, more than the \
             {MAX_CALENDAR_ENTRIES} allowed; simplify the schedule"
        )));
    }

    let mut dicts: Vec<plist::Dictionary> = vec![plist::Dictionary::new()];
    for (key, values) in slots {
        let Some(values) = values else { continue };
        let mut next = Vec::with_capacity(dicts.len() * values.len());
        for dict in &dicts {
            for value in values {
                let mut dict = dict.clone();
                dict.insert(key.to_string(), integer(*value));
                next.push(dict);
            }
        }
        dicts = next;
    }

    Ok(if dicts.len() == 1 {
        Value::Dictionary(dicts.remove(0))
    } else {
        Value::Array(dicts.into_iter().map(Value::Dictionary).collect())
    })
}

/// Inverse of the reader's `Hour=9 Minute=0 | Hour=17 Minute=0`
/// rendering, so a schedule shown by the viewer can be edited and
/// written back.
pub fn parse_formatted_calendar_interval(text: &str) -> Result<Value> {
    let mut dicts = Vec::new();
    for chunk in text.split('|') {
        let chunk = chunk.trim();
        if chunk.is_empty() {
            return Err(invalid(format!(
                "`{text}` has an empty calendar entry; expected `Hour=9 Minute=0`"
            )));
        }
        let mut dict = plist::Dictionary::new();
        for pair in chunk.split_whitespace() {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| invalid(format!("`{pair}` is not `Key=value`")))?;
            let bounds = match key {
                "Minute" => MINUTE,
                "Hour" => HOUR,
                "Day" => DOM,
                "Month" => MONTH,
                "Weekday" => Bounds {
                    name: "weekday",
                    min: 0,
                    max: 6,
                },
                other => {
                    return Err(invalid(format!(
                        "`{other}` is not a calendar key; expected one of \
                         Minute, Hour, Day, Month, Weekday"
                    )))
                }
            };
            dict.insert(key.to_string(), integer(parse_number(value, bounds)?));
        }
        dicts.push(dict);
    }
    if dicts.len() == 1 {
        Ok(Value::Dictionary(dicts.remove(0)))
    } else {
        Ok(Value::Array(
            dicts.into_iter().map(Value::Dictionary).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launchd::format_calendar_interval;
    use crate::systemd::parse_systemd_duration;

    /// Key/value pairs of a dict or array of dicts, order-independent.
    fn pairs(value: &Value) -> Vec<Vec<(String, i64)>> {
        fn one(d: &plist::Dictionary) -> Vec<(String, i64)> {
            let mut out: Vec<(String, i64)> = d
                .iter()
                .map(|(k, v)| (k.clone(), v.as_signed_integer().expect("integer")))
                .collect();
            out.sort();
            out
        }
        match value {
            Value::Dictionary(d) => vec![one(d)],
            Value::Array(items) => items
                .iter()
                .map(|v| one(v.as_dictionary().expect("dictionary")))
                .collect(),
            other => panic!("unexpected value {other:?}"),
        }
    }

    #[test]
    fn cron_to_oncalendar_converts_the_common_shapes() {
        let cases = [
            ("0 2 * * *", "*-*-* 02:00:00"),
            ("*/5 * * * *", "*-*-* *:*/5:00"),
            ("0 9 * * 1-5", "Mon..Fri *-*-* 09:00:00"),
            ("0 0 1 * *", "*-*-01 00:00:00"),
            ("0 9 * * 1", "Mon *-*-* 09:00:00"),
            ("0 9 * * 0", "Sun *-*-* 09:00:00"),
            ("0 9 * * 7", "Sun *-*-* 09:00:00"),
            ("30 3 * 1,7 *", "*-01,07-* 03:30:00"),
            ("0 0-6 * * *", "*-*-* 00..06:00:00"),
            ("0 */2 * * *", "*-*-* */2:00:00"),
            ("@daily", "daily"),
            ("@hourly", "hourly"),
            ("@weekly", "weekly"),
            ("@monthly", "monthly"),
            ("@yearly", "yearly"),
            ("@annually", "yearly"),
        ];
        for (expr, expected) in cases {
            assert_eq!(
                cron_to_oncalendar(expr).unwrap_or_else(|e| panic!("{expr}: {e}")),
                expected,
                "converting {expr}"
            );
        }
    }

    #[test]
    fn cron_to_oncalendar_refuses_what_systemd_cannot_express() {
        // cron ORs day-of-month and day-of-week; systemd ANDs them.
        let err = cron_to_oncalendar("0 0 1 * 1").expect_err("must refuse");
        assert!(matches!(err, Error::Validation(_)), "got {err:?}");

        let err = cron_to_oncalendar("@reboot").expect_err("must refuse");
        assert!(err.to_string().contains("OnBootSec"), "got {err}");

        for expr in [
            "0 2 * *",     // too few fields
            "0 2 * * * *", // too many
            "60 * * * *",  // minute out of range
            "0 24 * * *",  // hour out of range
            "0 0 0 * *",   // day 0
            "0 0 * 13 *",  // month 13
            "0 0 * * 8",   // weekday 8
            "*/0 * * * *", // zero step
            "0 9 * * 5-1", // backwards range
            "0 9 * * */2", // weekday step
            "@nope",
        ] {
            assert!(cron_to_oncalendar(expr).is_err(), "{expr} was accepted");
        }
    }

    #[test]
    fn systemd_durations_round_trip_through_the_reader() {
        let cases = [
            (900_u64, "15min"),
            (90, "90s"),
            (86_400, "1d"),
            (3_600, "1h"),
            (7_200, "2h"),
            (172_800, "2d"),
            (45, "45s"),
        ];
        for (secs, expected) in cases {
            let rendered = format_systemd_duration(Duration::from_secs(secs));
            assert_eq!(rendered, expected, "formatting {secs}s");
            assert_eq!(
                parse_systemd_duration(&rendered),
                Some(Duration::from_secs(secs)),
                "re-parsing {rendered}"
            );
        }
        // Zero has a rendering but no round trip: the reader treats a
        // zero interval as "no interval at all".
        assert_eq!(format_systemd_duration(Duration::ZERO), "0s");
        assert_eq!(parse_systemd_duration("0s"), None);
    }

    #[test]
    fn cron_to_calendar_interval_builds_dicts() {
        let single = cron_to_calendar_interval("0 9 * * 1").unwrap();
        assert_eq!(
            pairs(&single),
            vec![vec![
                ("Hour".to_string(), 9),
                ("Minute".to_string(), 0),
                ("Weekday".to_string(), 1),
            ]]
        );
        assert!(matches!(single, Value::Dictionary(_)));

        let two = cron_to_calendar_interval("0 9,17 * * *").unwrap();
        assert!(matches!(two, Value::Array(_)));
        assert_eq!(
            pairs(&two),
            vec![
                vec![("Hour".to_string(), 9), ("Minute".to_string(), 0)],
                vec![("Hour".to_string(), 17), ("Minute".to_string(), 0)],
            ]
        );

        // Sunday is 7 in cron and 0 in launchd.
        assert_eq!(
            pairs(&cron_to_calendar_interval("0 0 * * 7").unwrap()),
            vec![vec![
                ("Hour".to_string(), 0),
                ("Minute".to_string(), 0),
                ("Weekday".to_string(), 0),
            ]]
        );

        // Aliases expand first.
        assert_eq!(
            pairs(&cron_to_calendar_interval("@daily").unwrap()),
            vec![vec![("Hour".to_string(), 0), ("Minute".to_string(), 0)]]
        );
    }

    #[test]
    fn cron_to_calendar_interval_refuses_steps_ranges_and_explosions() {
        for expr in [
            "*/5 * * * *",
            "0 9-17 * * *",
            "@reboot",
            "0 25 * * *",
            "0 2 * *",
        ] {
            let err = cron_to_calendar_interval(expr).expect_err(expr);
            assert!(matches!(err, Error::Validation(_)), "{expr} gave {err:?}");
        }

        // 12 minutes x 12 hours = 144 entries, past the cap.
        let minutes = (0..12).map(|m| m.to_string()).collect::<Vec<_>>().join(",");
        let hours = (0..12).map(|h| h.to_string()).collect::<Vec<_>>().join(",");
        let err = cron_to_calendar_interval(&format!("{minutes} {hours} * * *"))
            .expect_err("must refuse");
        assert!(err.to_string().contains("more than the 64"), "got {err}");
    }

    #[test]
    fn formatted_calendar_intervals_round_trip() {
        for expr in ["0 9 * * 1", "0 9,17 * * *", "0 0 1 * *"] {
            let value = cron_to_calendar_interval(expr).unwrap();
            let text = format_calendar_interval(&value);
            let parsed = parse_formatted_calendar_interval(&text).unwrap();
            assert_eq!(pairs(&parsed), pairs(&value), "round tripping {expr}");
        }
        assert_eq!(
            format_calendar_interval(&cron_to_calendar_interval("0 9,17 * * *").unwrap()),
            "Minute=0 Hour=9 | Minute=0 Hour=17"
        );
    }

    #[test]
    fn parse_formatted_calendar_interval_rejects_junk() {
        for text in ["", "Hour", "Hour=x", "Nope=1", "Hour=9 |", "Hour=99"] {
            assert!(
                parse_formatted_calendar_interval(text).is_err(),
                "{text:?} was accepted"
            );
        }
    }
}
