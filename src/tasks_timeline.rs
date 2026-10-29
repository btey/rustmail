//! The Tasks timeline (Gantt): which days the chart shows and where each
//! task's bar sits. Pure date arithmetic; the controller turns the result into
//! Slint rows.

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use rustmail_core::tasks::Task;

/// The chart never spans more days than this; a bar outside it is clamped to
/// the edge.
pub(crate) const MAX_DAYS: i64 = 400;
/// The chart is at least this many days wide, so a few tasks do not squeeze
/// into a sliver.
const MIN_DAYS: i64 = 42;

pub(crate) struct DaySnap {
    /// Day of the month.
    pub label: String,
    pub weekend: bool,
    pub today: bool,
    /// "W36" on a Monday (ISO week), empty on the other days.
    pub week: String,
}

pub(crate) struct BarSnap {
    pub id: i64,
    pub title: String,
    pub start_text: String,
    pub due_text: String,
    /// Day index of the first and last day of the bar (inclusive).
    pub first_day: i32,
    pub last_day: i32,
    /// Only one of the two dates is set: drawn as a diamond on that day.
    pub milestone: bool,
    pub done: bool,
    /// Due before today and not done.
    pub overdue: bool,
}

#[derive(Default)]
pub(crate) struct TimelineSnap {
    pub days: Vec<DaySnap>,
    pub bars: Vec<BarSnap>,
    /// Index of today in `days`, -1 when outside the range.
    pub today_index: i32,
    /// Tasks without a start or a due date, which have no bar.
    pub undated: i64,
    /// "October 2026", the month the chart opens on.
    pub title: String,
}

fn local_date(ms: i64) -> Option<NaiveDate> {
    Local.timestamp_millis_opt(ms).single().map(|t| t.date_naive())
}

fn monday_of(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

/// Lay out `tasks` on a day grid around `today`. The grid starts on the Monday
/// a week before the earliest date (or today) and ends four weeks after the
/// latest.
pub(crate) fn build(tasks: &[Task], today: NaiveDate) -> TimelineSnap {
    let dates = |t: &Task| {
        (
            t.start_at.and_then(local_date),
            t.due_at.and_then(local_date),
        )
    };
    let all: Vec<NaiveDate> = tasks
        .iter()
        .flat_map(|t| {
            let (s, d) = dates(t);
            s.into_iter().chain(d)
        })
        .collect();
    let earliest = all.iter().copied().min().map_or(today, |d| d.min(today));
    let latest = all.iter().copied().max().map_or(today, |d| d.max(today));
    let first = monday_of(earliest - Duration::days(7));
    let mut span = (latest + Duration::days(28) - first).num_days();
    span = span.clamp(MIN_DAYS, MAX_DAYS);
    let last_index = (span - 1) as i32;
    let index_of = |d: NaiveDate| ((d - first).num_days() as i32).clamp(0, last_index);

    let days = (0..span)
        .map(|i| {
            let date = first + Duration::days(i);
            let monday = date.weekday().num_days_from_monday() == 0;
            DaySnap {
                label: date.day().to_string(),
                weekend: date.weekday().num_days_from_monday() >= 5,
                today: date == today,
                week: if monday {
                    format!("W{}", date.iso_week().week())
                } else {
                    String::new()
                },
            }
        })
        .collect();

    let mut dated: Vec<(NaiveDate, &Task)> = tasks
        .iter()
        .filter_map(|t| {
            let (s, d) = dates(t);
            s.or(d).map(|first_date| (first_date, t))
        })
        .collect();
    // Earliest first; tasks starting the same day keep their board order.
    dated.sort_by_key(|(date, _)| *date);
    let bars = dated
        .into_iter()
        .map(|(_, t)| {
            let (s, d) = dates(t);
            let start = s.or(d).unwrap_or(today);
            let end = d.or(s).unwrap_or(today);
            BarSnap {
                id: t.id,
                title: t.title.clone(),
                start_text: s.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                due_text: d.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                first_day: index_of(start.min(end)),
                last_day: index_of(end.max(start)),
                milestone: s.is_none() || d.is_none(),
                done: t.completed_at.is_some(),
                overdue: t.completed_at.is_none() && d.is_some_and(|d| d < today),
            }
        })
        .collect();

    let today_offset = (today - first).num_days();
    TimelineSnap {
        days,
        bars,
        today_index: if (0..span).contains(&today_offset) { today_offset as i32 } else { -1 },
        undated: tasks
            .iter()
            .filter(|t| t.start_at.is_none() && t.due_at.is_none())
            .count() as i64,
        title: today.format("%B %Y").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(y: i32, m: u32, d: u32) -> i64 {
        Local
            .from_local_datetime(&NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(12, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp_millis()
    }

    fn task(id: i64, start: Option<i64>, due: Option<i64>) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": id, "uid": format!("u{id}"), "project_id": 1, "column_id": 1,
            "title": format!("Task {id}"), "description": "", "due_at": due,
            "start_at": start, "priority": 0, "context_id": null,
            "assignee_email": null, "assignee_name": null, "position": "a",
            "completed_at": null, "source": "manual", "created_at": 0, "updated_at": 0,
            "label_ids": [], "checklist_total": 0, "checklist_done": 0, "link_count": 0,
            "move_back_on_reply": false, "follow_up_at": null, "quick_capture": false
        }))
        .unwrap()
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn the_grid_starts_on_a_monday_and_bars_sit_on_their_days() {
        // Wednesday 28 Oct 2026; the earliest date is a week before it.
        let today = day(2026, 10, 28);
        let tasks = [
            task(1, Some(at(2026, 10, 21)), Some(at(2026, 10, 30))),
            task(2, None, Some(at(2026, 11, 3))),
        ];
        let t = build(&tasks, today);
        // 21 Oct - 7 days = 14 Oct (Wed) -> Monday 12 Oct.
        assert_eq!(t.days[0].label, "12");
        assert_eq!(t.days[0].week, "W42");
        assert!(t.days[5].weekend && t.days[6].weekend && !t.days[4].weekend);
        assert_eq!(t.today_index, 16);
        assert!(t.days[16].today);
        // Task 1: 21 Oct is day 9, 30 Oct is day 18.
        let one = &t.bars[0];
        assert_eq!((one.id, one.first_day, one.last_day, one.milestone), (1, 9, 18, false));
        // Task 2 has only a due date: a milestone on 3 Nov (day 22).
        let two = &t.bars[1];
        assert_eq!((two.first_day, two.last_day, two.milestone), (22, 22, true));
        assert_eq!(two.due_text, "2026-11-03");
        assert_eq!(two.start_text, "");
        assert!(t.days.len() as i64 >= MIN_DAYS);
    }

    #[test]
    fn undated_tasks_have_no_bar_and_overdue_ones_are_flagged() {
        let today = day(2026, 10, 28);
        let mut done = task(3, None, Some(at(2026, 10, 1)));
        done.completed_at = Some(1);
        let tasks = [
            task(1, None, None),
            task(2, None, Some(at(2026, 10, 1))),
            done,
        ];
        let t = build(&tasks, today);
        assert_eq!(t.undated, 1);
        assert_eq!(t.bars.len(), 2);
        assert!(t.bars.iter().find(|b| b.id == 2).unwrap().overdue);
        assert!(!t.bars.iter().find(|b| b.id == 3).unwrap().overdue);
    }

    #[test]
    fn the_chart_is_capped_and_far_bars_are_clamped_to_its_edge() {
        let today = day(2026, 10, 28);
        let t = build(&[task(1, Some(at(2026, 10, 28)), Some(at(2030, 1, 1)))], today);
        assert_eq!(t.days.len() as i64, MAX_DAYS);
        assert_eq!(t.bars[0].last_day, (MAX_DAYS - 1) as i32);
        // With nothing dated, today still shows with the minimum width.
        let empty = build(&[], today);
        assert_eq!(empty.days.len() as i64, MIN_DAYS);
        assert!(empty.today_index >= 0);
    }
}
