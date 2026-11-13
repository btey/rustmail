//! Time zones named by Microsoft 365 (Windows names such as "W. Europe
//! Standard Time", or IANA names) turned into UTC instants. No time-zone
//! database is linked, so this holds a small table of the zones people use,
//! each with its standard offset and one of a few daylight-saving rules.
//! A name that is not in the table gives `None`; callers then treat the time
//! as UTC and say so (see ROADMAP Phase 12).

use chrono::{Datelike, NaiveDate, NaiveDateTime, Weekday};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    None,
    /// Second Sunday of March to first Sunday of November, at 02:00 local.
    Us,
    /// Last Sunday of March to last Sunday of October, at 01:00 UTC.
    Eu,
    /// First Sunday of October to first Sunday of April (southern Australia).
    Au,
    /// Last Sunday of September to first Sunday of April.
    Nz,
}

use Rule::{Au, Eu, None as NoDst, Nz, Us};

/// (Windows name, IANA name, standard offset in minutes, daylight rule)
const ZONES: &[(&str, &str, i32, Rule)] = &[
    ("UTC", "UTC", 0, NoDst),
    ("GMT Standard Time", "Europe/London", 0, Eu),
    ("GMT Standard Time", "Europe/Dublin", 0, Eu),
    ("GMT Standard Time", "Europe/Lisbon", 0, Eu),
    ("Greenwich Standard Time", "Atlantic/Reykjavik", 0, NoDst),
    ("W. Europe Standard Time", "Europe/Berlin", 60, Eu),
    ("W. Europe Standard Time", "Europe/Zurich", 60, Eu),
    ("W. Europe Standard Time", "Europe/Amsterdam", 60, Eu),
    ("W. Europe Standard Time", "Europe/Vienna", 60, Eu),
    ("W. Europe Standard Time", "Europe/Rome", 60, Eu),
    ("W. Europe Standard Time", "Europe/Stockholm", 60, Eu),
    ("Romance Standard Time", "Europe/Paris", 60, Eu),
    ("Romance Standard Time", "Europe/Madrid", 60, Eu),
    ("Romance Standard Time", "Europe/Brussels", 60, Eu),
    ("Romance Standard Time", "Europe/Copenhagen", 60, Eu),
    ("Central Europe Standard Time", "Europe/Budapest", 60, Eu),
    ("Central Europe Standard Time", "Europe/Prague", 60, Eu),
    ("Central European Standard Time", "Europe/Warsaw", 60, Eu),
    ("Central European Standard Time", "Europe/Belgrade", 60, Eu),
    ("E. Europe Standard Time", "Europe/Chisinau", 120, Eu),
    ("FLE Standard Time", "Europe/Kiev", 120, Eu),
    ("FLE Standard Time", "Europe/Kyiv", 120, Eu),
    ("FLE Standard Time", "Europe/Helsinki", 120, Eu),
    ("GTB Standard Time", "Europe/Bucharest", 120, Eu),
    ("GTB Standard Time", "Europe/Athens", 120, Eu),
    ("Turkey Standard Time", "Europe/Istanbul", 180, NoDst),
    ("Russian Standard Time", "Europe/Moscow", 180, NoDst),
    (
        "South Africa Standard Time",
        "Africa/Johannesburg",
        120,
        NoDst,
    ),
    ("Egypt Standard Time", "Africa/Cairo", 120, NoDst),
    ("E. Africa Standard Time", "Africa/Nairobi", 180, NoDst),
    ("W. Central Africa Standard Time", "Africa/Lagos", 60, NoDst),
    ("Arab Standard Time", "Asia/Riyadh", 180, NoDst),
    ("Arabian Standard Time", "Asia/Dubai", 240, NoDst),
    ("India Standard Time", "Asia/Kolkata", 330, NoDst),
    ("India Standard Time", "Asia/Calcutta", 330, NoDst),
    ("Pakistan Standard Time", "Asia/Karachi", 300, NoDst),
    ("SE Asia Standard Time", "Asia/Bangkok", 420, NoDst),
    ("SE Asia Standard Time", "Asia/Jakarta", 420, NoDst),
    ("China Standard Time", "Asia/Shanghai", 480, NoDst),
    ("Singapore Standard Time", "Asia/Singapore", 480, NoDst),
    ("Taipei Standard Time", "Asia/Taipei", 480, NoDst),
    ("W. Australia Standard Time", "Australia/Perth", 480, NoDst),
    ("Tokyo Standard Time", "Asia/Tokyo", 540, NoDst),
    ("Korea Standard Time", "Asia/Seoul", 540, NoDst),
    ("AUS Eastern Standard Time", "Australia/Sydney", 600, Au),
    ("AUS Eastern Standard Time", "Australia/Melbourne", 600, Au),
    (
        "E. Australia Standard Time",
        "Australia/Brisbane",
        600,
        NoDst,
    ),
    ("New Zealand Standard Time", "Pacific/Auckland", 720, Nz),
    ("Hawaiian Standard Time", "Pacific/Honolulu", -600, NoDst),
    ("Alaskan Standard Time", "America/Anchorage", -540, Us),
    ("Pacific Standard Time", "America/Los_Angeles", -480, Us),
    ("Pacific Standard Time", "America/Vancouver", -480, Us),
    ("US Mountain Standard Time", "America/Phoenix", -420, NoDst),
    ("Mountain Standard Time", "America/Denver", -420, Us),
    ("Central Standard Time", "America/Chicago", -360, Us),
    (
        "Central America Standard Time",
        "America/Guatemala",
        -360,
        NoDst,
    ),
    (
        "Central Standard Time (Mexico)",
        "America/Mexico_City",
        -360,
        NoDst,
    ),
    ("Eastern Standard Time", "America/New_York", -300, Us),
    ("Eastern Standard Time", "America/Toronto", -300, Us),
    ("US Eastern Standard Time", "America/Indianapolis", -300, Us),
    ("SA Pacific Standard Time", "America/Bogota", -300, NoDst),
    ("Atlantic Standard Time", "America/Halifax", -240, Us),
    ("SA Western Standard Time", "America/La_Paz", -240, NoDst),
    (
        "E. South America Standard Time",
        "America/Sao_Paulo",
        -180,
        NoDst,
    ),
    (
        "Argentina Standard Time",
        "America/Buenos_Aires",
        -180,
        NoDst,
    ),
    ("Pacific SA Standard Time", "America/Santiago", -240, NoDst),
];

fn nth_sunday(year: i32, month: u32, n: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let offset = (7 + Weekday::Sun.num_days_from_monday() as i64
        - first.weekday().num_days_from_monday() as i64)
        % 7;
    first.checked_add_days(chrono::Days::new(offset as u64 + 7 * (n as u64 - 1)))
}

fn last_sunday(year: i32, month: u32) -> Option<NaiveDate> {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let last = NaiveDate::from_ymd_opt(next_year, next_month, 1)?.pred_opt()?;
    let back = last.weekday().num_days_from_sunday() as u64;
    last.checked_sub_days(chrono::Days::new(back))
}

fn at(date: NaiveDate, hour: u32) -> Option<NaiveDateTime> {
    date.and_hms_opt(hour, 0, 0)
}

/// Whether daylight-saving time is in force at this UTC instant.
fn is_dst(rule: Rule, std_minutes: i32, utc: NaiveDateTime) -> Option<bool> {
    let year = utc.year();
    let std = chrono::Duration::minutes(i64::from(std_minutes));
    Some(match rule {
        Rule::None => false,
        Rule::Us => {
            let start = at(nth_sunday(year, 3, 2)?, 2)? - std;
            let end = at(nth_sunday(year, 11, 1)?, 1)? - std;
            utc >= start && utc < end
        }
        Rule::Eu => {
            let start = at(last_sunday(year, 3)?, 1)?;
            let end = at(last_sunday(year, 10)?, 1)?;
            utc >= start && utc < end
        }
        Rule::Au => {
            let start = at(nth_sunday(year, 10, 1)?, 2)? - std;
            let end = at(nth_sunday(year, 4, 1)?, 2)? - std;
            utc >= start || utc < end
        }
        Rule::Nz => {
            let start = at(last_sunday(year, 9)?, 2)? - std;
            let end = at(nth_sunday(year, 4, 1)?, 2)? - std;
            utc >= start || utc < end
        }
    })
}

fn find(zone: &str) -> Option<(i32, Rule)> {
    let zone = zone.trim();
    ZONES
        .iter()
        .find(|(windows, iana, _, _)| {
            windows.eq_ignore_ascii_case(zone) || iana.eq_ignore_ascii_case(zone)
        })
        .map(|(_, _, offset, rule)| (*offset, *rule))
}

/// Whether the zone name is one this table knows.
pub fn is_known(zone: &str) -> bool {
    find(zone).is_some()
}

/// The UTC instant (milliseconds) of a wall-clock time in a named zone, or
/// `None` when the zone is not in the table.
pub fn local_to_utc_ms(local: NaiveDateTime, zone: &str) -> Option<i64> {
    let (standard, rule) = find(zone)?;
    let as_standard = local - chrono::Duration::minutes(i64::from(standard));
    let dst = is_dst(rule, standard, as_standard)?;
    let utc = if dst {
        as_standard - chrono::Duration::minutes(60)
    } else {
        as_standard
    };
    Some(utc.and_utc().timestamp_millis())
}

/// The wall-clock time in a named zone at a UTC instant (milliseconds), or
/// `None` when the zone is not in the table.
pub fn utc_ms_to_local(utc_ms: i64, zone: &str) -> Option<NaiveDateTime> {
    let (standard, rule) = find(zone)?;
    let utc = chrono::DateTime::from_timestamp_millis(utc_ms)?.naive_utc();
    let dst = is_dst(rule, standard, utc)?;
    let minutes = i64::from(standard) + if dst { 60 } else { 0 };
    Some(utc + chrono::Duration::minutes(minutes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn utc(text: &str) -> i64 {
        local(text).and_utc().timestamp_millis()
    }

    #[test]
    fn central_europe_follows_summer_and_winter_time() {
        // 2026-07-01 12:00 in Berlin is 10:00 UTC (CEST), 2026-01-15 is CET.
        assert_eq!(
            local_to_utc_ms(local("2026-07-01T12:00:00"), "W. Europe Standard Time"),
            Some(utc("2026-07-01T10:00:00"))
        );
        assert_eq!(
            local_to_utc_ms(local("2026-01-15T12:00:00"), "Europe/Berlin"),
            Some(utc("2026-01-15T11:00:00"))
        );
        // EU summer time starts on the last Sunday of March (2026-03-29).
        assert_eq!(
            local_to_utc_ms(local("2026-03-28T12:00:00"), "W. Europe Standard Time"),
            Some(utc("2026-03-28T11:00:00"))
        );
        assert_eq!(
            local_to_utc_ms(local("2026-03-30T12:00:00"), "W. Europe Standard Time"),
            Some(utc("2026-03-30T10:00:00"))
        );
    }

    #[test]
    fn us_zones_switch_on_the_second_sunday_of_march() {
        // 2026-03-08 is the second Sunday of March.
        assert_eq!(
            local_to_utc_ms(local("2026-03-07T12:00:00"), "Eastern Standard Time"),
            Some(utc("2026-03-07T17:00:00"))
        );
        assert_eq!(
            local_to_utc_ms(local("2026-03-09T12:00:00"), "Eastern Standard Time"),
            Some(utc("2026-03-09T16:00:00"))
        );
        assert_eq!(
            local_to_utc_ms(local("2026-12-01T09:00:00"), "Pacific Standard Time"),
            Some(utc("2026-12-01T17:00:00"))
        );
    }

    #[test]
    fn southern_zones_have_summer_time_at_new_year() {
        assert_eq!(
            local_to_utc_ms(local("2026-01-10T12:00:00"), "AUS Eastern Standard Time"),
            Some(utc("2026-01-10T01:00:00"))
        );
        assert_eq!(
            local_to_utc_ms(local("2026-07-10T12:00:00"), "AUS Eastern Standard Time"),
            Some(utc("2026-07-10T02:00:00"))
        );
        assert_eq!(
            local_to_utc_ms(local("2026-01-10T12:00:00"), "New Zealand Standard Time"),
            Some(utc("2026-01-09T23:00:00"))
        );
    }

    #[test]
    fn a_utc_instant_turns_back_into_the_wall_clock_of_a_zone() {
        // The inverse of `local_to_utc_ms`, either side of a change.
        for (utc_text, zone, wall) in [
            (
                "2026-03-28T11:00:00",
                "Europe/Berlin",
                "2026-03-28T12:00:00",
            ),
            (
                "2026-03-30T10:00:00",
                "Europe/Berlin",
                "2026-03-30T12:00:00",
            ),
            (
                "2026-03-09T16:00:00",
                "Eastern Standard Time",
                "2026-03-09T12:00:00",
            ),
            (
                "2026-01-10T01:00:00",
                "AUS Eastern Standard Time",
                "2026-01-10T12:00:00",
            ),
            (
                "2026-07-01T06:30:00",
                "India Standard Time",
                "2026-07-01T12:00:00",
            ),
        ] {
            assert_eq!(
                utc_ms_to_local(utc(utc_text), zone),
                Some(local(wall)),
                "{zone}"
            );
            assert_eq!(
                local_to_utc_ms(local(wall), zone),
                Some(utc(utc_text)),
                "{zone}"
            );
        }
        assert!(utc_ms_to_local(0, "Nowhere Time").is_none());
    }

    #[test]
    fn zones_without_summer_time_and_unknown_names() {
        assert_eq!(
            local_to_utc_ms(local("2026-07-01T12:00:00"), "India Standard Time"),
            Some(utc("2026-07-01T06:30:00"))
        );
        assert_eq!(
            local_to_utc_ms(local("2026-07-01T12:00:00"), "UTC"),
            Some(utc("2026-07-01T12:00:00"))
        );
        assert!(local_to_utc_ms(local("2026-07-01T12:00:00"), "Nowhere Time").is_none());
        assert!(is_known("europe/london") && !is_known("Mars/Base"));
    }
}
