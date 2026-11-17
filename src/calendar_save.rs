//! Saving the calendar editor: check the form, then create or change the event.
//!
//! The window callback only reads the form and shows the outcome; everything
//! that decides what is stored lives here so it can be tested without a window.

use crate::{
    UiMessage,
    calendar::{LocalCalendarEvent, RepeatForm, rrule_from_repeat_form},
    mail::CoreMailSource,
};
use chrono::{Duration as ChronoDuration, Local, NaiveDate, NaiveTime, TimeZone};
use rustmail_core::models::{CreateEventArgs, UpdateEventArgs};

/// What the editor shows, as typed.
pub(crate) struct EventForm {
    pub(crate) title: String,
    pub(crate) date: String,
    pub(crate) time: String,
    pub(crate) duration: String,
    /// Minutes before the start; negative keeps the app's default lead time.
    pub(crate) reminder_minutes: i32,
    pub(crate) repeat: RepeatForm,
    /// A series is being edited and the person chose "this occurrence only".
    pub(crate) this_occurrence_only: bool,
    /// Account a new event belongs to (0 = local only).
    pub(crate) new_event_account_id: i64,
}

pub(crate) struct SavedEvent {
    pub(crate) date: NaiveDate,
    /// The event is stored, but its reminder or repeat could not be set.
    pub(crate) options_error: Option<String>,
}

/// Creates a new event, or changes `editing` when it is a local one.
pub(crate) fn save_event(
    core: &CoreMailSource,
    runtime: &tokio::runtime::Runtime,
    form: &EventForm,
    editing: Option<LocalCalendarEvent>,
) -> Result<SavedEvent, UiMessage> {
    let parsed = NaiveDate::parse_from_str(form.date.trim(), "%Y-%m-%d").and_then(|date| {
        NaiveTime::parse_from_str(form.time.trim(), "%H:%M").map(|time| (date, time))
    });
    let Ok((date, time)) = parsed else {
        return Err(UiMessage::plain(
            "Use YYYY-MM-DD and a 24-hour time such as 09:30.",
        ));
    };
    let Ok(duration) = form.duration.trim().parse::<i32>() else {
        return Err(UiMessage::plain(
            "Event duration must be a number of minutes.",
        ));
    };
    if form.title.trim().is_empty() || !(15..=1440).contains(&duration) {
        return Err(UiMessage::plain(
            "Add a title and choose a duration from 15 to 1440 minutes.",
        ));
    }
    let Some(start) = Local.from_local_datetime(&date.and_time(time)).earliest() else {
        return Err(UiMessage::plain(
            "That local time does not exist in the current time zone.",
        ));
    };
    let starts_at = start.timestamp_millis();
    let ends_at = (start + ChronoDuration::minutes(i64::from(duration))).timestamp_millis();
    let title = form.title.trim().to_owned();

    // Reminder and repeat, checked before anything is saved.
    let reminder = match form.reminder_minutes {
        minutes if minutes < 0 => None,
        minutes => Some(i64::from(minutes)),
    };
    let wanted_rule = match rrule_from_repeat_form(&form.repeat, date) {
        Ok(rule) => Some(rule),
        // A rule the editor cannot show stays as it is.
        Err("custom") => None,
        Err(reason) => {
            return Err(match reason {
                "The repeat interval must be a number from 1 to 999." => UiMessage::plain(
                    "The repeat interval must be a number from 1 to 999.",
                ),
                "The number of events must be from 1 to 999." => {
                    UiMessage::plain("The number of events must be from 1 to 999.")
                }
                "Use YYYY-MM-DD for the last day of the repeat." => {
                    UiMessage::plain("Use YYYY-MM-DD for the last day of the repeat.")
                }
                _ => UiMessage::plain("The repeat must not end before the event starts."),
            });
        }
    };

    let only_this_occurrence = form.this_occurrence_only && editing.is_some();
    let result = if let Some(event) = editing.clone() {
        if only_this_occurrence {
            runtime
                .block_on(core.file_core().exchange_edit_occurrence(
                    i64::from(event.id),
                    event.occurrence_start,
                    Some(title),
                    Some(starts_at),
                    Some(ends_at),
                ))
                .map(|()| i64::from(event.id))
                .map_err(|error| error.to_string())
        } else {
            // A series is moved as a whole: the change the person made to
            // this occurrence is applied to the series' own first start.
            let (series_start, series_end) = if event.recurrence.is_empty() {
                (starts_at, ends_at)
            } else {
                let first = runtime
                    .block_on(core.file_core().calendar_event(i64::from(event.id)))
                    .ok()
                    .flatten()
                    .map_or(event.occurrence_start, |master| master.starts_at);
                let shifted = first + (starts_at - event.occurrence_start);
                (shifted, shifted + (ends_at - starts_at))
            };
            runtime
                .block_on(core.update_event(UpdateEventArgs {
                    event_id: i64::from(event.id),
                    summary: title,
                    description: (!event.description.is_empty()).then_some(event.description),
                    location: (!event.location.is_empty()).then_some(event.location),
                    join_url: (!event.join_url.is_empty()).then_some(event.join_url),
                    starts_at: series_start,
                    ends_at: series_end,
                    all_day: event.all_day,
                    attendees: event.attendee_addresses,
                    notify: false,
                }))
                .map(|updated| updated.id)
        }
    } else {
        runtime
            .block_on(core.create_event(CreateEventArgs {
                account_id: form.new_event_account_id,
                calendar_id: None,
                summary: title,
                description: None,
                location: None,
                join_url: None,
                starts_at,
                ends_at,
                all_day: false,
                attendees: Vec::new(),
            }))
            .map(|created| created.id)
    };
    let id = result.map_err(|error| UiMessage::detail("Could not save event: {}", error))?;

    // The reminder and the repeat rule apply to the event itself, not to one
    // occurrence of it.
    let mut options_error = None;
    if !only_this_occurrence {
        let original_reminder = editing.as_ref().and_then(|event| event.reminder_minutes);
        if reminder != original_reminder
            && let Err(error) =
                runtime.block_on(core.file_core().set_event_reminder(id, reminder))
        {
            options_error = Some(error.to_string());
        }
        let original_rule = editing
            .as_ref()
            .map(|event| event.recurrence.as_str())
            .filter(|rule| !rule.is_empty())
            .map(str::to_owned);
        if let Some(rule) = wanted_rule
            && rule != original_rule
            && let Err(error) =
                runtime.block_on(core.file_core().set_event_recurrence(id, rule))
        {
            options_error = Some(error.to_string());
        }
    }
    Ok(SavedEvent {
        date,
        options_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::core_calendar_event;
    use rustmail_core::{accounts::secrets::DevelopmentFileCredentialStore, config::Paths};
    use std::sync::Arc;

    struct Fixture {
        _dir: tempfile::TempDir,
        runtime: tokio::runtime::Runtime,
        core: CoreMailSource,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let core = runtime
            .block_on(CoreMailSource::start(
                Paths::for_tests(dir.path()),
                Arc::new(DevelopmentFileCredentialStore::new(
                    dir.path().join("credentials.json"),
                )),
                Arc::new(rustmail_core::oauth::redirect::LoopbackRedirectBroker::default()),
            ))
            .unwrap();
        Fixture {
            _dir: dir,
            runtime,
            core,
        }
    }

    fn form(title: &str) -> EventForm {
        EventForm {
            title: title.into(),
            date: "2030-03-06".into(),
            time: "09:30".into(),
            duration: "60".into(),
            reminder_minutes: -1,
            repeat: RepeatForm::default(),
            this_occurrence_only: false,
            new_event_account_id: 0,
        }
    }

    fn stored(fixture: &Fixture) -> Vec<rustmail_core::models::CalendarEvent> {
        let day = |text: &str| {
            let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap();
            Local
                .from_local_datetime(&date.and_time(NaiveTime::MIN))
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        fixture
            .runtime
            .block_on(
                fixture
                    .core
                    .load_events(day("2030-03-01"), day("2030-04-30")),
            )
            .unwrap()
    }

    fn occurrence(fixture: &Fixture, index: usize) -> LocalCalendarEvent {
        core_calendar_event(stored(fixture).remove(index))
    }

    #[test]
    fn a_new_event_is_stored_with_its_start_and_length() {
        let fixture = fixture();
        let saved = save_event(&fixture.core, &fixture.runtime, &form("  Planning "), None)
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        assert_eq!(saved.date, NaiveDate::from_ymd_opt(2030, 3, 6).unwrap());
        assert!(saved.options_error.is_none());
        let events = stored(&fixture);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.summary.as_deref(), Some("Planning"));
        assert_eq!(event.ends_at.unwrap() - event.starts_at, 3_600_000);
        assert!(event.is_local);
        assert_eq!(event.rrule, None);
        assert_eq!(event.reminder_minutes, None);
    }

    #[test]
    fn a_new_event_can_repeat_and_have_a_reminder() {
        let fixture = fixture();
        let mut weekly = form("Standup");
        weekly.reminder_minutes = 10;
        weekly.repeat = RepeatForm {
            kind: "weekly".into(),
            end_kind: "count".into(),
            end_value: "3".into(),
            ..RepeatForm::default()
        };
        let saved = save_event(&fixture.core, &fixture.runtime, &weekly, None)
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        assert!(saved.options_error.is_none());
        let events = stored(&fixture);
        assert_eq!(events.len(), 3, "three weekly occurrences in the window");
        let rule = events[0].rrule.clone().unwrap();
        assert!(rule.contains("FREQ=WEEKLY") && rule.contains("COUNT=3"), "{rule}");
        assert_eq!(events[0].reminder_minutes, Some(10));
        // A week later on the wall clock, whatever the time zone does between.
        let (first, second) = (occurrence(&fixture, 0), occurrence(&fixture, 1));
        assert_eq!(second.date, first.date + ChronoDuration::days(7));
        assert_eq!(second.start_minutes, first.start_minutes);
    }

    #[test]
    fn editing_keeps_what_the_form_does_not_show() {
        let fixture = fixture();
        // Inviting people needs an account to send from.
        fixture
            .runtime
            .block_on(fixture.core.file_core().db.write(|conn| {
                conn.execute(
                    "INSERT INTO accounts (id,email,provider,auth_kind,username,imap_host,imap_port,smtp_host,smtp_port,created_at)
                     VALUES (1,'me@example.test','imap','password','me','imap.example.test',993,'smtp.example.test',465,0)",
                    [],
                )?;
                Ok(())
            }))
            .unwrap();
        let created = fixture
            .runtime
            .block_on(fixture.core.create_event(CreateEventArgs {
                account_id: 1,
                calendar_id: None,
                summary: "Review".into(),
                description: Some("Agenda".into()),
                location: Some("Room 4".into()),
                join_url: Some("https://meet.example.test/x".into()),
                starts_at: 1_898_000_000_000,
                ends_at: 1_898_003_600_000,
                all_day: false,
                attendees: vec![rustmail_core::models::Address {
                    name: Some("Ana".into()),
                    email: "ana@example.test".into(),
                }],
            }))
            .unwrap();
        let editing = core_calendar_event(created);
        let mut changed = form("Review (moved)");
        changed.date = "2030-03-07".into();
        changed.time = "14:00".into();
        changed.duration = "90".into();
        save_event(&fixture.core, &fixture.runtime, &changed, Some(editing))
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        let events = stored(&fixture);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.summary.as_deref(), Some("Review (moved)"));
        assert_eq!(event.ends_at.unwrap() - event.starts_at, 90 * 60_000);
        assert_eq!(event.description.as_deref(), Some("Agenda"));
        assert_eq!(event.location.as_deref(), Some("Room 4"));
        assert_eq!(event.join_url.as_deref(), Some("https://meet.example.test/x"));
        assert_eq!(event.attendees.len(), 1);
        assert_eq!(event.attendees[0].email, "ana@example.test");
    }

    #[test]
    fn a_series_is_moved_as_a_whole_and_can_stop_repeating() {
        let fixture = fixture();
        let mut weekly = form("Standup");
        weekly.repeat = RepeatForm {
            kind: "weekly".into(),
            end_kind: "count".into(),
            end_value: "4".into(),
            ..RepeatForm::default()
        };
        save_event(&fixture.core, &fixture.runtime, &weekly, None)
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        let first_before = stored(&fixture)[0].starts_at;
        // The person edits the second occurrence and starts it an hour later.
        let second = occurrence(&fixture, 1);
        let mut moved = form("Standup");
        moved.date = "2030-03-13".into();
        moved.time = "10:30".into();
        moved.repeat = repeat_of(&second);
        save_event(&fixture.core, &fixture.runtime, &moved, Some(second))
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        let events = stored(&fixture);
        assert_eq!(events.len(), 4, "the series keeps its four occurrences");
        assert_eq!(events[0].starts_at, first_before + 3_600_000);

        // Setting the repeat to "none" turns the series into a single event.
        let editing = occurrence(&fixture, 0);
        let mut single = form("Standup");
        single.date = "2030-03-06".into();
        single.time = "10:30".into();
        save_event(&fixture.core, &fixture.runtime, &single, Some(editing))
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        let events = stored(&fixture);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rrule, None);
    }

    fn repeat_of(event: &LocalCalendarEvent) -> RepeatForm {
        crate::calendar::repeat_form_from_rrule(&event.recurrence)
    }

    #[test]
    fn a_reminder_changes_and_clears_with_the_form() {
        let fixture = fixture();
        let mut with_reminder = form("Call");
        with_reminder.reminder_minutes = 30;
        save_event(&fixture.core, &fixture.runtime, &with_reminder, None)
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        assert_eq!(stored(&fixture)[0].reminder_minutes, Some(30));

        let mut none = form("Call");
        none.reminder_minutes = 0;
        save_event(&fixture.core, &fixture.runtime, &none, Some(occurrence(&fixture, 0)))
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        assert_eq!(stored(&fixture)[0].reminder_minutes, Some(0), "no reminder");

        let mut default = form("Call");
        default.reminder_minutes = -1;
        save_event(&fixture.core, &fixture.runtime, &default, Some(occurrence(&fixture, 0)))
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        assert_eq!(stored(&fixture)[0].reminder_minutes, None);
    }

    #[test]
    fn a_bad_form_is_refused_and_nothing_is_stored() {
        let fixture = fixture();
        let cases: Vec<(EventForm, UiMessage)> = vec![
            (
                EventForm {
                    date: "06/03/2030".into(),
                    ..form("x")
                },
                UiMessage::plain(
                    "Use YYYY-MM-DD and a 24-hour time such as 09:30.",
                ),
            ),
            (
                EventForm {
                    time: "9h".into(),
                    ..form("x")
                },
                UiMessage::plain(
                    "Use YYYY-MM-DD and a 24-hour time such as 09:30.",
                ),
            ),
            (
                EventForm {
                    duration: "soon".into(),
                    ..form("x")
                },
                UiMessage::plain(
                    "Event duration must be a number of minutes.",
                ),
            ),
            (
                EventForm {
                    duration: "5".into(),
                    ..form("x")
                },
                UiMessage::plain(
                    "Add a title and choose a duration from 15 to 1440 minutes.",
                ),
            ),
            (
                form("   "),
                UiMessage::plain(
                    "Add a title and choose a duration from 15 to 1440 minutes.",
                ),
            ),
            (
                EventForm {
                    repeat: RepeatForm {
                        kind: "daily".into(),
                        interval: "0".into(),
                        ..RepeatForm::default()
                    },
                    ..form("x")
                },
                UiMessage::plain(
                    "The repeat interval must be a number from 1 to 999.",
                ),
            ),
            (
                EventForm {
                    repeat: RepeatForm {
                        kind: "daily".into(),
                        end_kind: "until".into(),
                        end_value: "2030-03-01".into(),
                        ..RepeatForm::default()
                    },
                    ..form("x")
                },
                UiMessage::plain(
                    "The repeat must not end before the event starts.",
                ),
            ),
        ];
        for (bad, expected) in cases {
            let error = save_event(&fixture.core, &fixture.runtime, &bad, None)
                .err()
                .unwrap_or_else(|| panic!("{expected:?} was accepted"));
            assert_eq!(error, expected);
        }
        assert!(stored(&fixture).is_empty());
    }

    #[test]
    fn one_occurrence_of_a_local_series_cannot_be_changed_without_a_server() {
        let fixture = fixture();
        let mut weekly = form("Standup");
        weekly.repeat = RepeatForm {
            kind: "weekly".into(),
            end_kind: "count".into(),
            end_value: "3".into(),
            ..RepeatForm::default()
        };
        save_event(&fixture.core, &fixture.runtime, &weekly, None)
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        let before: Vec<_> = stored(&fixture).iter().map(|event| event.starts_at).collect();
        let second = occurrence(&fixture, 1);
        let mut only_this = form("Standup (special)");
        only_this.date = "2030-03-13".into();
        only_this.this_occurrence_only = true;
        only_this.repeat = repeat_of(&second);
        let error = save_event(&fixture.core, &fixture.runtime, &only_this, Some(second))
            .err()
            .expect("an exception needs a Microsoft 365 account");
        assert!(format!("{error:?}").contains("Could not save event"), "{error:?}");
        let after = stored(&fixture);
        assert_eq!(
            after.iter().map(|event| event.starts_at).collect::<Vec<_>>(),
            before
        );
        assert!(after.iter().all(|event| event.summary.as_deref() == Some("Standup")));
    }
}
