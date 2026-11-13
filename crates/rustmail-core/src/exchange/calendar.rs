//! Calendars of an Exchange Online mailbox over Graph, sharing the CalDAV task
//! loop, the `calendars` / `events` tables and the event bus with the other
//! calendar providers (kind `exchange`). Local changes are pushed first
//! (create, patch, delete), then each enabled calendar is pulled with
//! `calendarView/delta`. Graph expands a recurring series into occurrences,
//! so each occurrence is stored as its own row and no RRULE is handled here;
//! times come in the zone the event was made in and are converted through
//! `exchange::zones` (all-day dates stay dates).

use super::client::{EventResponse, GraphMail, SeriesMaster};
use super::recurrence;
use crate::calendar::IcsEvent;
use crate::db::Db;
use crate::db::repo;
use crate::db::repo::calendar::SyncRow;
use crate::error::{CoreError, Result};
use crate::events::{CoreEvent, EventBus, NewEventInfo};
use crate::graphcal::{MAX_GRAPH_DELTA_PAGES, MAX_NEW_EVENT_NOTIFICATIONS, graph_event, ics_event};
use crate::models::now_ms;

/// List the mailbox's calendars and register them (and the account's
/// calendar sync) in the calendar database. Existing choices (enabled,
/// default) are kept.
pub(crate) async fn connect(
    db: &Db,
    client: &GraphMail,
    account_id: i64,
    graph_base: &str,
) -> Result<()> {
    let discovered = client.calendars().await?;
    if discovered.is_empty() {
        return Err(CoreError::CalDav("no calendars found".into()));
    }
    let cfg = repo::caldav::CaldavConfig {
        account_id,
        kind: "exchange".to_string(),
        base_url: graph_base.to_owned(),
        username: None,
        principal_url: None,
        home_set_url: None,
        enabled: true,
        last_error: None,
    };
    db.write(move |conn| {
        let tx = conn.transaction()?;
        repo::caldav::upsert_config(&tx, &cfg)?;
        let mut default_id = None;
        let mut first_id = None;
        for c in &discovered {
            let id = repo::caldav::upsert_calendar(
                &tx,
                account_id,
                &c.id,
                c.name.as_deref(),
                c.hex_color.as_deref(),
                !c.can_edit,
            )?;
            first_id.get_or_insert(id);
            if c.is_default {
                default_id.get_or_insert(id);
            }
        }
        if let Some(id) = default_id.or(first_id) {
            let has_default: i64 = tx.query_row(
                "SELECT COUNT(*) FROM calendars WHERE account_id = ?1 AND is_default = 1",
                rusqlite::params![account_id],
                |r| r.get(0),
            )?;
            if has_default == 0 {
                repo::caldav::set_default_calendar(&tx, account_id, id)?;
            }
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

/// One calendar cycle for the account. Returns whether anything changed.
pub(crate) async fn sync_account(
    db: &Db,
    bus: &EventBus,
    client: &GraphMail,
    account_id: i64,
    (past_ms, future_ms): (i64, i64),
) -> Result<bool> {
    let pushed = push_dirty(db, bus, client, account_id).await?;
    let calendars = db
        .read(move |conn| repo::caldav::list_calendars(conn, Some(account_id)))
        .await?;
    let mut changed_any = pushed;
    let mut new_events: Vec<NewEventInfo> = Vec::new();
    for cal in calendars.into_iter().filter(|c| c.enabled) {
        match sync_calendar(
            db,
            client,
            account_id,
            cal.id,
            &cal.url,
            (past_ms, future_ms),
        )
        .await
        {
            Ok((changed, fresh)) => {
                changed_any |= changed;
                let remaining = MAX_NEW_EVENT_NOTIFICATIONS.saturating_sub(new_events.len());
                new_events.extend(fresh.into_iter().take(remaining));
            }
            Err(error @ (CoreError::NeedsReauth | CoreError::Auth(_))) => return Err(error),
            Err(error) => {
                tracing::warn!("exchange calendar {} sync failed: {error}", cal.id);
                let message = error.to_string();
                db.write(move |conn| {
                    repo::caldav::set_config_error(conn, account_id, Some(&message))
                })
                .await?;
            }
        }
    }
    if changed_any {
        bus.emit(CoreEvent::CalendarUpdated { account_id });
    }
    if !new_events.is_empty() {
        bus.emit(CoreEvent::CalendarEventsAdded {
            account_id,
            events: new_events,
        });
    }
    Ok(changed_any)
}

async fn push_dirty(db: &Db, bus: &EventBus, client: &GraphMail, account_id: i64) -> Result<bool> {
    let rows = db
        .read(move |conn| repo::calendar::dirty_rows(conn, account_id))
        .await?;
    let mut any = false;
    for row in rows {
        match push_row(db, bus, client, account_id, &row).await {
            Ok(()) => any = true,
            Err(error @ (CoreError::NeedsReauth | CoreError::Auth(_))) => return Err(error),
            Err(error) => tracing::warn!("exchange push of event {} failed: {error}", row.event.id),
        }
    }
    Ok(any)
}

async fn push_row(
    db: &Db,
    bus: &EventBus,
    client: &GraphMail,
    account_id: i64,
    row: &SyncRow,
) -> Result<()> {
    let event_id = row.event.id;
    if row.deleted {
        if let Some(href) = &row.caldav_href {
            client.delete_event(href).await?;
        }
        db.write(move |conn| repo::calendar::hard_delete(conn, event_id))
            .await?;
        return Ok(());
    }
    let subject = row.event.summary.clone().unwrap_or_default();
    let mut event = graph_event(&subject, &row.event);
    // A series master carries its recurrence; a rule outside the mapped
    // subset is left as it is on the server.
    if let Some(rrule) = row.event.rrule.as_deref()
        && let Some(start) = chrono::DateTime::from_timestamp_millis(row.event.starts_at)
    {
        event.recurrence = recurrence::recurrence_from_rrule(rrule, start.date_naive());
    }
    let payload = crate::graph::event_payload(&event)?;
    match &row.caldav_href {
        Some(href) => {
            let calendar_id = row
                .event
                .calendar_id
                .ok_or_else(|| CoreError::CalDav("synced event without calendar".into()))?;
            // Changed on the server since the last pull: the server wins (its
            // version arrives with the next delta) and the local edit is
            // saved there as a second event, so neither is lost.
            if let (Some(stored), Some(current)) = (
                row.etag.as_deref(),
                client.event_version(href).await?.flatten(),
            ) && stored != current
            {
                let mut copy = payload.clone();
                copy["subject"] = format!("{subject} (your edit)").into();
                copy.as_object_mut()
                    .map(|object| object.remove("recurrence"));
                let target = db
                    .read(move |conn| repo::caldav::get_calendar(conn, calendar_id))
                    .await?
                    .map(|calendar| calendar.url);
                client.create_event(target.as_deref(), &copy).await?;
                bus.emit(CoreEvent::Notice {
                    account_id,
                    message: format!(
                        "\"{subject}\" was changed here and on Microsoft 365. The server's version was kept and your edit was saved as \"{subject} (your edit)\"."
                    ),
                });
                db.write(move |conn| {
                    conn.execute(
                        "UPDATE calendar_events SET dirty = 0 WHERE id = ?1",
                        [event_id],
                    )?;
                    Ok(())
                })
                .await?;
                return Ok(());
            }
            let version = client.update_event(href, &payload).await?;
            let href = href.clone();
            // A series keeps the text that lists its cancelled dates.
            let raw = row.ical_raw.clone().unwrap_or_default();
            db.write(move |conn| {
                repo::calendar::clear_dirty_set_etag(
                    conn,
                    event_id,
                    calendar_id,
                    &href,
                    version.as_deref(),
                    &raw,
                )
            })
            .await?;
        }
        None => {
            let mut target = None;
            if let Some(cal_id) = row.event.calendar_id {
                target = db
                    .read(move |conn| repo::caldav::get_calendar(conn, cal_id))
                    .await?
                    .filter(|c| c.enabled && !c.read_only);
            }
            let default = match target {
                Some(c) => c,
                None => db
                    .read(move |conn| repo::caldav::default_calendar(conn, account_id))
                    .await?
                    .ok_or_else(|| CoreError::CalDav("no calendar to write to".into()))?,
            };
            let id = client.create_event(Some(&default.url), &payload).await?;
            let calendar_id = default.id;
            db.write(move |conn| {
                repo::calendar::clear_dirty_set_etag(conn, event_id, calendar_id, &id, None, "")
            })
            .await?;
        }
    }
    Ok(())
}

async fn sync_calendar(
    db: &Db,
    client: &GraphMail,
    account_id: i64,
    calendar_id: i64,
    graph_calendar_id: &str,
    (past_ms, future_ms): (i64, i64),
) -> Result<(bool, Vec<NewEventInfo>)> {
    let (_, stored) = db
        .read(move |conn| repo::caldav::sync_state(conn, calendar_id))
        .await?;
    let mut incremental = stored.is_some();
    let now = now_ms();
    let window = (now - past_ms, now + future_ms);
    let mut link = stored;
    let mut restarted = false;
    let mut changed_any = false;
    let mut new_events: Vec<NewEventInfo> = Vec::new();
    let delta_link: Option<String>;
    let mut seen = std::collections::HashSet::new();
    let mut pages = 0usize;
    loop {
        pages += 1;
        if pages > MAX_GRAPH_DELTA_PAGES {
            return Err(CoreError::CalDav(
                "the calendar delta exceeded the page safety limit".into(),
            ));
        }
        if let Some(url) = &link
            && !seen.insert(url.clone())
        {
            return Err(CoreError::CalDav(
                "the calendar delta repeated a pagination link".into(),
            ));
        }
        let Some(page) = client
            .calendar_delta(graph_calendar_id, link.as_deref(), window)
            .await?
        else {
            if restarted {
                return Err(CoreError::CalDav(
                    "the calendar delta restarted twice".into(),
                ));
            }
            restarted = true;
            incremental = false;
            link = None;
            continue;
        };
        let (removed, upserts): (Vec<_>, Vec<_>) = page.events.into_iter().partition(|e| e.removed);
        // Series touched by this page: changed occurrences, and every stored
        // master when something was removed (a cancelled occurrence arrives
        // as a bare removal that names no series).
        let mut wanted: Vec<String> = Vec::new();
        for ev in &upserts {
            if let Some(master) = &ev.series_master_id
                && !wanted.contains(master)
            {
                wanted.push(master.clone());
            }
        }
        if !removed.is_empty() {
            let known = db
                .read(move |conn| {
                    repo::calendar::series_master_hrefs(conn, calendar_id, MAX_SERIES_PER_CYCLE)
                })
                .await?;
            for href in known {
                if !wanted.contains(&href) && !removed.iter().any(|gone| gone.id == href) {
                    wanted.push(href);
                }
            }
        }
        wanted.truncate(MAX_SERIES_PER_CYCLE);
        let mut series: std::collections::HashMap<String, Option<SeriesRow>> =
            std::collections::HashMap::new();
        for id in wanted {
            let row = match client.series_master(&id).await? {
                Some(master) => series_row(&master),
                None => None,
            };
            series.insert(id, row);
        }
        if !upserts.is_empty() || series.values().any(Option::is_some) {
            changed_any = true;
            let batch_new = db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    let mut fresh: Vec<NewEventInfo> = Vec::new();
                    for (id, row) in &series {
                        match row {
                            Some(row) => {
                                repo::calendar::delete_series_occurrences(&tx, calendar_id, id)?;
                                repo::calendar::upsert_remote(
                                    &tx,
                                    account_id,
                                    calendar_id,
                                    id,
                                    row.etag.as_deref().unwrap_or(""),
                                    &row.raw,
                                    &row.ics,
                                )?;
                            }
                            // The rule no longer maps: go back to occurrences.
                            None if repo::calendar::has_series_master(&tx, calendar_id, id)? => {
                                repo::calendar::delete_by_href(&tx, calendar_id, id)?;
                            }
                            None => {}
                        }
                    }
                    for ev in &upserts {
                        if ev.start_ms == 0 || ev.event_type.as_deref() == Some("seriesMaster") {
                            continue;
                        }
                        if let Some(master) = &ev.series_master_id
                            && matches!(series.get(master), Some(Some(_)))
                        {
                            continue; // shown through the master's rule
                        }
                        let ics = ics_event(ev);
                        let inserted = repo::calendar::upsert_remote(
                            &tx,
                            account_id,
                            calendar_id,
                            &ev.id,
                            ev.etag.as_deref().unwrap_or(""),
                            "",
                            &ics,
                        )?;
                        if inserted && fresh.len() < MAX_NEW_EVENT_NOTIFICATIONS {
                            fresh.push(NewEventInfo {
                                summary: ics.summary.clone(),
                                starts_at: ics.starts_at_ms,
                                all_day: ics.all_day,
                            });
                        }
                        if let Some(master) = &ev.series_master_id {
                            repo::calendar::set_series_master(&tx, calendar_id, &ev.id, master)?;
                        }
                    }
                    tx.commit()?;
                    Ok(fresh)
                })
                .await?;
            let remaining = MAX_NEW_EVENT_NOTIFICATIONS.saturating_sub(new_events.len());
            new_events.extend(batch_new.into_iter().take(remaining));
        }
        if !removed.is_empty() {
            changed_any = true;
            db.write(move |conn| {
                let tx = conn.transaction()?;
                for ev in &removed {
                    repo::calendar::delete_by_href(&tx, calendar_id, &ev.id)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        }
        match page.next_link {
            Some(next) => link = Some(next),
            None => {
                delta_link = page.delta_link;
                break;
            }
        }
    }
    if !incremental {
        new_events.clear();
    }
    let synced_at = now_ms();
    db.write(move |conn| {
        repo::caldav::set_sync_state(conn, calendar_id, None, delta_link.as_deref(), synced_at)
    })
    .await?;
    Ok((changed_any, new_events))
}

/// How many series masters one cycle reads.
const MAX_SERIES_PER_CYCLE: usize = 50;

/// A series master ready to store.
struct SeriesRow {
    ics: IcsEvent,
    raw: String,
    etag: Option<String>,
}

fn ics_time(ms: i64, all_day: bool) -> String {
    let Some(date) = chrono::DateTime::from_timestamp_millis(ms) else {
        return String::new();
    };
    if all_day {
        date.format("%Y%m%d").to_string()
    } else {
        date.format("%Y%m%dT%H%M%SZ").to_string()
    }
}

/// Turn a Graph series master into a master row with an RRULE, or `None`
/// when its rule does not map (the occurrences stay expanded).
fn series_row(master: &SeriesMaster) -> Option<SeriesRow> {
    let event = &master.event;
    if event.start_ms == 0 || event.cancelled {
        return None;
    }
    let (local_text, zone) = master.start_local.as_ref()?;
    let local = chrono::NaiveDateTime::parse_from_str(local_text, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    let rrule = recurrence::rrule_from_recurrence(&master.recurrence, local.date())?;
    let all_day = event.all_day;
    // A cancelled date happens at the series' own time of day.
    let at_start_time = |date: chrono::NaiveDate| -> Option<i64> {
        let moment = date.and_time(local.time());
        if all_day {
            return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
        }
        crate::graph::parse_graph_time(
            Some(&serde_json::json!({
                "dateTime": moment.format("%Y-%m-%dT%H:%M:%S").to_string(),
                "timeZone": zone,
            })),
            false,
        )
    };
    let mut exdates: Vec<i64> = recurrence::cancelled_dates(&master.cancelled)
        .into_iter()
        .filter_map(at_start_time)
        .collect();
    exdates.extend(
        master
            .exceptions
            .iter()
            .filter(|e| e.cancelled)
            .map(|e| e.original_start_ms),
    );
    exdates.sort_unstable();
    exdates.dedup();
    let mut ics = ics_event(event);
    ics.rrule = Some(rrule.clone());
    // The series repeats at this wall-clock time in its own zone.
    ics.tzid = Some(zone.clone());
    let uid = &ics.uid;
    let mut raw = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n");
    raw.push_str(&format!("UID:{uid}\r\n"));
    raw.push_str(&format!(
        "DTSTART:{}\r\n",
        ics_time(event.start_ms, all_day)
    ));
    raw.push_str(&format!("RRULE:{rrule}\r\n"));
    if !exdates.is_empty() {
        let list: Vec<String> = exdates.iter().map(|ms| ics_time(*ms, all_day)).collect();
        raw.push_str(&format!("EXDATE:{}\r\n", list.join(",")));
    }
    raw.push_str("END:VEVENT\r\n");
    for moved in master
        .exceptions
        .iter()
        .filter(|e| !e.cancelled && e.start_ms != 0)
    {
        let end = moved.end_ms.unwrap_or(moved.start_ms);
        raw.push_str("BEGIN:VEVENT\r\n");
        raw.push_str(&format!("UID:{uid}\r\n"));
        raw.push_str(&format!(
            "RECURRENCE-ID:{}\r\n",
            ics_time(moved.original_start_ms, all_day)
        ));
        raw.push_str(&format!(
            "DTSTART:{}\r\n",
            ics_time(moved.start_ms, all_day)
        ));
        raw.push_str(&format!("DTEND:{}\r\n", ics_time(end, all_day)));
        raw.push_str("END:VEVENT\r\n");
    }
    raw.push_str("END:VCALENDAR\r\n");
    Some(SeriesRow {
        ics,
        raw,
        etag: event.etag.clone(),
    })
}

/// How far from the named start an occurrence may begin and still be the
/// one meant (a minute: the stored start was converted from a zone).
const OCCURRENCE_SLACK_MS: i64 = 60_000;

/// What to change about one occurrence of a series.
#[derive(Debug, Clone, Default)]
pub(crate) struct OccurrenceChange {
    pub(crate) summary: Option<String>,
    pub(crate) starts_at: Option<i64>,
    pub(crate) ends_at: Option<i64>,
}

/// The Graph id of the occurrence that starts at `occurrence_start_ms`.
async fn occurrence_id(
    client: &GraphMail,
    series_id: &str,
    occurrence_start_ms: i64,
) -> Result<String> {
    let window = (
        occurrence_start_ms - 86_400_000,
        occurrence_start_ms + 86_400_000,
    );
    client
        .series_instances(series_id, window)
        .await?
        .into_iter()
        .find(|(_, start, _)| (start - occurrence_start_ms).abs() <= OCCURRENCE_SLACK_MS)
        .map(|(id, _, _)| id)
        .ok_or_else(|| CoreError::NotFound("occurrence".into()))
}

/// The series' Graph id of a stored master row.
async fn series_id(db: &Db, event_id: i64) -> Result<(SyncRow, String)> {
    let row = db
        .read(move |conn| repo::calendar::sync_row_for(conn, event_id))
        .await?
        .ok_or_else(|| CoreError::NotFound("event".into()))?;
    let id = match (&row.caldav_href, &row.event.rrule) {
        (Some(href), Some(_)) => href.clone(),
        _ => {
            return Err(CoreError::CalDav(
                "this event is not a series on Microsoft 365".into(),
            ));
        }
    };
    Ok((row, id))
}

/// Cancel one occurrence of a series on the server (the attendees are told,
/// as Outlook does).
pub(crate) async fn cancel_occurrence(
    db: &Db,
    client: &GraphMail,
    event_id: i64,
    occurrence_start_ms: i64,
) -> Result<()> {
    let (_, series) = series_id(db, event_id).await?;
    let occurrence = occurrence_id(client, &series, occurrence_start_ms).await?;
    client.delete_event(&occurrence).await
}

/// Change one occurrence of a series on the server (an exception).
pub(crate) async fn edit_occurrence(
    db: &Db,
    client: &GraphMail,
    event_id: i64,
    occurrence_start_ms: i64,
    change: &OccurrenceChange,
) -> Result<()> {
    let (row, series) = series_id(db, event_id).await?;
    let occurrence = occurrence_id(client, &series, occurrence_start_ms).await?;
    let mut payload = serde_json::Map::new();
    if let Some(summary) = &change.summary {
        payload.insert("subject".into(), summary.clone().into());
    }
    let all_day = row.event.all_day;
    let moment = |ms: i64| -> Result<serde_json::Value> {
        let date = chrono::DateTime::from_timestamp_millis(ms)
            .ok_or_else(|| CoreError::Other("invalid timestamp".into()))?;
        let text = if all_day {
            date.format("%Y-%m-%dT00:00:00")
        } else {
            date.format("%Y-%m-%dT%H:%M:%S")
        };
        Ok(serde_json::json!({ "dateTime": text.to_string(), "timeZone": "UTC" }))
    };
    if let Some(start) = change.starts_at {
        payload.insert("start".into(), moment(start)?);
    }
    if let Some(end) = change.ends_at {
        payload.insert("end".into(), moment(end)?);
    }
    if payload.is_empty() {
        return Ok(());
    }
    client
        .update_event(&occurrence, &serde_json::Value::Object(payload))
        .await
        .map(|_| ())
}

/// Free/busy of mailboxes (an optional meeting-planning lookup; the Calendar
/// view has no place for it yet, so nothing in the UI calls it).
pub(crate) async fn free_busy(
    client: &GraphMail,
    addresses: &[String],
    window_ms: (i64, i64),
) -> Result<Vec<(String, Vec<super::client::BusySlot>)>> {
    client.free_busy(addresses, window_ms).await
}

/// Answer a meeting invitation on the server and remember the answer.
pub(crate) async fn respond(
    db: &Db,
    client: &GraphMail,
    event_id: i64,
    response: EventResponse,
    comment: &str,
) -> Result<()> {
    let row = db
        .read(move |conn| repo::calendar::sync_row_for(conn, event_id))
        .await?
        .ok_or_else(|| CoreError::NotFound("event".into()))?;
    let href = row
        .caldav_href
        .ok_or_else(|| CoreError::CalDav("this event is not on Microsoft 365".into()))?;
    client.respond_to_event(&href, response, comment).await?;
    let status = match response {
        EventResponse::Accept => "ACCEPTED",
        EventResponse::Tentative => "TENTATIVE",
        EventResponse::Decline => "DECLINED",
    };
    db.write(move |conn| repo::calendar::set_rsvp(conn, event_id, status))
        .await
}
