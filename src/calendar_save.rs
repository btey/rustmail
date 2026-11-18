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
use rustmail_core::models::{Address, CreateEventArgs, UpdateEventArgs};
use std::collections::HashSet;

/// Most people one event can invite from the editor.
const MAX_ATTENDEES: usize = 50;

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
    /// Who is invited, as typed: addresses separated by commas, semicolons or
    /// lines, each optionally written `Name <address>`.
    pub(crate) attendees: String,
}

/// The start and end of the event the form describes, in milliseconds, when
/// its date, time and duration can be read.
pub(crate) fn event_window(date: &str, time: &str, duration: &str) -> Option<(i64, i64)> {
    let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let time = NaiveTime::parse_from_str(time.trim(), "%H:%M").ok()?;
    let minutes = duration.trim().parse::<i64>().ok().filter(|m| (15..=1440).contains(m))?;
    let start = Local.from_local_datetime(&date.and_time(time)).earliest()?;
    Some((
        start.timestamp_millis(),
        (start + ChronoDuration::minutes(minutes)).timestamp_millis(),
    ))
}

/// Reads the attendee field. Blank entries and repeats of an address are
/// dropped; the first spelling of a name wins.
pub(crate) fn parse_attendees(text: &str) -> Result<Vec<Address>, UiMessage> {
    let mut seen = HashSet::new();
    let mut people = Vec::new();
    for entry in text.split([',', ';', '\n']) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (name, email) = match (entry.find('<'), entry.rfind('>')) {
            (Some(open), Some(close)) if open < close && close == entry.len() - 1 => (
                Some(entry[..open].trim().trim_matches('"').trim().to_owned())
                    .filter(|name| !name.is_empty()),
                entry[open + 1..close].trim(),
            ),
            _ => (None, entry),
        };
        if !plausible_address(email) {
            return Err(UiMessage::detail(
                "Check this attendee address: {}",
                entry.to_owned(),
            ));
        }
        if seen.insert(email.to_ascii_lowercase()) {
            people.push(Address {
                name,
                email: email.to_owned(),
            });
        }
    }
    if people.len() > MAX_ATTENDEES {
        return Err(UiMessage::plain("Invite at most 50 people to one event."));
    }
    Ok(people)
}

/// One `@`, something before it, a dotted domain after it, no spaces.
fn plausible_address(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(|c| c.is_whitespace() || c == '<' || c == '>')
}

/// The attendee field's text for an event being edited.
pub(crate) fn attendee_field_text(people: &[Address]) -> String {
    people
        .iter()
        .map(|person| match person.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => format!("{name} <{}>", person.email),
            _ => person.email.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
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
    let wanted_attendees = parse_attendees(&form.attendees)?;
    let account_known = editing
        .as_ref()
        .map_or(form.new_event_account_id, |event| event.account_id)
        != 0;
    if !wanted_attendees.is_empty() && !account_known {
        return Err(UiMessage::plain(
            "Connect a mail account before inviting attendees.",
        ));
    }
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
            // Names already known are kept; invitations go out only when the
            // list of addresses changed.
            let emails = |people: &[Address]| {
                people
                    .iter()
                    .map(|person| person.email.to_ascii_lowercase())
                    .collect::<HashSet<_>>()
            };
            let attendees_changed = emails(&wanted_attendees) != emails(&event.attendee_addresses);
            let attendees = wanted_attendees
                .into_iter()
                .map(|person| {
                    let known = event
                        .attendee_addresses
                        .iter()
                        .find(|old| old.email.eq_ignore_ascii_case(&person.email));
                    Address {
                        name: person.name.or_else(|| known.and_then(|old| old.name.clone())),
                        email: person.email,
                    }
                })
                .collect();
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
                    attendees,
                    notify: attendees_changed,
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
                attendees: wanted_attendees,
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
            attendees: String::new(),
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
        // The editor shows the invited people; leaving the field alone keeps them.
        changed.attendees = attendee_field_text(&editing.attendee_addresses);
        assert_eq!(changed.attendees, "Ana <ana@example.test>");
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
    fn attendees_can_be_added_and_removed_when_editing() {
        let fixture = fixture();
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
        let mut invited = form("Review");
        invited.new_event_account_id = 1;
        invited.attendees = "Ana <ana@example.test>; ben@example.test".into();
        save_event(&fixture.core, &fixture.runtime, &invited, None)
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        let emails = |fixture: &Fixture| {
            stored(fixture)[0]
                .attendees
                .iter()
                .map(|person| (person.email.clone(), person.name.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            emails(&fixture),
            vec![
                ("ana@example.test".to_owned(), Some("Ana".to_owned())),
                ("ben@example.test".to_owned(), None)
            ]
        );
        // Another person joins; a name already known is kept when only the address is typed.
        let mut edit = form("Review");
        edit.date = "2030-03-06".into();
        edit.attendees = "ana@example.test, ben@example.test, Cy <cy@example.test>".into();
        save_event(&fixture.core, &fixture.runtime, &edit, Some(occurrence(&fixture, 0)))
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        let after = emails(&fixture);
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], ("ana@example.test".to_owned(), Some("Ana".to_owned())));
        assert_eq!(after[2], ("cy@example.test".to_owned(), Some("Cy".to_owned())));
        // An empty field removes everyone.
        let mut cleared = form("Review");
        cleared.date = "2030-03-06".into();
        save_event(&fixture.core, &fixture.runtime, &cleared, Some(occurrence(&fixture, 0)))
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        assert!(stored(&fixture)[0].attendees.is_empty());
    }

    #[test]
    fn the_availability_window_needs_a_readable_date_time_and_length() {
        let (start, end) = event_window("2030-03-06", "09:30", "45").unwrap();
        assert_eq!(end - start, 45 * 60_000);
        assert!(event_window("2030-03-06", "9h", "45").is_none());
        assert!(event_window("soon", "09:30", "45").is_none());
        assert!(event_window("2030-03-06", "09:30", "5").is_none());
        assert!(event_window("2030-03-06", "09:30", "").is_none());
    }

    #[test]
    fn a_bad_attendee_list_stores_nothing() {
        let fixture = fixture();
        for text in ["not-an-address", "ana@example", "a b@example.test", "ana@@example.test"] {
            let mut bad = form("Review");
            bad.new_event_account_id = 1;
            bad.attendees = text.into();
            assert!(
                save_event(&fixture.core, &fixture.runtime, &bad, None).is_err(),
                "{text}"
            );
        }
        // Inviting needs an account to send from.
        let mut local = form("Review");
        local.attendees = "ana@example.test".into();
        assert!(save_event(&fixture.core, &fixture.runtime, &local, None).is_err());
        assert!(stored(&fixture).is_empty());
    }

    #[test]
    fn the_attendee_field_reads_names_separators_and_repeats() {
        let people = parse_attendees(
            " Ana Diaz <ana@example.test>,\nben@example.test ; \"Cy, Jr\" <cy@example.test>;ANA@example.test,, ",
        );
        // A comma inside quotes splits the entry, so that spelling is refused
        // instead of guessing.
        assert!(people.is_err());
        let people = parse_attendees(" Ana Diaz <ana@example.test>,\nben@example.test ; ANA@example.test,, ")
            .map_err(|message| format!("{message:?}"))
            .unwrap();
        assert_eq!(people.len(), 2, "a repeated address is dropped");
        assert_eq!(people[0].name.as_deref(), Some("Ana Diaz"));
        assert_eq!(people[0].email, "ana@example.test");
        assert_eq!(people[1].name, None);
        assert_eq!(attendee_field_text(&people), "Ana Diaz <ana@example.test>, ben@example.test");
        assert!(parse_attendees("  ,; ").unwrap().is_empty());
        let many = (0..51)
            .map(|n| format!("p{n}@example.test"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_attendees(&many).is_err());
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
