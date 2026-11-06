//! The Tasks timeline (Gantt): which days the chart shows and where each
//! task's bar sits. Pure date arithmetic; the controller turns the result into
//! Slint rows.

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use rustmail_core::tasks::Task;

/// The chart always spans this many days (52 weeks), whatever the tasks' dates,
/// so its memory use is constant. It opens 12 weeks before this week's Monday;
/// scrolling toward an edge moves the window by whole weeks (`shift_weeks`).
pub(crate) const WINDOW_DAYS: i64 = 364;
/// Weeks of the window that lie before the current week.
const WEEKS_BEFORE: i64 = 12;

pub(crate) struct DaySnap {
    /// Day of the month.
    pub label: String,
    pub weekend: bool,
    pub today: bool,
    /// "W36" on a Monday (ISO week), empty on the other days.
    pub week: String,
    /// "November 2026": the toolbar names the month in the middle of the view.
    pub month: String,
}

#[derive(Clone)]
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
    /// Days past the due date for an overdue task, else 0.
    pub late_days: i32,
    /// The board column, when the tasks are grouped by column, else -1.
    pub column_id: i64,
    /// Days since a sent mail got no reply, -1 when the task is not waiting.
    pub no_reply_days: i32,
    /// The column's colour (hex) when grouped, else empty.
    pub colour: String,
    /// Checklist progress, 0 to 100 (0 when the task has no checklist).
    pub progress: i32,
    /// Linked items by kind, for the LINKS column.
    pub has_mail: bool,
    pub has_sent: bool,
    pub has_event: bool,
    /// A group row (one board column): the span and progress of its tasks.
    pub group: bool,
    /// The board column's name, for the summary card.
    pub status: String,
    /// Calendar days from the first to the last date, both counted.
    pub day_count: i32,
    /// A group row whose tasks are folded away.
    pub collapsed: bool,
    /// Day index where the wait for a reply starts (the sent mail's day, cut at
    /// the bar), -1 when the task is not waiting.
    pub waiting_from: i32,
    /// Mail and event pins on the bar, at the day each happened.
    pub pins: Vec<PinSnap>,
    /// The first few linked items, for the summary card.
    pub links: Vec<LinkLine>,
}

/// A pin on a bar: the day index and what happened (`PinKind`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PinSnap {
    pub day: i32,
    pub kind: PinKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinKind {
    Mail,
    Sent,
    Event,
}

impl PinKind {
    pub fn token(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Sent => "sent",
            Self::Event => "event",
        }
    }
}

/// One linked item in the summary card: kind token and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkLine {
    pub kind: PinKind,
    pub text: String,
}

/// A task's pins as (Unix ms, kind) and its card lines, by task id.
pub(crate) type TaskPins = (i64, Vec<(i64, PinKind)>, Vec<LinkLine>);

/// At most this many bars are laid out; the rest are counted in the toolbar.
pub(crate) const MAX_BARS: usize = 500;
/// Links listed in the summary card.
const CARD_LINKS: usize = 3;

/// A calendar event drawn in the lane above the tasks.
pub(crate) struct EventSnap {
    /// "09:30 Title", or just the title for an all-day event.
    pub label: String,
    pub first_day: i32,
    pub last_day: i32,
    /// Row in the lane (0 to `EVENT_LANES` - 1); overlapping events stack.
    pub lane: i32,
    /// Calendar event row id, its first day as `YYYY-MM-DD`, and whether it
    /// was made in Rustmail (opens in the editor).
    pub id: i64,
    pub date: String,
    pub local: bool,
    /// The calendar's colour (hex), empty for none.
    pub colour: String,
}

/// Rows in the events lane; an event that finds them all taken is counted in
/// `events_hidden` instead of drawn.
pub(crate) const EVENT_LANES: usize = 3;

#[derive(Default)]
pub(crate) struct TimelineSnap {
    /// First day of the grid.
    pub first_date: Option<NaiveDate>,
    /// `first_date` as a day number, so the view can keep its place when the
    /// window moves.
    pub origin_days: i32,
    /// The window is not at its home position.
    pub shifted: bool,
    pub events: Vec<EventSnap>,
    pub events_hidden: i64,
    /// Tasks with dates that did not fit in `MAX_BARS`.
    pub bars_hidden: i64,
    /// Local time of day for the "Today" label, "11:00".
    pub now_text: String,
    pub days: Vec<DaySnap>,
    pub bars: Vec<BarSnap>,
    /// Index of today in `days`, -1 when outside the range.
    pub today_index: i32,
    /// Tasks without a start or a due date, which have no bar.
    pub undated: i64,
    /// "October 2026", the month the chart opens on.
    pub title: String,
    /// Finish-to-start arrows as (prerequisite bar, dependent bar) indexes into
    /// `bars`; a dependency with an undated end draws nothing.
    pub edges: Vec<(i32, i32)>,
}

fn local_date(ms: i64) -> Option<NaiveDate> {
    Local.timestamp_millis_opt(ms).single().map(|t| t.date_naive())
}

fn monday_of(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

/// `build_shifted` with the window at its home position.
#[cfg(test)]
pub(crate) fn build(
    tasks: &[Task],
    today: NaiveDate,
    dependencies: &[(i64, i64)],
    columns: &[(i64, String)],
) -> TimelineSnap {
    build_shifted(tasks, today, dependencies, columns, 0, &[])
}

/// Lay out `tasks` on a day grid; `dependencies` are (prerequisite id,
/// dependent id) pairs. The grid is `WINDOW_DAYS` long and starts on a Monday,
/// `WEEKS_BEFORE` weeks before this week, moved by `shift_weeks`. A bar that
/// only partly falls in the window is cut at its edge; one that lies wholly
/// outside gets an index off the chart, so nothing is drawn.
pub(crate) fn build_shifted(
    tasks: &[Task],
    today: NaiveDate,
    dependencies: &[(i64, i64)],
    columns: &[(i64, String)],
    shift_weeks: i32,
    collapsed: &[i64],
) -> TimelineSnap {
    let dates = |t: &Task| {
        (
            t.start_at.and_then(local_date),
            t.due_at.and_then(local_date),
        )
    };
    let first = monday_of(today) + Duration::days((i64::from(shift_weeks) - WEEKS_BEFORE) * 7);
    let span = WINDOW_DAYS;
    let last_index = (span - 1) as i32;
    // Both ends of a bar are clamped together, so a bar wholly outside the
    // window stays outside (-2 or `span` + 1) instead of landing on the edge.
    let place = |start: NaiveDate, end: NaiveDate| -> (i32, i32) {
        let a = (start - first).num_days();
        let b = (end - first).num_days();
        if b < 0 {
            (-2, -2)
        } else if a > span - 1 {
            (last_index + 2, last_index + 2)
        } else {
            (a.clamp(0, i64::from(last_index)) as i32, b.clamp(0, i64::from(last_index)) as i32)
        }
    };

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
                month: date.format("%B %Y").to_string(),
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
    // With several columns in use the tasks are grouped by column.
    let position = |t: &Task| columns.iter().position(|(id, _)| *id == t.column_id);
    let grouped = columns.len() > 1
        && dated
            .iter()
            .map(|(_, t)| t.column_id)
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1;
    dated.sort_by_key(|(date, t)| (if grouped { position(t) } else { None }, *date));
    let bars: Vec<(i64, BarSnap)> = dated
        .into_iter()
        .map(|(_, t)| {
            let (s, d) = dates(t);
            let start = s.or(d).unwrap_or(today);
            let end = d.or(s).unwrap_or(today);
            (t.column_id, BarSnap {
                id: t.id,
                title: t.title.clone(),
                start_text: s.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                due_text: d.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                first_day: place(start.min(end), end.max(start)).0,
                last_day: place(start.min(end), end.max(start)).1,
                milestone: s.is_none() || d.is_none(),
                done: t.completed_at.is_some(),
                overdue: t.completed_at.is_none() && d.is_some_and(|d| d < today),
                late_days: match d {
                    Some(d) if t.completed_at.is_none() && d < today => {
                        (today - d).num_days().clamp(0, i64::from(i32::MAX)) as i32
                    }
                    _ => 0,
                },
                // A percentage typed by hand wins over the checklist.
                progress: match t.percent {
                    Some(p) => p.clamp(0, 100) as i32,
                    None if t.checklist_total > 0 => {
                        (t.checklist_done.clamp(0, t.checklist_total) * 100 / t.checklist_total)
                            as i32
                    }
                    None => 0,
                },
                column_id: if grouped { t.column_id } else { -1 },
                no_reply_days: -1,
                colour: String::new(),
                has_mail: false,
                has_sent: false,
                has_event: false,
                group: false,
                status: columns
                    .iter()
                    .find(|(id, _)| *id == t.column_id)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default(),
                day_count: (end.max(start) - start.min(end)).num_days().clamp(0, 36500) as i32 + 1,
                collapsed: false,
                waiting_from: -1,
                pins: Vec::new(),
                links: Vec::new(),
            })
        })
        .collect();
    let bars_hidden = bars.len().saturating_sub(MAX_BARS) as i64;
    let mut bars = bars;
    bars.truncate(MAX_BARS);

    let bars: Vec<BarSnap> = if grouped {
        let mut out = Vec::with_capacity(bars.len() + columns.len());
        for (column, name) in columns {
            let members: Vec<&BarSnap> =
                bars.iter().filter(|(c, _)| c == column).map(|(_, b)| b).collect();
            if members.is_empty() {
                continue;
            }
            let done = members.iter().filter(|b| b.done).count();
            out.push(BarSnap {
                id: -1,
                title: name.clone(),
                start_text: members.iter().map(|b| b.start_text.as_str()).filter(|t| !t.is_empty()).min().unwrap_or_default().to_owned(),
                due_text: members.iter().map(|b| b.due_text.as_str()).max().unwrap_or_default().to_owned(),
                first_day: members.iter().map(|b| b.first_day).min().unwrap_or(0).clamp(0, last_index),
                last_day: members.iter().map(|b| b.last_day).max().unwrap_or(0).clamp(0, last_index),
                milestone: false,
                done: done == members.len(),
                overdue: false,
                late_days: 0,
                column_id: *column,
                no_reply_days: -1,
                colour: String::new(),
                progress: (done * 100 / members.len()) as i32,
                has_mail: false,
                has_sent: false,
                has_event: false,
                group: true,
                status: name.clone(),
                day_count: 0,
                collapsed: collapsed.contains(column),
                waiting_from: -1,
                pins: Vec::new(),
                links: Vec::new(),
            });
            if !collapsed.contains(column) {
                out.extend(bars.iter().filter(|(c, _)| c == column).map(|(_, b)| b.clone()));
            }
        }
        out
    } else {
        bars.into_iter().map(|(_, b)| b).collect()
    };
    let at: std::collections::HashMap<i64, i32> = bars
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i as i32))
        .collect();
    let edges = dependencies
        .iter()
        .filter_map(|(pre, dep)| Some((*at.get(pre)?, *at.get(dep)?)))
        .collect();
    let today_offset = (today - first).num_days();
    TimelineSnap {
        first_date: Some(first),
        origin_days: first.num_days_from_ce(),
        shifted: shift_weeks != 0,
        days,
        bars,
        edges,
        today_index: if (0..span).contains(&today_offset) { today_offset as i32 } else { -1 },
        undated: tasks
            .iter()
            .filter(|t| t.start_at.is_none() && t.due_at.is_none())
            .count() as i64,
        title: today.format("%B %Y").to_string(),
        bars_hidden,
        now_text: Local::now().format("%H:%M").to_string(),
        ..TimelineSnap::default()
    }
}

/// Calendar event as the lane needs it: title, start, end (Unix ms) and
/// whether it lasts whole days.
pub(crate) struct LaneEvent {
    pub id: i64,
    pub local: bool,
    pub colour: String,
    pub title: String,
    pub starts_at: i64,
    pub ends_at: Option<i64>,
    pub all_day: bool,
}

impl TimelineSnap {
    /// Record which kinds of items each task links to (task id, mail
    /// received, mail sent, event), for the LINKS column.
    pub(crate) fn set_links(&mut self, links: &[(i64, bool, bool, bool)]) {
        for &(id, mail, sent, event) in links {
            if let Some(b) = self.bars.iter_mut().find(|b| b.id == id) {
                b.has_mail = mail;
                b.has_sent = sent;
                b.has_event = event;
            }
        }
    }

    /// Give each grouped bar and group row its column's colour.
    pub(crate) fn set_colours(&mut self, colours: &[(i64, String)]) {
        for b in self.bars.iter_mut().filter(|b| b.column_id >= 0) {
            if let Some((_, c)) = colours.iter().find(|(id, _)| *id == b.column_id) {
                b.colour = c.clone();
            }
        }
    }

    /// Mark the tasks whose sent mail has gone without a reply, as
    /// (task id, days).
    pub(crate) fn set_no_reply(&mut self, waiting: &[(i64, i64)]) {
        let today = self.today_index;
        for &(id, days) in waiting {
            if let Some(b) = self.bars.iter_mut().find(|b| b.id == id) {
                b.no_reply_days = days.clamp(0, i64::from(i32::MAX)) as i32;
                // The hatched span starts the day the mail went out.
                b.waiting_from = if today >= 0 && !b.milestone {
                    (today - b.no_reply_days).clamp(b.first_day, b.last_day)
                } else {
                    -1
                };
            }
        }
    }

    /// Pins and card lines for each task: (task id, pins as (Unix ms, kind),
    /// lines). A pin outside the grid is dropped; a pin day is clamped to the
    /// bar so it never floats off it.
    pub(crate) fn set_pins(&mut self, pins: &[TaskPins]) {
        let Some(first) = self.first_date else { return };
        let span = self.days.len() as i64;
        for (id, marks, lines) in pins {
            let Some(b) = self.bars.iter_mut().find(|b| b.id == *id) else { continue };
            b.pins = marks
                .iter()
                .filter_map(|&(ms, kind)| {
                    let day = (local_date(ms)? - first).num_days();
                    (0..span).contains(&day).then(|| PinSnap {
                        day: (day as i32).clamp(b.first_day, b.last_day),
                        kind,
                    })
                })
                .collect();
            b.links = lines.iter().take(CARD_LINKS).cloned().collect();
        }
    }

    /// The grid's range as Unix ms (start inclusive, end exclusive), for
    /// asking the calendar which events to show.
    pub(crate) fn range_ms(&self) -> Option<(i64, i64)> {
        let first = self.first_date?;
        let start = Local
            .from_local_datetime(&first.and_hms_opt(0, 0, 0)?)
            .earliest()?
            .timestamp_millis();
        let end = start + self.days.len() as i64 * 86_400_000 + 3_600_000;
        Some((start, end))
    }

    /// Put `events` in the lane: each on the first row free over its days,
    /// earliest first. An event outside the grid is skipped.
    pub(crate) fn set_events(&mut self, events: &[LaneEvent]) {
        let Some(first) = self.first_date else { return };
        let span = self.days.len() as i64;
        let mut order: Vec<&LaneEvent> = events.iter().collect();
        order.sort_by_key(|e| e.starts_at);
        // Last day used so far in each row.
        let mut busy = [-1i64; EVENT_LANES];
        self.events.clear();
        self.events_hidden = 0;
        for e in order {
            let Some(start) = local_date(e.starts_at) else { continue };
            let mut end = e.ends_at.and_then(|ms| {
                // An all-day end is the day after the last one.
                local_date(if e.all_day { ms - 1 } else { ms })
            });
            end = Some(end.map_or(start, |d| d.max(start)));
            let from = (start - first).num_days();
            let to = (end.unwrap_or(start) - first).num_days();
            if to < 0 || from >= span {
                continue;
            }
            let (from, to) = (from.max(0), to.min(span - 1));
            let Some(lane) = busy.iter().position(|&last| last < from) else {
                self.events_hidden += 1;
                continue;
            };
            busy[lane] = to;
            let label = match local_time(e.starts_at) {
                Some(t) if !e.all_day => format!("{t} {}", e.title),
                _ => e.title.clone(),
            };
            self.events.push(EventSnap {
                label,
                first_day: from as i32,
                last_day: to as i32,
                lane: lane as i32,
                id: e.id,
                date: start.format("%Y-%m-%d").to_string(),
                local: e.local,
                colour: e.colour.clone(),
            });
        }
    }
}

/// Which part of a bar is dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragKind {
    /// The whole bar: both dates move.
    Move,
    /// The left edge: the start date.
    Start,
    /// The right edge: the due date.
    Due,
}

impl DragKind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "move" => Some(Self::Move),
            "start" => Some(Self::Start),
            "due" => Some(Self::Due),
            _ => None,
        }
    }
}

/// `ms` moved by whole local calendar days, keeping the time of day.
fn shift_days(ms: i64, days: i64) -> Option<i64> {
    let date = Local.timestamp_millis_opt(ms).single()?.naive_local();
    Local
        .from_local_datetime(&(date + Duration::days(days)))
        .earliest()
        .map(|t| t.timestamp_millis())
}

/// The start and due dates after dragging a bar by `days`. A bar with one date
/// moves that date; its edges cannot be dragged. An edge never crosses the
/// other date.
pub(crate) fn drag_dates(
    start: Option<i64>,
    due: Option<i64>,
    kind: DragKind,
    days: i64,
) -> Result<(Option<i64>, Option<i64>), &'static str> {
    const BAD: &str = "That date is out of range";
    let shift = |ms: Option<i64>| ms.map(|ms| shift_days(ms, days).ok_or(BAD)).transpose();
    let (start, due) = match kind {
        DragKind::Move => (shift(start)?, shift(due)?),
        DragKind::Start | DragKind::Due if start.is_none() || due.is_none() => {
            return Err("Only a bar with a start and a due date has edges");
        }
        DragKind::Start => (shift(start)?, due),
        DragKind::Due => (start, shift(due)?),
    };
    if let (Some(start), Some(due)) = (start, due)
        && start > due
    {
        return Err("The start date must not be after the due date");
    }
    Ok((start, due))
}

fn local_time(ms: i64) -> Option<String> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%H:%M").to_string())
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
            "move_back_on_reply": false, "follow_up_at": null, "quick_capture": false, "percent": null
        }))
        .unwrap()
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn bar_progress_follows_the_checklist() {
        let mut with_list = task(1, Some(at(2026, 10, 21)), Some(at(2026, 10, 30)));
        with_list.checklist_total = 4;
        with_list.checklist_done = 1;
        let plain = task(2, Some(at(2026, 10, 21)), Some(at(2026, 10, 30)));
        let t = build(&[with_list, plain], day(2026, 10, 28), &[], &[]);
        assert_eq!(t.bars[0].progress, 25);
        assert_eq!(t.bars[1].progress, 0);
    }

    #[test]
    fn a_hand_set_percent_beats_the_checklist() {
        let mut t = task(1, Some(at(2026, 10, 21)), Some(at(2026, 10, 30)));
        t.checklist_total = 4;
        t.checklist_done = 1;
        t.percent = Some(80);
        let mut zero = task(2, Some(at(2026, 10, 21)), Some(at(2026, 10, 30)));
        zero.checklist_total = 4;
        zero.checklist_done = 4;
        zero.percent = Some(0);
        let built = build(&[t, zero], day(2026, 10, 28), &[], &[]);
        assert_eq!(built.bars[0].progress, 80);
        assert_eq!(built.bars[1].progress, 0);
    }

    #[test]
    fn tasks_are_grouped_by_column_under_summary_rows() {
        let mut a = task(1, Some(at(2026, 10, 21)), Some(at(2026, 10, 24)));
        a.column_id = 10;
        a.completed_at = Some(1);
        let mut b = task(2, Some(at(2026, 10, 22)), Some(at(2026, 10, 30)));
        b.column_id = 10;
        let mut c = task(3, Some(at(2026, 10, 20)), Some(at(2026, 10, 22)));
        c.column_id = 20;
        let cols = [(10, "Doing".to_string()), (20, "Later".to_string())];
        let t = build(&[a, b, c], day(2026, 10, 28), &[(1, 2)], &cols);
        let titles: Vec<_> = t.bars.iter().map(|b| (b.group, b.id)).collect();
        assert_eq!(titles, [(true, -1), (false, 1), (false, 2), (true, -1), (false, 3)]);
        assert_eq!(t.bars[0].progress, 50);
        assert_eq!(t.bars[0].due_text, "2026-10-30");
        // Edges point at rows, group rows included.
        assert_eq!(t.edges, vec![(1, 2)]);
        // One column in use: no group rows.
        let flat = build(&[task(4, Some(at(2026, 10, 21)), None)], day(2026, 10, 28), &[], &cols);
        assert!(flat.bars.iter().all(|b| !b.group));
        // Grouped bars and their group rows carry the column.
        assert_eq!(t.bars[0].column_id, t.bars[1].column_id);
        assert_ne!(t.bars[0].column_id, t.bars[4].column_id);
        assert!(flat.bars.iter().all(|b| b.column_id == -1));
        let mut coloured = t;
        let first = coloured.bars[0].column_id;
        coloured.set_colours(&[(first, "#ef4444".into())]);
        assert_eq!(coloured.bars[1].colour, "#ef4444");
        assert_eq!(coloured.bars[4].colour, "");
    }

    #[test]
    fn a_task_waiting_for_a_reply_is_marked() {
        let mut t = build(&[task(1, Some(at(2026, 10, 21)), None)], day(2026, 10, 28), &[], &[]);
        assert_eq!(t.bars[0].no_reply_days, -1);
        t.set_no_reply(&[(1, 3), (99, 5)]);
        assert_eq!(t.bars[0].no_reply_days, 3);
    }

    #[test]
    fn links_are_recorded_on_the_matching_bar() {
        let tasks = [
            task(1, Some(at(2026, 10, 21)), Some(at(2026, 10, 30))),
            task(2, Some(at(2026, 10, 21)), Some(at(2026, 10, 30))),
        ];
        let mut t = build(&tasks, day(2026, 10, 28), &[], &[]);
        t.set_links(&[(2, true, false, true), (99, true, true, true)]);
        assert!(!t.bars[0].has_mail && !t.bars[0].has_event);
        assert!(t.bars[1].has_mail && !t.bars[1].has_sent && t.bars[1].has_event);
    }

    #[test]
    fn the_grid_starts_on_a_monday_and_bars_sit_on_their_days() {
        // Wednesday 28 Oct 2026; the earliest date is a week before it.
        let today = day(2026, 10, 28);
        let tasks = [
            task(1, Some(at(2026, 10, 21)), Some(at(2026, 10, 30))),
            task(2, None, Some(at(2026, 11, 3))),
        ];
        let t = build(&tasks, today, &[], &[]);
        // This week's Monday is 26 Oct; the window opens 12 weeks before it.
        assert_eq!(t.days[0].label, "3");
        assert_eq!(t.days[0].week, "W32");
        assert!(t.days[5].weekend && t.days[6].weekend && !t.days[4].weekend);
        assert_eq!(t.today_index, 86);
        assert!(t.days[86].today);
        // Task 1: 21 Oct is day 79, 30 Oct is day 88.
        let one = &t.bars[0];
        assert_eq!((one.id, one.first_day, one.last_day, one.milestone), (1, 79, 88, false));
        // Task 2 has only a due date: a milestone on 3 Nov (day 92).
        let two = &t.bars[1];
        assert_eq!((two.first_day, two.last_day, two.milestone), (92, 92, true));
        assert_eq!(two.due_text, "2026-11-03");
        assert_eq!(two.start_text, "");
        assert_eq!(t.days.len() as i64, WINDOW_DAYS);
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
        let t = build(&tasks, today, &[], &[]);
        assert_eq!(t.undated, 1);
        assert_eq!(t.bars.len(), 2);
        assert!(t.bars.iter().find(|b| b.id == 2).unwrap().overdue);
        assert_eq!(t.bars.iter().find(|b| b.id == 2).unwrap().late_days, 27);
        assert_eq!(t.bars.iter().find(|b| b.id == 3).unwrap().late_days, 0);
        assert!(!t.bars.iter().find(|b| b.id == 3).unwrap().overdue);
    }

    #[test]
    fn dependencies_become_arrows_between_bars_that_exist() {
        let today = day(2026, 10, 28);
        let tasks = [
            task(1, Some(at(2026, 10, 21)), Some(at(2026, 10, 23))),
            task(2, Some(at(2026, 10, 26)), Some(at(2026, 10, 30))),
            task(3, None, None),
        ];
        // 1 -> 2 is drawn; 2 -> 3 has no bar at the end, so it is left out.
        let t = build(&tasks, today, &[(1, 2), (2, 3)], &[]);
        assert_eq!(t.edges, vec![(0, 1)]);
    }

    #[test]
    fn the_window_has_a_fixed_size_and_moves_by_weeks() {
        let today = day(2026, 10, 28);
        // A bar that runs past the window is cut at its edge.
        let t = build(&[task(1, Some(at(2026, 10, 28)), Some(at(2030, 1, 1)))], today, &[], &[]);
        assert_eq!(t.days.len() as i64, WINDOW_DAYS);
        assert_eq!(t.bars[0].last_day, (WINDOW_DAYS - 1) as i32);
        // Without tasks the window is the same size and shows today.
        let empty = build(&[], today, &[], &[]);
        assert_eq!(empty.days.len() as i64, WINDOW_DAYS);
        assert_eq!(empty.today_index, 86);
        // Moved 13 weeks on: the origin advances by 91 days, today falls out
        // of the window and a bar wholly before it is not drawn.
        let old = task(2, Some(at(2026, 10, 1)), Some(at(2026, 10, 5)));
        let moved = build_shifted(&[old], today, &[], &[], 13, &[]);
        assert_eq!(moved.origin_days - empty.origin_days, 91);
        assert_eq!(moved.today_index, -1);
        assert!(moved.bars[0].last_day < 0);
        // A bar wholly after the window is off the chart on the right.
        let late = build_shifted(&[task(3, Some(at(2027, 12, 1)), Some(at(2027, 12, 5)))], today, &[], &[], 0, &[]);
        assert!(late.bars[0].first_day >= WINDOW_DAYS as i32);
    }

    #[test]
    fn events_stack_in_rows_and_overflow_is_counted() {
        let today = day(2026, 10, 28);
        let mut t = build(&[], today, &[], &[]);
        let ev = |title: &str, y, m, d| LaneEvent {
            id: 1,
            local: true,
            colour: String::new(),
            title: title.into(),
            starts_at: at(y, m, d),
            ends_at: Some(at(y, m, d) + 3_600_000),
            all_day: false,
        };
        let mut list = vec![
            ev("A", 2026, 10, 28),
            ev("B", 2026, 10, 28),
            ev("C", 2026, 10, 28),
            ev("D", 2026, 10, 28),
            ev("Far", 2030, 1, 1),
        ];
        // An all-day event ending at midnight covers only its own day.
        list.push(LaneEvent {
            id: 2,
            local: false,
            colour: "#22a06b".into(),
            title: "Day".into(),
            starts_at: at(2026, 10, 30) - 12 * 3_600_000,
            ends_at: Some(at(2026, 10, 31) - 12 * 3_600_000),
            all_day: true,
        });
        t.set_events(&list);
        let lanes: Vec<i32> = t.events.iter().map(|e| e.lane).collect();
        assert_eq!(lanes, vec![0, 1, 2, 0]);
        assert_eq!(t.events_hidden, 1);
        assert_eq!(t.events[0].label, "12:00 A");
        let all_day = t.events.last().unwrap();
        assert_eq!(all_day.label, "Day");
        assert_eq!(all_day.first_day, all_day.last_day);
    }

    #[test]
    fn pins_sit_on_their_day_and_stay_on_the_bar() {
        let today = day(2026, 10, 28);
        let mut t = build(&[task(1, Some(at(2026, 10, 26)), Some(at(2026, 10, 30)))], today, &[], &[]);
        let bar = t.bars[0].clone();
        t.set_pins(&[(
            1,
            vec![
                (at(2026, 10, 28), PinKind::Sent),
                // Before the bar: clamped to its first day.
                (at(2026, 10, 1), PinKind::Mail),
                // Outside the grid: dropped.
                (at(2040, 1, 1), PinKind::Event),
            ],
            vec![LinkLine { kind: PinKind::Sent, text: "Request".into() }; 5],
        )]);
        let b = &t.bars[0];
        assert_eq!(b.pins.len(), 2);
        assert_eq!(b.pins[0].day, bar.first_day + 2);
        assert_eq!(b.pins[1].day, bar.first_day);
        // The card lists a few links only.
        assert_eq!(b.links.len(), 3);
    }

    #[test]
    fn the_wait_for_a_reply_is_hatched_from_the_day_the_mail_went() {
        let today = day(2026, 10, 28);
        let mut t = build(&[task(1, Some(at(2026, 10, 26)), Some(at(2026, 11, 3)))], today, &[], &[]);
        let (first, today_index) = (t.bars[0].first_day, t.today_index);
        t.set_no_reply(&[(1, 2)]);
        assert_eq!(t.bars[0].waiting_from, today_index - 2);
        assert!(t.bars[0].waiting_from >= first);
        // Sent before the bar started: the span starts at the bar.
        t.set_no_reply(&[(1, 30)]);
        assert_eq!(t.bars[0].waiting_from, first);
    }

    #[test]
    fn collapsing_a_group_hides_its_tasks_and_the_cap_counts_the_rest() {
        let today = day(2026, 10, 28);
        let mut a = task(1, Some(at(2026, 10, 26)), Some(at(2026, 10, 30)));
        let mut b = task(2, Some(at(2026, 10, 27)), Some(at(2026, 10, 31)));
        a.column_id = 1;
        b.column_id = 2;
        let columns = vec![(1, "Next".to_owned()), (2, "Waiting".to_owned())];
        let open = build_shifted(&[a.clone(), b.clone()], today, &[], &columns, 0, &[]);
        assert_eq!(open.bars.len(), 4);
        assert_eq!(open.bars[1].status, "Next");
        let folded = build_shifted(&[a, b], today, &[], &columns, 0, &[1]);
        assert_eq!(folded.bars.len(), 3);
        assert!(folded.bars[0].group && folded.bars[0].collapsed);
        // Past the cap, the surplus is counted, not drawn.
        let many: Vec<Task> = (0..(MAX_BARS as i64 + 25))
            .map(|i| task(i + 10, Some(at(2026, 10, 26)), Some(at(2026, 10, 30))))
            .collect();
        let capped = build(&many, today, &[], &[]);
        assert_eq!(capped.bars.len(), MAX_BARS);
        assert_eq!(capped.bars_hidden, 25);
    }

    #[test]
    fn a_thousand_tasks_lay_out_quickly() {
        let today = day(2026, 10, 28);
        let many: Vec<Task> = (0..1000)
            .map(|i| task(i + 1, Some(at(2026, 10, 1) + i * 3_600_000), Some(at(2026, 11, 1))))
            .collect();
        let began = std::time::Instant::now();
        let t = build(&many, today, &[], &[]);
        assert_eq!(t.bars.len(), MAX_BARS);
        assert!(began.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn dragging_moves_whole_days_and_edges_never_cross() {
        let (s, d) = (at(2026, 10, 26), at(2026, 10, 30));
        let day_ms = |ms: i64| local_date(ms).unwrap();
        let (ns, nd) = drag_dates(Some(s), Some(d), DragKind::Move, 3).unwrap();
        assert_eq!(day_ms(ns.unwrap()), day(2026, 10, 29));
        assert_eq!(day_ms(nd.unwrap()), day(2026, 11, 2));
        let (ns, nd) = drag_dates(Some(s), Some(d), DragKind::Due, 2).unwrap();
        assert_eq!(ns, Some(s));
        assert_eq!(day_ms(nd.unwrap()), day(2026, 11, 1));
        let (ns, _) = drag_dates(Some(s), Some(d), DragKind::Start, -5).unwrap();
        assert_eq!(day_ms(ns.unwrap()), day(2026, 10, 21));
        // Crossing the other date is refused.
        assert!(drag_dates(Some(s), Some(d), DragKind::Start, 6).is_err());
        assert!(drag_dates(Some(s), Some(d), DragKind::Due, -6).is_err());
        // A diamond moves its one date and has no edges.
        let (ns, nd) = drag_dates(None, Some(d), DragKind::Move, 1).unwrap();
        assert!(ns.is_none() && day_ms(nd.unwrap()) == day(2026, 10, 31));
        assert!(drag_dates(None, Some(d), DragKind::Due, 1).is_err());
        assert_eq!(DragKind::parse("due"), Some(DragKind::Due));
        assert_eq!(DragKind::parse("x"), None);
    }
}
