//! Graph `patternedRecurrence` <-> the RRULE subset the local calendar
//! expander understands (`caldav::rrule`): FREQ, INTERVAL, COUNT, UNTIL and
//! BYDAY (with an ordinal for monthly rules). A rule that does not map
//! returns `None`; the caller then keeps the expanded occurrences Graph's
//! calendarView sends, which is the safe fallback.

use chrono::{Datelike, NaiveDate};
use serde_json::{Value, json};

const DAYS: [(&str, &str); 7] = [
    ("monday", "MO"),
    ("tuesday", "TU"),
    ("wednesday", "WE"),
    ("thursday", "TH"),
    ("friday", "FR"),
    ("saturday", "SA"),
    ("sunday", "SU"),
];
const INDEXES: [(&str, i32); 5] = [
    ("first", 1),
    ("second", 2),
    ("third", 3),
    ("fourth", 4),
    ("last", -1),
];

fn day_code(name: &str) -> Option<&'static str> {
    DAYS.iter()
        .find(|(long, _)| long.eq_ignore_ascii_case(name))
        .map(|(_, code)| *code)
}

fn day_name(code: &str) -> Option<&'static str> {
    DAYS.iter()
        .find(|(_, short)| short.eq_ignore_ascii_case(code))
        .map(|(long, _)| *long)
}

fn weekday_code(date: NaiveDate) -> &'static str {
    DAYS[date.weekday().num_days_from_monday() as usize].1
}

/// The RRULE for a Graph `recurrence` object, given the series' first day.
pub fn rrule_from_recurrence(recurrence: &Value, start: NaiveDate) -> Option<String> {
    let pattern = recurrence.get("pattern")?;
    let range = recurrence.get("range")?;
    let interval = pattern
        .get("interval")
        .and_then(Value::as_i64)
        .filter(|n| (1..=999).contains(n))
        .unwrap_or(1);
    let kind = pattern.get("type")?.as_str()?;
    let days: Vec<&str> = pattern
        .get("daysOfWeek")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut rule = match kind {
        "daily" => format!("FREQ=DAILY;INTERVAL={interval}"),
        "weekly" => {
            let codes: Option<Vec<&str>> = days.iter().map(|d| day_code(d)).collect();
            let mut codes = codes?;
            if codes.is_empty() {
                codes.push(weekday_code(start));
            }
            format!("FREQ=WEEKLY;INTERVAL={interval};BYDAY={}", codes.join(","))
        }
        "absoluteMonthly" => {
            // The expander repeats the start's day of the month.
            let day = pattern.get("dayOfMonth").and_then(Value::as_i64)?;
            if day != i64::from(start.day()) {
                return None;
            }
            format!("FREQ=MONTHLY;INTERVAL={interval}")
        }
        "relativeMonthly" => {
            let [only] = days.as_slice() else {
                return None;
            };
            let index = pattern.get("index").and_then(Value::as_str)?;
            let ordinal = INDEXES
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(index))?
                .1;
            format!(
                "FREQ=MONTHLY;INTERVAL={interval};BYDAY={ordinal}{}",
                day_code(only)?
            )
        }
        "absoluteYearly" => {
            let day = pattern.get("dayOfMonth").and_then(Value::as_i64)?;
            let month = pattern.get("month").and_then(Value::as_i64)?;
            if day != i64::from(start.day()) || month != i64::from(start.month()) {
                return None;
            }
            format!("FREQ=YEARLY;INTERVAL={interval}")
        }
        _ => return None,
    };
    match range.get("type")?.as_str()? {
        "noEnd" => {}
        "numbered" => {
            let count = range
                .get("numberOfOccurrences")
                .and_then(Value::as_i64)
                .filter(|n| *n >= 1)?;
            rule.push_str(&format!(";COUNT={count}"));
        }
        "endDate" => {
            let end =
                NaiveDate::parse_from_str(range.get("endDate")?.as_str()?, "%Y-%m-%d").ok()?;
            rule.push_str(&format!(";UNTIL={}T235959Z", end.format("%Y%m%d")));
        }
        _ => return None,
    }
    Some(rule)
}

/// The Graph `recurrence` object for an RRULE (the subset above), given the
/// series' first day. `None` when the rule is outside that subset.
pub fn recurrence_from_rrule(rrule: &str, start: NaiveDate) -> Option<Value> {
    let mut freq = None;
    let mut interval = 1i64;
    let mut count = None;
    let mut until = None;
    let mut byday: Vec<(i32, &str)> = Vec::new();
    for part in rrule.split(';') {
        let (key, value) = part.split_once('=')?;
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(value.to_ascii_uppercase()),
            "INTERVAL" => interval = value.parse().ok().filter(|n| (1..=999).contains(n))?,
            "COUNT" => count = Some(value.parse::<i64>().ok().filter(|n| *n >= 1)?),
            "UNTIL" => {
                until = Some(NaiveDate::parse_from_str(value.get(..8)?, "%Y%m%d").ok()?);
            }
            "BYDAY" => {
                for token in value.split(',') {
                    let split = token.len().checked_sub(2)?;
                    let (ordinal, code) = token.split_at(split);
                    day_name(code)?;
                    let ordinal = if ordinal.is_empty() {
                        0
                    } else {
                        ordinal.parse().ok()?
                    };
                    byday.push((ordinal, code));
                }
            }
            "WKST" => {}
            _ => return None,
        }
    }
    let pattern = match freq?.as_str() {
        "DAILY" if byday.is_empty() => json!({"type": "daily", "interval": interval}),
        "WEEKLY" if byday.iter().all(|(ordinal, _)| *ordinal == 0) => {
            let days: Vec<&str> = if byday.is_empty() {
                vec![day_name(weekday_code(start))?]
            } else {
                byday
                    .iter()
                    .map(|(_, code)| day_name(code))
                    .collect::<Option<_>>()?
            };
            json!({
                "type": "weekly", "interval": interval,
                "daysOfWeek": days, "firstDayOfWeek": "sunday",
            })
        }
        "MONTHLY" if byday.is_empty() => json!({
            "type": "absoluteMonthly", "interval": interval,
            "dayOfMonth": start.day(),
        }),
        "MONTHLY" => {
            let [(ordinal, code)] = byday.as_slice() else {
                return None;
            };
            let index = INDEXES.iter().find(|(_, n)| n == ordinal)?.0;
            json!({
                "type": "relativeMonthly", "interval": interval,
                "daysOfWeek": [day_name(code)?], "index": index,
            })
        }
        "YEARLY" if byday.is_empty() => json!({
            "type": "absoluteYearly", "interval": interval,
            "dayOfMonth": start.day(), "month": start.month(),
        }),
        _ => return None,
    };
    let start_date = start.format("%Y-%m-%d").to_string();
    let range = match (count, until) {
        (Some(n), _) => json!({
            "type": "numbered", "startDate": start_date, "numberOfOccurrences": n,
        }),
        (None, Some(end)) => json!({
            "type": "endDate", "startDate": start_date,
            "endDate": end.format("%Y-%m-%d").to_string(),
        }),
        (None, None) => json!({"type": "noEnd", "startDate": start_date}),
    };
    Some(json!({"pattern": pattern, "range": range}))
}

/// The calendar dates named in a master's `cancelledOccurrences`: Graph
/// writes them as ids ending in the occurrence's date (`...2026-11-12`).
pub fn cancelled_dates(values: &[String]) -> Vec<NaiveDate> {
    values
        .iter()
        .filter_map(|text| {
            let bytes = text.as_bytes();
            (0..bytes.len().saturating_sub(9)).rev().find_map(|at| {
                let candidate = text.get(at..at + 10)?;
                NaiveDate::parse_from_str(candidate, "%Y-%m-%d").ok()
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap()
    }

    fn recurrence(pattern: Value, range: Value) -> Value {
        json!({"pattern": pattern, "range": range})
    }

    #[test]
    fn graph_patterns_become_rrules() {
        let start = date("2026-11-10"); // a Tuesday
        let no_end = json!({"type": "noEnd", "startDate": "2026-11-10"});
        let daily = recurrence(json!({"type": "daily", "interval": 2}), no_end.clone());
        assert_eq!(
            rrule_from_recurrence(&daily, start).unwrap(),
            "FREQ=DAILY;INTERVAL=2"
        );
        let weekly = recurrence(
            json!({"type": "weekly", "interval": 1, "daysOfWeek": ["monday", "friday"]}),
            json!({"type": "numbered", "startDate": "2026-11-10", "numberOfOccurrences": 8}),
        );
        assert_eq!(
            rrule_from_recurrence(&weekly, start).unwrap(),
            "FREQ=WEEKLY;INTERVAL=1;BYDAY=MO,FR;COUNT=8"
        );
        let ends = recurrence(
            json!({"type": "relativeMonthly", "interval": 1, "daysOfWeek": ["tuesday"], "index": "second"}),
            json!({"type": "endDate", "startDate": "2026-11-10", "endDate": "2027-03-01"}),
        );
        assert_eq!(
            rrule_from_recurrence(&ends, start).unwrap(),
            "FREQ=MONTHLY;INTERVAL=1;BYDAY=2TU;UNTIL=20270301T235959Z"
        );
        let last = recurrence(
            json!({"type": "relativeMonthly", "interval": 3, "daysOfWeek": ["friday"], "index": "last"}),
            no_end.clone(),
        );
        assert_eq!(
            rrule_from_recurrence(&last, start).unwrap(),
            "FREQ=MONTHLY;INTERVAL=3;BYDAY=-1FR"
        );
        let monthly = recurrence(
            json!({"type": "absoluteMonthly", "interval": 1, "dayOfMonth": 10}),
            no_end.clone(),
        );
        assert_eq!(
            rrule_from_recurrence(&monthly, start).unwrap(),
            "FREQ=MONTHLY;INTERVAL=1"
        );
        let yearly = recurrence(
            json!({"type": "absoluteYearly", "interval": 1, "dayOfMonth": 10, "month": 11}),
            no_end,
        );
        assert_eq!(
            rrule_from_recurrence(&yearly, start).unwrap(),
            "FREQ=YEARLY;INTERVAL=1"
        );
    }

    #[test]
    fn patterns_the_expander_cannot_repeat_do_not_map() {
        let start = date("2026-11-10");
        let no_end = json!({"type": "noEnd", "startDate": "2026-11-10"});
        // A day of the month other than the start's, several relative days,
        // and the relative yearly form are outside the local expander.
        let other_day = recurrence(
            json!({"type": "absoluteMonthly", "interval": 1, "dayOfMonth": 31}),
            no_end.clone(),
        );
        assert!(rrule_from_recurrence(&other_day, start).is_none());
        let two_days = recurrence(
            json!({"type": "relativeMonthly", "interval": 1, "daysOfWeek": ["monday", "friday"], "index": "first"}),
            no_end.clone(),
        );
        assert!(rrule_from_recurrence(&two_days, start).is_none());
        let relative_yearly = recurrence(
            json!({"type": "relativeYearly", "interval": 1, "daysOfWeek": ["monday"], "index": "first", "month": 11}),
            no_end,
        );
        assert!(rrule_from_recurrence(&relative_yearly, start).is_none());
        assert!(rrule_from_recurrence(&json!({}), start).is_none());
    }

    #[test]
    fn rrules_round_trip_through_graph_patterns() {
        let start = date("2026-11-10");
        for rule in [
            "FREQ=DAILY;INTERVAL=2",
            "FREQ=WEEKLY;INTERVAL=1;BYDAY=MO,FR;COUNT=8",
            "FREQ=MONTHLY;INTERVAL=1;BYDAY=2TU;UNTIL=20270301T235959Z",
            "FREQ=MONTHLY;INTERVAL=3;BYDAY=-1FR",
            "FREQ=MONTHLY;INTERVAL=1",
            "FREQ=YEARLY;INTERVAL=1",
        ] {
            let graph = recurrence_from_rrule(rule, start).unwrap_or_else(|| panic!("{rule}"));
            assert_eq!(rrule_from_recurrence(&graph, start).as_deref(), Some(rule));
        }
        assert!(recurrence_from_rrule("FREQ=MONTHLY;BYMONTHDAY=3,4", start).is_none());
        assert!(recurrence_from_rrule("FREQ=HOURLY", start).is_none());
    }

    #[test]
    fn cancelled_occurrences_name_their_dates() {
        let dates = cancelled_dates(&[
            "OID.AAMkAGI2.2026-11-17".to_owned(),
            "no date here".to_owned(),
        ]);
        assert_eq!(dates, vec![date("2026-11-17")]);
    }
}
