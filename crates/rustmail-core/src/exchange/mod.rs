//! Exchange Online (Microsoft 365) accounts over Microsoft Graph, with no
//! IMAP/SMTP/EWS and no local proxy. See `ROADMAP.md`, Phase 12.
//!
//! - `settings`: per-account configuration and its validation.
//! - `auth`: OAuth 2.0 authorization code + PKCE, device code, token refresh.
//! - `client`: the Graph mail REST client (folders, delta, MIME, actions, send).

pub mod auth;
pub mod client;
pub mod settings;
#[cfg(test)]
pub(crate) mod testing;
pub mod zones;

pub use settings::{ExchangeCloud, ExchangeEndpoint, ExchangeSettings, ExchangeSignIn};

/// What the setup form hands over once the user has signed in.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddExchangeAccountArgs {
    pub email: String,
    pub display_name: Option<String>,
    pub exchange: ExchangeSettings,
}

pub(crate) mod calendar;
pub(crate) mod contacts;
pub(crate) mod draft;
pub mod recurrence;
pub(crate) mod sync;

use crate::accounts::secrets::CredentialStoreHandle;
use crate::error::Result;
use crate::models::AccountConfig;
use crate::sync::engine::SyncCtx;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

type TokenRegistry = Mutex<HashMap<i64, (ExchangeSettings, Arc<auth::ExchangeTokens>)>>;

/// One token cache per account, shared by the sync actor and the sender, so a
/// rotated refresh token is never overwritten by a second holder of the old one.
pub(crate) fn tokens_for(
    account_id: i64,
    settings: &ExchangeSettings,
    credentials: &CredentialStoreHandle,
) -> Arc<auth::ExchangeTokens> {
    static REGISTRY: OnceLock<TokenRegistry> = OnceLock::new();
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((known, tokens)) = registry.get(&account_id)
        && known == settings
    {
        return tokens.clone();
    }
    let tokens = auth::ExchangeTokens::new(account_id, settings.clone(), credentials.clone());
    registry.insert(account_id, (settings.clone(), tokens.clone()));
    tokens
}

/// The account's Exchange settings with its proxy setting applied: an HTTP
/// (CONNECT) proxy, or a SOCKS5 proxy that resolves the host names itself
/// (`socks5h`). The proxy settings carry no login, so a SOCKS5 proxy must
/// accept the "no authentication" method.
pub(crate) fn settings_for(config: &AccountConfig) -> Result<ExchangeSettings> {
    use crate::models::ProxyKind;
    let connection = &config.settings.connection;
    let mut settings = connection.exchange.clone();
    match connection.proxy {
        ProxyKind::None => {}
        ProxyKind::Http | ProxyKind::Socks5 => {
            let host = connection.proxy_host.trim();
            let scheme = if connection.proxy == ProxyKind::Http {
                "http"
            } else {
                "socks5h"
            };
            let url = url::Url::parse(&format!("{scheme}://{host}:{}", connection.proxy_port))
                .ok()
                .filter(|url| {
                    url.host_str().is_some()
                        && !host.is_empty()
                        && connection.proxy_port != 0
                        && url.username().is_empty()
                })
                .ok_or_else(|| {
                    crate::error::CoreError::Other(
                        "The proxy host or port of this account is not valid.".into(),
                    )
                })?;
            settings.proxy_url = url.to_string();
        }
    }
    Ok(settings)
}

type DirectoryCache = Mutex<HashMap<i64, Vec<(String, String)>>>;

const DIRECTORY_CACHE_PER_ACCOUNT: usize = 200;

fn directory_cache() -> &'static DirectoryCache {
    static CACHE: OnceLock<DirectoryCache> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Keep what the organization directory answered, in memory only, so the
/// composer can offer it as the user types without waiting for the network.
pub(crate) fn remember_directory(account_id: i64, hits: &[(String, String)]) {
    let mut cache = directory_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let known = cache.entry(account_id).or_default();
    for (name, email) in hits {
        if !known
            .iter()
            .any(|(_, known_email)| known_email.eq_ignore_ascii_case(email))
        {
            known.push((name.clone(), email.clone()));
        }
    }
    let excess = known.len().saturating_sub(DIRECTORY_CACHE_PER_ACCOUNT);
    known.drain(..excess);
}

/// Directory entries heard earlier that match `query` (name or address).
pub fn cached_directory(
    account_id: Option<i64>,
    query: &str,
    limit: usize,
) -> Vec<(String, String)> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let cache = directory_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .iter()
        .filter(|(id, _)| account_id.is_none_or(|wanted| wanted == **id))
        .flat_map(|(_, entries)| entries.iter())
        .filter(|(name, email)| {
            name.to_lowercase().contains(&needle) || email.to_lowercase().contains(&needle)
        })
        .take(limit)
        .cloned()
        .collect()
}

pub(crate) fn forget_directory(account_id: i64) {
    directory_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&account_id);
}

static GLOBAL_WORK_OFFLINE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The "Work offline" switch of the Settings dialog (all accounts).
pub fn set_global_work_offline(offline: bool) {
    GLOBAL_WORK_OFFLINE.store(offline, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the account must stay off the network: the global switch or the
/// account's own.
pub fn is_work_offline(settings: &ExchangeSettings) -> bool {
    settings.work_offline || GLOBAL_WORK_OFFLINE.load(std::sync::atomic::Ordering::Relaxed)
}

/// A Graph client for the account, using its stored credentials. While the
/// account works offline this fails at once with [`CoreError::Offline`]
/// instead of waiting for a connection that is not wanted.
pub(crate) fn graph_for(ctx: &SyncCtx, config: &AccountConfig) -> Result<client::GraphMail> {
    let settings = settings_for(config)?;
    if is_work_offline(&settings) {
        return Err(crate::error::CoreError::Offline);
    }
    let tokens = tokens_for(config.id, &settings, &ctx.credentials);
    client::GraphMail::new(&settings, tokens)
}

/// Send a built message through Microsoft 365 (which files the Sent Items copy).
pub(crate) async fn send_raw(
    ctx: &SyncCtx,
    config: &AccountConfig,
    draft_id: i64,
    raw: &[u8],
) -> Result<()> {
    if raw.len() > client::MAX_SIMPLE_MIME_BYTES {
        draft::send_large(ctx, config, draft_id).await?;
    } else {
        graph_for(ctx, config)?.send_mime(raw).await?;
    }
    // The server copy of the draft (if one was pushed) is not wanted now.
    draft::discard_after_send(ctx, config, draft_id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AccountSettings, AuthKind, MailProtocol, Provider, ProxyKind};

    fn config(proxy: ProxyKind, host: &str, port: u16) -> AccountConfig {
        let mut settings = AccountSettings::default();
        settings.connection.exchange.enabled = true;
        settings.connection.proxy = proxy;
        settings.connection.proxy_host = host.into();
        settings.connection.proxy_port = port;
        AccountConfig {
            id: 1,
            email: "me@example.org".into(),
            display_name: None,
            avatar_url: None,
            provider: Provider::Imap,
            auth_kind: AuthKind::Password,
            mail_protocol: MailProtocol::Exchange,
            username: "me@example.org".into(),
            jmap_url: String::new(),
            jmap_account_id: None,
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            settings,
        }
    }

    #[test]
    fn the_global_and_the_account_switch_each_take_an_account_offline() {
        let mut settings = ExchangeSettings::default();
        assert!(!is_work_offline(&settings));
        settings.work_offline = true;
        assert!(is_work_offline(&settings));
        settings.work_offline = false;
        set_global_work_offline(true);
        let global = is_work_offline(&settings);
        set_global_work_offline(false);
        assert!(global && !is_work_offline(&settings));
    }

    #[test]
    fn the_account_proxy_setting_reaches_the_exchange_settings() {
        assert!(
            settings_for(&config(ProxyKind::None, "", 0))
                .unwrap()
                .proxy()
                .is_none()
        );
        let http = settings_for(&config(ProxyKind::Http, "proxy.example.org", 3128)).unwrap();
        assert_eq!(http.proxy(), Some("http://proxy.example.org:3128/"));
        assert!(settings_for(&config(ProxyKind::Http, "", 3128)).is_err());
        assert!(settings_for(&config(ProxyKind::Http, "proxy.example.org", 0)).is_err());
        let socks = settings_for(&config(ProxyKind::Socks5, "proxy.example.org", 1080)).unwrap();
        assert_eq!(socks.proxy(), Some("socks5h://proxy.example.org:1080"));
        assert!(settings_for(&config(ProxyKind::Socks5, "", 1080)).is_err());
        assert!(settings_for(&config(ProxyKind::Socks5, "proxy.example.org", 0)).is_err());
    }
}
