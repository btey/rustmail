//! Per-account CalDAV task: an HTTP-only loop separate from the IMAP actor
//! (a slow calendar server must never stall mail sync). Wakes on the selected
//! sync cadence or when nudged (local mutation queued, "sync now", due-check).

use tokio::sync::mpsc;

use crate::accounts::secrets::{self, CredentialStoreHandle, Slot};
use crate::db::Db;
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::EventBus;
use crate::models::{Provider, now_ms};
use crate::oauth::tokens::TokenProvider;

use super::http::{DavAuth, HttpTransport};
use super::sync::sync_account;

const COLLECTION_REFRESH_SECS: i64 = 6 * 60 * 60;
const MAX_ATTEMPTS: i64 = 8;

/// How often a Microsoft 365 calendar is read while the Calendar view is open.
/// Graph's change notifications need a public endpoint, which Rustmail does not
/// have, so the app asks for the (cheap) delta itself: one request per calendar.
pub(crate) const WATCH_POLL_SECS: u64 = 45;
/// The longest wait after repeated failures while watching.
const MAX_BACKOFF_SECS: u64 = 10 * 60;

/// Seconds to wait before the next read of a calendar. Watched Microsoft 365
/// calendars use the short poll (doubled after each failed cycle, up to ten
/// minutes, and never shorter than the normal cadence's own failure wait);
/// everything else keeps the configured cadence.
pub(crate) fn next_poll_secs(configured_secs: u64, watching_exchange: bool, failures: u32) -> u64 {
    if !watching_exchange {
        return configured_secs;
    }
    WATCH_POLL_SECS
        .saturating_mul(1u64 << failures.min(10))
        .min(MAX_BACKOFF_SECS.max(WATCH_POLL_SECS))
        .min(configured_secs.max(WATCH_POLL_SECS))
        .max(WATCH_POLL_SECS)
}

#[derive(Clone)]
pub struct CalTaskHandle {
    pub account_id: i64,
    tx: mpsc::Sender<()>,
}

impl CalTaskHandle {
    pub fn nudge(&self) {
        // Nudges are idempotent; one pending wake represents every newer one.
        let _ = self.tx.try_send(());
    }
}

pub fn spawn(
    db: Db,
    mail_db: Db,
    bus: EventBus,
    tokens: TokenProvider,
    credentials: CredentialStoreHandle,
    account_id: i64,
    watching: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> CalTaskHandle {
    let (tx, mut rx) = mpsc::channel::<()>(1);
    let handle = CalTaskHandle { account_id, tx };
    tokio::spawn(async move {
        let mut last_collection_refresh_ms = 0;
        let mut failures: u32 = 0;
        loop {
            let configured = crate::sync::engine::configured_sync_interval(&mail_db).await;
            let watching_exchange = watching.load(std::sync::atomic::Ordering::Relaxed)
                && db
                    .read(move |conn| repo::caldav::get_config(conn, account_id))
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|cfg| cfg.enabled && cfg.kind == "exchange");
            let poll_delay = std::time::Duration::from_secs(next_poll_secs(
                configured.as_secs(),
                watching_exchange,
                failures,
            ));
            let woke = tokio::select! {
                m = rx.recv() => m.is_some(),
                _ = tokio::time::sleep(poll_delay) => true,
            };
            if !woke {
                break; // handle dropped: account removed / calendar disconnected
            }
            // Drain coalesced nudges.
            while rx.try_recv().is_ok() {}

            let refresh_collections =
                now_ms() - last_collection_refresh_ms >= COLLECTION_REFRESH_SECS * 1_000;
            match run_cycle(
                &db,
                &mail_db,
                &bus,
                &tokens,
                credentials.clone(),
                account_id,
                refresh_collections,
            )
            .await
            {
                Ok(()) => {
                    failures = 0;
                    if refresh_collections {
                        last_collection_refresh_ms = now_ms();
                    }
                }
                Err(CoreError::NeedsReauth) => {
                    failures = failures.saturating_add(1);
                    let _ = db
                        .write(move |conn| {
                            repo::caldav::set_config_error(conn, account_id, Some("needs_reauth"))
                        })
                        .await;
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!("caldav cycle for account {account_id} failed: {e}");
                    let msg = e.to_string();
                    let _ = db
                        .write(move |conn| {
                            repo::caldav::set_config_error(conn, account_id, Some(&msg))
                        })
                        .await;
                }
            }
        }
        tracing::debug!("caldav task for account {account_id} stopped");
    });
    handle
}

/// Resolve the account's transport from its config (Bearer for Google via the
/// OAuth token cache, Basic for generic servers via the keyring).
pub async fn transport_for(
    db: &Db,
    mail_db: &Db,
    tokens: &TokenProvider,
    credential_store: CredentialStoreHandle,
    account_id: i64,
) -> Result<Option<HttpTransport>> {
    let cfg = db
        .read(move |conn| repo::caldav::get_config(conn, account_id))
        .await?;
    let Some(cfg) = cfg.filter(|c| c.enabled) else {
        return Ok(None);
    };
    let auth = match cfg.kind.as_str() {
        "google" => {
            let token = tokens.access_token(account_id, Provider::Gmail).await?;
            DavAuth::Bearer(token)
        }
        _ => {
            let user = cfg.username.clone().unwrap_or_default();
            let pass =
                secrets::load_async(credential_store.clone(), account_id, Slot::CaldavPassword)
                    .await?;
            DavAuth::Basic(user, pass)
        }
    };
    let proxy = crate::proxy_url::for_account(mail_db, &credential_store, account_id).await?;
    Ok(Some(HttpTransport::new(
        auth,
        &cfg.base_url,
        proxy.as_deref(),
    )?))
}

async fn run_cycle(
    db: &Db,
    mail_db: &Db,
    bus: &EventBus,
    tokens: &TokenProvider,
    credential_store: CredentialStoreHandle,
    account_id: i64,
    refresh_collections: bool,
) -> Result<()> {
    let cfg = db
        .read(move |conn| repo::caldav::get_config(conn, account_id))
        .await?;
    let Some(cfg) = cfg.filter(|c| c.enabled) else {
        return Ok(());
    };

    // Working offline: no request is made, edited events stay marked and the
    // queued calendar actions stay pending until the switch is turned off.
    if cfg.kind == "exchange" {
        let mail = mail_db
            .read(move |conn| repo::accounts::get_config(conn, account_id))
            .await?;
        if mail.is_some_and(|mail| {
            crate::exchange::is_work_offline(&mail.settings.connection.exchange)
        }) {
            return Ok(());
        }
    }

    // Claim due cal_% actions up front; sync_account's push-dirty pass is
    // what actually performs them (actions are nudges carrying intent).
    let now = now_ms();
    let claimed: Vec<(i64, i64)> = db
        .write(move |conn| {
            let due = repo::actions::due_calendar(conn, account_id, now, 50)?;
            let mut out = Vec::new();
            for a in due {
                if repo::actions::try_claim(conn, a.id)? {
                    out.push((a.id, a.attempts));
                }
            }
            Ok(out)
        })
        .await?;

    // Microsoft calendars sync over Graph (no CalDAV endpoint exists);
    // everything else goes through the DAV transport.
    let result = if cfg.kind == "microsoft" {
        let proxy = crate::proxy_url::for_account(mail_db, &credential_store, account_id).await?;
        crate::proxy_url::scoped(
            proxy,
            crate::graphcal::sync_account(db, bus, tokens, account_id),
        )
        .await
    } else if cfg.kind == "exchange" {
        // Exchange Online: the mail account's own sign-in, through its Graph client.
        let mail = mail_db
            .read(move |conn| repo::accounts::get_config(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))?;
        let settings = crate::exchange::settings_for(&mail)?;
        let client = crate::exchange::client::GraphMail::new(
            &settings,
            crate::exchange::tokens_for(account_id, &settings, &credential_store),
        )?;
        crate::exchange::calendar::sync_account(
            db,
            bus,
            &client,
            account_id,
            settings.calendar_window_ms(),
        )
        .await
    } else if cfg.kind == "google" {
        let token = tokens.access_token(account_id, Provider::Gmail).await?;
        if refresh_collections {
            let scoped =
                crate::proxy_url::for_account(mail_db, &credential_store, account_id).await?;
            let discovered =
                crate::proxy_url::scoped(scoped, crate::googlecal::list_calendars(&token)).await?;
            db.write(move |conn| {
                crate::googlecal::reconcile_calendars(conn, account_id, &discovered).map(|_| ())
            })
            .await?;
            bus.emit(crate::events::CoreEvent::CalendarUpdated { account_id });
        }
        let proxy = crate::proxy_url::for_account(mail_db, &credential_store, account_id).await?;
        let transport =
            HttpTransport::new(DavAuth::Bearer(token), &cfg.base_url, proxy.as_deref())?;
        let self_email = mail_db
            .read(move |conn| Ok(repo::accounts::get(conn, account_id)?.map(|a| a.email)))
            .await?
            .unwrap_or_default();
        sync_account(db, bus, &transport, account_id, &self_email).await
    } else {
        let self_email = mail_db
            .read(move |conn| Ok(repo::accounts::get(conn, account_id)?.map(|a| a.email)))
            .await?
            .unwrap_or_default();
        match transport_for(db, mail_db, tokens, credential_store.clone(), account_id).await? {
            Some(transport) => sync_account(db, bus, &transport, account_id, &self_email).await,
            None => return Ok(()),
        }
    };

    // The Microsoft 365 cycle keeps its own status: a change the server
    // refused must stay visible, so a finished cycle does not wipe it.
    let keeps_own_status = cfg.kind == "exchange";
    match &result {
        Ok(_) => {
            db.write(move |conn| {
                for (id, _) in &claimed {
                    repo::actions::set_state(conn, *id, "done", None)?;
                }
                if !keeps_own_status {
                    repo::caldav::set_config_error(conn, account_id, None)?;
                }
                Ok(())
            })
            .await?;
        }
        Err(e) => {
            let msg = e.to_string();
            db.write(move |conn| {
                for (id, attempts) in &claimed {
                    if *attempts + 1 >= MAX_ATTEMPTS {
                        repo::actions::set_state(conn, *id, "failed", Some(&msg))?;
                    } else {
                        let backoff = 60_000 * (1 << (*attempts).min(6));
                        repo::actions::bump_attempt(conn, *id, now_ms() + backoff, &msg)?;
                    }
                }
                Ok(())
            })
            .await?;
        }
    }
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watched_microsoft_365_calendar_polls_fast_and_backs_off_after_failures() {
        // Not watched: the configured cadence, failures or not.
        assert_eq!(next_poll_secs(300, false, 0), 300);
        assert_eq!(next_poll_secs(300, false, 5), 300);
        // Watched: the short poll, doubling per failure, capped by the
        // configured cadence (never below the short poll).
        assert_eq!(next_poll_secs(900, true, 0), WATCH_POLL_SECS);
        assert_eq!(next_poll_secs(900, true, 1), 90);
        assert_eq!(next_poll_secs(900, true, 2), 180);
        assert_eq!(next_poll_secs(900, true, 10), 600);
        assert_eq!(next_poll_secs(60, true, 10), 60);
        assert_eq!(next_poll_secs(10, true, 0), WATCH_POLL_SECS);
    }
}
