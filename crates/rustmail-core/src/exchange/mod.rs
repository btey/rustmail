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

/// The account's Exchange settings with its proxy setting applied. Only an
/// HTTP (CONNECT) proxy is supported for Microsoft 365 traffic.
pub(crate) fn settings_for(config: &AccountConfig) -> Result<ExchangeSettings> {
    use crate::models::ProxyKind;
    let connection = &config.settings.connection;
    let mut settings = connection.exchange.clone();
    match connection.proxy {
        ProxyKind::None => {}
        ProxyKind::Http => {
            let host = connection.proxy_host.trim();
            let url = url::Url::parse(&format!("http://{host}:{}", connection.proxy_port))
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
        ProxyKind::Socks5 => {
            return Err(crate::error::CoreError::Other(
                "Microsoft 365 accounts can use an HTTP proxy only; a SOCKS5 proxy is not supported for them.".into(),
            ));
        }
    }
    Ok(settings)
}

/// A Graph client for the account, using its stored credentials.
pub(crate) fn graph_for(ctx: &SyncCtx, config: &AccountConfig) -> Result<client::GraphMail> {
    let settings = settings_for(config)?;
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
        let socks = settings_for(&config(ProxyKind::Socks5, "proxy.example.org", 1080));
        assert!(socks.unwrap_err().to_string().contains("HTTP proxy only"));
    }
}
