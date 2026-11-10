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

pub use settings::{ExchangeCloud, ExchangeEndpoint, ExchangeSettings, ExchangeSignIn};

/// What the setup form hands over once the user has signed in.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddExchangeAccountArgs {
    pub email: String,
    pub display_name: Option<String>,
    pub exchange: ExchangeSettings,
}

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

/// A Graph client for the account, using its stored credentials.
pub(crate) fn graph_for(ctx: &SyncCtx, config: &AccountConfig) -> Result<client::GraphMail> {
    let settings = &config.settings.connection.exchange;
    client::GraphMail::new(settings, tokens_for(config.id, settings, &ctx.credentials))
}

/// Send a built message through Microsoft 365 (which files the Sent Items copy).
pub(crate) async fn send_raw(ctx: &SyncCtx, config: &AccountConfig, raw: &[u8]) -> Result<()> {
    graph_for(ctx, config)?.send_mime(raw).await
}
