//! Calendars of an Exchange Online mailbox over Graph, sharing the CalDAV task
//! loop, the `calendars` / `events` tables and the event bus with the other
//! calendar providers (kind `exchange`). Local changes are pushed first
//! (create, patch, delete), then each enabled calendar is pulled with
//! `calendarView/delta`. Graph expands a recurring series into occurrences,
//! so each occurrence is stored as its own row and no RRULE is handled here;
//! times come in the zone the event was made in and are converted through
//! `exchange::zones` (all-day dates stay dates).

use super::client::{EventResponse, GraphMail};
use crate::db::Db;
use crate::db::repo;
use crate::db::repo::calendar::SyncRow;
use crate::error::{CoreError, Result};
use crate::events::{CoreEvent, EventBus, NewEventInfo};
use crate::graphcal::{
    MAX_GRAPH_DELTA_PAGES, MAX_NEW_EVENT_NOTIFICATIONS, WINDOW_FUTURE_MS, WINDOW_PAST_MS,
    graph_event, ics_event,
};
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
) -> Result<bool> {
    let pushed = push_dirty(db, client, account_id).await?;
    let calendars = db
        .read(move |conn| repo::caldav::list_calendars(conn, Some(account_id)))
        .await?;
    let mut changed_any = pushed;
    let mut new_events: Vec<NewEventInfo> = Vec::new();
    for cal in calendars.into_iter().filter(|c| c.enabled) {
        match sync_calendar(db, client, account_id, cal.id, &cal.url).await {
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

async fn push_dirty(db: &Db, client: &GraphMail, account_id: i64) -> Result<bool> {
    let rows = db
        .read(move |conn| repo::calendar::dirty_rows(conn, account_id))
        .await?;
    let mut any = false;
    for row in rows {
        match push_row(db, client, account_id, &row).await {
            Ok(()) => any = true,
            Err(error @ (CoreError::NeedsReauth | CoreError::Auth(_))) => return Err(error),
            Err(error) => tracing::warn!("exchange push of event {} failed: {error}", row.event.id),
        }
    }
    Ok(any)
}

async fn push_row(db: &Db, client: &GraphMail, account_id: i64, row: &SyncRow) -> Result<()> {
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
    let event = graph_event(&subject, &row.event);
    let payload = crate::graph::event_payload(&event)?;
    match &row.caldav_href {
        Some(href) => {
            client.update_event(href, &payload).await?;
            let calendar_id = row
                .event
                .calendar_id
                .ok_or_else(|| CoreError::CalDav("synced event without calendar".into()))?;
            let href = href.clone();
            db.write(move |conn| {
                repo::calendar::clear_dirty_set_etag(conn, event_id, calendar_id, &href, None, "")
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
) -> Result<(bool, Vec<NewEventInfo>)> {
    let (_, stored) = db
        .read(move |conn| repo::caldav::sync_state(conn, calendar_id))
        .await?;
    let mut incremental = stored.is_some();
    let now = now_ms();
    let window = (now - WINDOW_PAST_MS, now + WINDOW_FUTURE_MS);
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
        if !upserts.is_empty() {
            changed_any = true;
            let batch_new = db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    let mut fresh: Vec<NewEventInfo> = Vec::new();
                    for ev in &upserts {
                        if ev.start_ms == 0 || ev.event_type.as_deref() == Some("seriesMaster") {
                            continue;
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
