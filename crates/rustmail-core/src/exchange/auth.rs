//! Sign-in for Exchange Online accounts: OAuth 2.0 authorization code with
//! PKCE (system browser, loopback redirect), the device-code and
//! paste-the-address fallbacks, and refresh-token renewal.
//!
//! No password ever reaches Rustmail: the browser (or the device code page)
//! handles credentials, MFA and conditional access. The refresh token is
//! stored only in the credential store (`Slot::RefreshToken`); access tokens
//! live in memory. Tokens are never written to logs or error text.

use super::settings::{ExchangeSettings, ExchangeSignIn};
use crate::accounts::secrets::{CredentialStoreHandle, Slot};
use crate::error::{CoreError, Result};
use crate::oauth::redirect::{OAUTH_REDIRECT_TIMEOUT, OAuthRedirectBrokerHandle};
use crate::oauth::tokens::post_form_via;
use base64::Engine;
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const MAX_TOKEN_BYTES: usize = 64 * 1024;
const MAX_DEVICE_WAIT: Duration = Duration::from_secs(15 * 60);

/// What a token endpoint handed back.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    /// Space-separated permissions the token carries, when Microsoft says.
    pub scope: Option<String>,
}

// Tokens must never end up in a log line through `{:?}`.
impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

/// The v1 endpoints send numbers such as `expires_in` as strings, the v2.0
/// ones as numbers: accept both.
fn number_or_string<'de, D>(deserializer: D) -> std::result::Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(number) => number.as_i64(),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    })
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default, deserialize_with = "number_or_string")]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

/// An authorization request ready to open in a browser.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    pub url: String,
    pub state: String,
    pub verifier: String,
    pub redirect_uri: String,
}

fn random_string(len: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}

/// Build the sign-in address. `redirect_uri` is the address the browser will
/// be sent back to (the loopback listener's, or the configured one).
pub fn build_authorization_request(
    settings: &ExchangeSettings,
    redirect_uri: &str,
    login_hint: Option<&str>,
    state: Option<&str>,
) -> Result<AuthorizationRequest> {
    settings.validate_sign_in_fields()?;
    let verifier = random_string(64);
    let state = state.map_or_else(|| random_string(32), str::to_owned);
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("response_type", "code")
        .append_pair("client_id", settings.client_id())
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_mode", "query");
    // The classic endpoint names the API (`resource`) and takes its
    // permissions from the app registration; the v2.0 one lists scopes.
    if settings.uses_resource_endpoint() {
        query.append_pair("resource", &settings.resource_url()?);
    } else {
        query.append_pair("scope", &settings.effective_scopes().join(" "));
    }
    query.append_pair("state", &state);
    // The classic sign-in of Microsoft's own apps is a public client without
    // PKCE (DavMail sends none either); a challenge there can end in
    // AADSTS9002313.
    if !settings.uses_resource_endpoint() {
        query
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256");
    }
    match login_hint.map(str::trim).filter(|hint| !hint.is_empty()) {
        Some(hint) => {
            query.append_pair("login_hint", hint);
        }
        // Without a hint an active browser session would sign in silently as
        // someone else; make the account choice visible.
        None => {
            query.append_pair("prompt", "select_account");
        }
    }
    Ok(AuthorizationRequest {
        url: format!("{}?{}", settings.authorize_url()?, query.finish()),
        state,
        verifier,
        redirect_uri: redirect_uri.to_owned(),
    })
}

async fn token_request(
    settings: &ExchangeSettings,
    form: Vec<(String, String)>,
    sign_in: bool,
) -> Result<TokenSet> {
    let body = post_form_via(&settings.token_url()?, &form, settings.proxy()).await?;
    parse_token_response(&body, sign_in)
}

/// `sign_in` is true while a person is signing in: a refusal then says what
/// Microsoft said instead of asking for a sign-in they are already doing.
fn parse_token_response(body: &str, sign_in: bool) -> Result<TokenSet> {
    if body.len() > MAX_TOKEN_BYTES * 4 {
        return Err(CoreError::Auth(
            "the sign-in service sent an oversized reply".into(),
        ));
    }
    let Ok(token) = serde_json::from_str::<TokenResponse>(body) else {
        return Err(if sign_in {
            explain_sign_in_error(body)
        } else {
            explain_token_error(body)
        });
    };
    let valid = |value: &str| {
        !value.is_empty() && value.len() <= MAX_TOKEN_BYTES && !value.chars().any(char::is_control)
    };
    if !valid(&token.access_token)
        || token.refresh_token.as_deref().is_some_and(|t| !valid(t))
        || token
            .expires_in
            .is_some_and(|seconds| !(1..=366 * 24 * 3600).contains(&seconds))
    {
        return Err(CoreError::Auth(
            "the sign-in service returned an invalid token".into(),
        ));
    }
    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_in: token.expires_in,
        scope: token.scope.filter(|scope| !scope.trim().is_empty()),
    })
}

/// The resource sign-in cannot ask for permissions, so check what the token
/// carries: a list without mail access would only fail later, in the sync,
/// with a less helpful message.
fn ensure_mail_access(settings: &ExchangeSettings, tokens: &TokenSet) -> Result<()> {
    if !settings.uses_resource_endpoint() {
        return Ok(());
    }
    let Some(scope) = tokens.scope.as_deref() else {
        return Ok(());
    };
    if scope
        .split_whitespace()
        .any(|granted| granted.starts_with("Mail.ReadWrite"))
    {
        return Ok(());
    }
    Err(CoreError::Auth(
        "Microsoft signed you in, but this app is not allowed to read and write mail through Microsoft Graph. Enter the client ID of your own app registration in the account settings.".into(),
    ))
}

/// The token request fields that name what the token is for.
fn audience(settings: &ExchangeSettings) -> Result<(String, String)> {
    if settings.uses_resource_endpoint() {
        Ok(("resource".into(), settings.resource_url()?))
    } else {
        Ok(("scope".into(), settings.effective_scopes().join(" ")))
    }
}

/// Redeem an authorization code.
pub async fn exchange_code(
    settings: &ExchangeSettings,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenSet> {
    let mut form = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("client_id".into(), settings.client_id().to_owned()),
        ("code".into(), code.to_owned()),
        ("redirect_uri".into(), redirect_uri.to_owned()),
        audience(settings)?,
    ];
    if !settings.uses_resource_endpoint() {
        form.push(("code_verifier".into(), verifier.to_owned()));
    }
    let tokens = token_request(settings, form, true).await?;
    ensure_mail_access(settings, &tokens)?;
    if tokens.refresh_token.is_none() {
        return Err(CoreError::Auth(
            "Microsoft did not return an offline refresh token; make sure the offline_access scope is allowed and sign in again".into(),
        ));
    }
    Ok(tokens)
}

/// Trade a refresh token for a new access token (and a rotated refresh token).
pub async fn refresh_tokens(settings: &ExchangeSettings, refresh_token: &str) -> Result<TokenSet> {
    let form = vec![
        ("grant_type".into(), "refresh_token".into()),
        ("client_id".into(), settings.client_id().to_owned()),
        ("refresh_token".into(), refresh_token.to_owned()),
        audience(settings)?,
    ];
    token_request(settings, form, false).await
}

/// Browser sign-in: open the system browser, wait for the loopback redirect,
/// and redeem the code. `open_url` is supplied by the host.
pub async fn authorize_browser(
    settings: &ExchangeSettings,
    login_hint: Option<&str>,
    redirects: OAuthRedirectBrokerHandle,
    open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
) -> Result<TokenSet> {
    if settings.sign_in != ExchangeSignIn::Browser {
        return Err(CoreError::Auth(
            "this account is set to a different sign-in method".into(),
        ));
    }
    let state = random_string(32);
    let session = redirects
        .begin(crate::models::Provider::Microsoft, &state)
        .await?;
    let redirect_uri = session.redirect_uri().to_owned();
    let request = build_authorization_request(settings, &redirect_uri, login_hint, Some(&state))?;
    open_url(request.url.clone())
        .map_err(|error| CoreError::Auth(format!("could not open the sign-in browser: {error}")))?;
    let code = session.wait(OAUTH_REDIRECT_TIMEOUT).await?;
    if code.state.as_deref() != Some(request.state.as_str()) {
        return Err(CoreError::Auth(
            "the sign-in state did not match; try again".into(),
        ));
    }
    exchange_code(settings, &code.code, &request.verifier, &redirect_uri).await
}

/// Manual sign-in: the user pastes the address the browser ended on (or just
/// the code). Returns the authorization code.
pub fn code_from_pasted(input: &str, expected_state: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() || input.len() > 8192 {
        return Err(CoreError::Auth(
            "paste the address the browser ended on".into(),
        ));
    }
    // Browsers and some pages shorten a long address with an ellipsis; the
    // code would then be cut in the middle and Microsoft calls it malformed.
    if input.contains('\u{2026}') || input.contains("...") {
        return Err(CoreError::Auth(
            "the pasted address was shortened (it contains \"\u{2026}\"), so the sign-in code is incomplete. Copy the whole address again: click the address bar, select all and copy, or use the page's copy button, then paste it here".into(),
        ));
    }
    // A pasted address without its scheme ("login.microsoftonline.com/...?code=").
    let with_scheme;
    let candidate = if !input.contains("://")
        && !input.starts_with('?')
        && !input.starts_with("urn:")
        && !input.chars().any(char::is_whitespace)
        && (input.contains("?code=") || input.contains("&code="))
    {
        with_scheme = format!("https://{input}");
        with_scheme.as_str()
    } else {
        input
    };
    // The out-of-band page shows only the answer, e.g. "Success code=0.AX…&
    // session_state=…": take it from the first "code=" or "error=" on.
    let answer_only = !candidate.contains("://")
        && !candidate.starts_with("urn:")
        && ["code=", "error="].iter().any(|key| {
            candidate.find(key).is_some_and(|at| {
                candidate[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|prev| !prev.is_ascii_alphanumeric())
            })
        });
    let parsed = if answer_only {
        None
    } else {
        url::Url::parse(candidate).ok()
    };
    if parsed.is_none() && !answer_only {
        // A bare code: no state to check. Anything that looks like a form
        // would be sent to Microsoft as a malformed code.
        if input
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control() || matches!(ch, '&' | '=' | '?'))
        {
            return Err(CoreError::Auth("that is not a sign-in code".into()));
        }
        return Ok(input.to_owned());
    }
    let answer = if answer_only {
        let at = ["code=", "error="]
            .iter()
            .filter_map(|key| candidate.find(key))
            .min()
            .unwrap_or(0);
        candidate[at..].to_owned()
    } else {
        String::new()
    };
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    // The query carries the answer (response_mode=query); a fragment is read
    // too for a browser or proxy that moved it.
    let fragment = parsed
        .as_ref()
        .and_then(|parsed| parsed.fragment())
        .unwrap_or_default()
        .to_owned();
    let query = parsed
        .as_ref()
        .and_then(|parsed| parsed.query())
        .unwrap_or_default();
    for (key, value) in url::form_urlencoded::parse(answer.as_bytes())
        .chain(url::form_urlencoded::parse(query.as_bytes()))
        .chain(url::form_urlencoded::parse(fragment.as_bytes()))
    {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }
    if error.is_some() || description.is_some() {
        return Err(explain_token_error(
            &serde_json::json!({ "error": error, "error_description": description }).to_string(),
        ));
    }
    // The out-of-band page may leave `state` out; the PKCE verifier still
    // binds the code to this attempt. A state that is present must match.
    if state
        .as_deref()
        .is_some_and(|state| state != expected_state)
    {
        return Err(CoreError::Auth(
            "the pasted address belongs to a different sign-in attempt".into(),
        ));
    }
    code.filter(|code| !code.is_empty())
        .ok_or_else(|| CoreError::Auth("the pasted address has no sign-in code".into()))
}

/// What the user must do to finish a device-code sign-in.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    device_code: String,
}

#[derive(serde::Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    // The classic endpoint calls it `verification_url`.
    #[serde(alias = "verification_url")]
    verification_uri: String,
    #[serde(default, deserialize_with = "number_or_string")]
    expires_in: Option<i64>,
    #[serde(default, deserialize_with = "number_or_string")]
    interval: Option<i64>,
}

pub async fn start_device_code(settings: &ExchangeSettings) -> Result<DeviceCode> {
    settings.validate_sign_in_fields()?;
    let form = vec![
        ("client_id".into(), settings.client_id().to_owned()),
        audience(settings)?,
    ];
    let body = post_form_via(&settings.device_code_url()?, &form, settings.proxy()).await?;
    let Ok(reply) = serde_json::from_str::<DeviceCodeResponse>(&body) else {
        return Err(explain_token_error(&body));
    };
    let uri_ok = url::Url::parse(&reply.verification_uri)
        .is_ok_and(|url| url.scheme() == "https" || url.host_str() == Some("127.0.0.1"));
    if reply.device_code.is_empty() || reply.user_code.is_empty() || !uri_ok {
        return Err(CoreError::Auth(
            "the sign-in service sent an invalid device code".into(),
        ));
    }
    Ok(DeviceCode {
        user_code: reply.user_code,
        verification_uri: reply.verification_uri,
        expires_in: u64::try_from(reply.expires_in.unwrap_or(0))
            .unwrap_or(0)
            .clamp(1, MAX_DEVICE_WAIT.as_secs()),
        interval: u64::try_from(reply.interval.unwrap_or(0))
            .unwrap_or(0)
            .clamp(1, 30),
        device_code: reply.device_code,
    })
}

/// Poll until the user finishes signing in, declines, or the code expires.
/// `min_interval` lets tests poll quickly.
pub async fn poll_device_code(
    settings: &ExchangeSettings,
    device: &DeviceCode,
    min_interval: Option<Duration>,
) -> Result<TokenSet> {
    let started = std::time::Instant::now();
    let mut interval = min_interval.unwrap_or(Duration::from_secs(device.interval));
    let limit = Duration::from_secs(device.expires_in).min(MAX_DEVICE_WAIT);
    loop {
        tokio::time::sleep(interval).await;
        if started.elapsed() > limit {
            return Err(CoreError::Auth(
                "the device code expired; start the sign-in again".into(),
            ));
        }
        let mut form = vec![
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
            ("client_id".into(), settings.client_id().to_owned()),
        ];
        if settings.uses_resource_endpoint() {
            // The classic endpoint names the code `code` and wants the API.
            form.push(("code".into(), device.device_code.clone()));
            form.push(audience(settings)?);
        } else {
            form.push(("device_code".into(), device.device_code.clone()));
        }
        let body = post_form_via(&settings.token_url()?, &form, settings.proxy()).await?;
        let error = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|e| e.as_str().map(str::to_owned))
            });
        match error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                interval += if min_interval.is_some() {
                    Duration::from_millis(5)
                } else {
                    Duration::from_secs(5)
                };
            }
            Some("authorization_declined") => {
                return Err(CoreError::Auth("the sign-in was declined".into()));
            }
            Some("expired_token") => {
                return Err(CoreError::Auth(
                    "the device code expired; start the sign-in again".into(),
                ));
            }
            _ => {
                let tokens = parse_token_response(&body, true)?;
                ensure_mail_access(settings, &tokens)?;
                if tokens.refresh_token.is_none() {
                    return Err(CoreError::Auth(
                        "Microsoft did not return an offline refresh token; sign in again".into(),
                    ));
                }
                return Ok(tokens);
            }
        }
    }
}

/// Turn a token-endpoint error body into text a person can act on. Only the
/// AADSTS code and Microsoft's one-line description are kept; nothing from the
/// request (and no token) is echoed.
pub fn explain_token_error(body: &str) -> CoreError {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let description = value
        .get("error_description")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let code = value
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let aadsts = description.find("AADSTS").map(|start| {
        description[start..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
    });
    let advice = match aadsts.as_deref() {
        Some("AADSTS65001") => Some(
            "An administrator must approve Rustmail (or your own app registration) for this organization, or you must consent in the sign-in window. Ask your administrator for approval.",
        ),
        Some("AADSTS90094" | "AADSTS90095") => Some(
            "Admin approval is required before this app can be used. Ask your administrator to grant consent.",
        ),
        Some("AADSTS700016") => Some(
            "The application (client) ID was not found in this tenant. Check the client ID and tenant in the account settings.",
        ),
        Some("AADSTS700025" | "AADSTS7000218") => Some(
            "The app registration is not a public client. In Entra ID, enable \"Allow public client flows\" for it.",
        ),
        Some("AADSTS50020") => Some(
            "This account does not belong to the selected tenant. Use a tenant ID or the organizations endpoint.",
        ),
        Some("AADSTS53003" | "AADSTS530003") => Some(
            "A conditional access policy of your organization blocks this sign-in from this device or app.",
        ),
        Some("AADSTS50076" | "AADSTS50079") => {
            Some("Multi-factor authentication is required. Finish the sign-in in the browser.")
        }
        Some("AADSTS50058") => Some(
            "Microsoft could not find a signed-in session. Sign in again and complete the sign-in window.",
        ),
        Some("AADSTS50105") => {
            Some("Your account is not assigned to this application. Ask your administrator.")
        }
        Some("AADSTS90002" | "AADSTS900023") => {
            Some("The tenant was not found. Check the tenant in the account settings.")
        }
        Some("AADSTS50011") => Some(
            "The redirect address is not registered for this app. Add it in Entra ID or change the redirect URI in the account settings.",
        ),
        Some("AADSTS70043" | "AADSTS700082") => Some("The sign-in expired. Sign in again."),
        Some("AADSTS65002") => Some(
            "Microsoft does not let this app ask for the requested permissions. The built-in client ID is Microsoft's own Office app and may not be allowed to use Microsoft Graph; use the Resource sign-in style, or enter the client ID of your own app registration, in the account settings.",
        ),
        Some("AADSTS500011") => {
            Some("This app is not allowed to use the requested permissions in this tenant.")
        }
        Some("AADSTS7000112" | "AADSTS900144") => Some(
            "The tenant or an administrator has turned this app off. Ask your administrator, or use your own app registration.",
        ),
        _ => None,
    };
    let reauth = matches!(
        code,
        "invalid_grant" | "interaction_required" | "consent_required"
    ) || matches!(
        aadsts.as_deref(),
        Some("AADSTS70043" | "AADSTS700082" | "AADSTS50076" | "AADSTS50079")
    );
    if reauth && advice.is_none_or(|text| text.starts_with("The sign-in expired")) {
        return CoreError::NeedsReauth;
    }
    let first_line =
        crate::http_body::single_line_excerpt(description.lines().next().unwrap_or_default(), 200);
    let mut message = String::from("Microsoft sign-in failed");
    if let Some(aadsts) = &aadsts {
        message.push_str(&format!(" ({aadsts})"));
    } else if !code.is_empty() {
        message.push_str(&format!(
            " ({})",
            crate::http_body::single_line_excerpt(code, 40)
        ));
    }
    message.push('.');
    if let Some(advice) = advice {
        message.push(' ');
        message.push_str(advice);
    } else if !first_line.is_empty() {
        message.push(' ');
        message.push_str(&first_line);
    }
    CoreError::Auth(message)
}

/// Like `explain_token_error`, for a sign-in in progress: the codes that mean
/// "sign in again" for a stored account (`invalid_grant`, `interaction_required`,
/// `consent_required`, ...) keep Microsoft's own description here, because the
/// person is already signing in and needs to know why it was refused.
pub fn explain_sign_in_error(body: &str) -> CoreError {
    let CoreError::NeedsReauth = explain_token_error(body) else {
        return explain_token_error(body);
    };
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let code = value
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("error");
    let description = value
        .get("error_description")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let line =
        crate::http_body::single_line_excerpt(description.lines().next().unwrap_or_default(), 300);
    CoreError::Auth(format!(
        "Microsoft refused the sign-in ({}){}{}. If this keeps happening, check the redirect address, that the code was pasted only once, and that your organization allows this app.",
        crate::http_body::single_line_excerpt(code, 40),
        if line.is_empty() { "" } else { ": " },
        line
    ))
}

/// Access tokens for one account: cached in memory, renewed from the stored
/// refresh token, which is rotated in the credential store when Microsoft
/// issues a new one.
pub struct ExchangeTokens {
    account_id: i64,
    settings: ExchangeSettings,
    credentials: CredentialStoreHandle,
    cache: Mutex<Option<(String, i64)>>,
}

impl ExchangeTokens {
    pub fn new(
        account_id: i64,
        settings: ExchangeSettings,
        credentials: CredentialStoreHandle,
    ) -> Arc<Self> {
        Arc::new(Self {
            account_id,
            settings,
            credentials,
            cache: Mutex::new(None),
        })
    }

    /// Persist the result of a sign-in.
    pub async fn store_initial(&self, tokens: &TokenSet) -> Result<()> {
        let refresh = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| CoreError::Auth("no refresh token to store".into()))?;
        let credentials = self.credentials.clone();
        let id = self.account_id;
        tokio::task::spawn_blocking(move || credentials.store(id, Slot::RefreshToken, &refresh))
            .await
            .map_err(|error| CoreError::Other(error.to_string()))??;
        self.remember(tokens).await;
        Ok(())
    }

    async fn remember(&self, tokens: &TokenSet) {
        let usable = tokens
            .expires_in
            .unwrap_or(3600)
            .clamp(1, 366 * 24 * 3600)
            .saturating_sub(60)
            .max(1);
        *self.cache.lock().await = Some((
            tokens.access_token.clone(),
            crate::models::now_ms().saturating_add(usable * 1000),
        ));
    }

    pub async fn invalidate(&self) {
        *self.cache.lock().await = None;
    }

    pub async fn access_token(&self) -> Result<String> {
        // One refresh at a time per account: a rotated refresh token must not
        // be overwritten by a racing request holding the old one.
        let mut cache = self.cache.lock().await;
        if let Some((token, expires_at)) = cache.as_ref()
            && *expires_at > crate::models::now_ms()
        {
            return Ok(token.clone());
        }
        let credentials = self.credentials.clone();
        let id = self.account_id;
        let refresh = tokio::task::spawn_blocking(move || credentials.load(id, Slot::RefreshToken))
            .await
            .map_err(|error| CoreError::Other(error.to_string()))?
            .map_err(|error| match error {
                CoreError::Auth(_) => CoreError::NeedsReauth,
                other => other,
            })?;
        let tokens = refresh_tokens(&self.settings, &refresh).await?;
        if let Some(rotated) = tokens.refresh_token.clone().filter(|new| *new != refresh) {
            let credentials = self.credentials.clone();
            tokio::task::spawn_blocking(move || {
                credentials.store(id, Slot::RefreshToken, &rotated)
            })
            .await
            .map_err(|error| CoreError::Other(error.to_string()))??;
        }
        let usable = tokens
            .expires_in
            .unwrap_or(3600)
            .clamp(1, 366 * 24 * 3600)
            .saturating_sub(60)
            .max(1);
        *cache = Some((
            tokens.access_token.clone(),
            crate::models::now_ms().saturating_add(usable * 1000),
        ));
        Ok(tokens.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::settings::ExchangeEndpoint;
    use crate::exchange::testing::{FakeServer, Reply};

    fn settings(base: &str) -> ExchangeSettings {
        ExchangeSettings {
            enabled: true,
            client_id: "facd6cff-a294-4415-b59f-c5b01937d7bd".into(),
            cloud: crate::exchange::settings::ExchangeCloud::Custom,
            custom_login_url: base.into(),
            custom_graph_url: format!("{base}/graph"),
            ..ExchangeSettings::default()
        }
    }

    #[test]
    fn authorization_url_carries_pkce_state_scopes_and_hint() {
        let s = settings("https://login.example.org");
        let request =
            build_authorization_request(&s, "http://localhost:4000", Some("a@b.com"), None)
                .unwrap();
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.path(), "/common/oauth2/v2.0/authorize");
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], s.client_id);
        assert_eq!(query["redirect_uri"], "http://localhost:4000");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["code_challenge"], pkce_challenge(&request.verifier));
        assert_eq!(query["state"], request.state);
        assert_eq!(query["login_hint"], "a@b.com");
        assert!(!query.contains_key("prompt"));
        assert!(
            query["scope"].contains("offline_access") && query["scope"].contains("Mail.ReadWrite")
        );
        assert!(request.verifier.len() >= 43);
        let no_hint = build_authorization_request(&s, "http://localhost:4000", None, None).unwrap();
        assert!(no_hint.url.contains("prompt=select_account"));
    }

    #[test]
    fn out_of_band_redirect_is_sent_unchanged_to_the_authorize_and_token_requests() {
        let s = ExchangeSettings {
            cloud: crate::exchange::settings::ExchangeCloud::Custom,
            custom_login_url: "https://login.example.org".into(),
            custom_graph_url: "https://graph.example.org".into(),
            ..ExchangeSettings::default()
        };
        assert_eq!(s.sign_in, ExchangeSignIn::Manual);
        s.validate().unwrap();
        let request = build_authorization_request(&s, s.redirect_uri(), None, None).unwrap();
        let url = url::Url::parse(&request.url).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["redirect_uri"], "urn:ietf:wg:oauth:2.0:oob");
        assert_eq!(query["client_id"], "d3590ed6-52b3-4102-aeff-aad2292ab01c");
        assert_eq!(request.redirect_uri, "urn:ietf:wg:oauth:2.0:oob");
    }

    #[test]
    fn a_shortened_pasted_address_is_refused_with_advice() {
        let error = code_from_pasted(
            "urn:ietf:wg:oauth:2.0:oob?code=1.AR\u{2026}xyz&state=s",
            "s",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("shortened"), "{error}");
    }

    #[test]
    fn pasted_sign_in_accepts_a_code_an_address_and_an_address_without_scheme() {
        let address = "https://login.microsoftonline.com/common/oauth2/nativeclient?code=0.AbC-d_1&session_state=s&state=xyz";
        assert_eq!(code_from_pasted(address, "xyz").unwrap(), "0.AbC-d_1");
        assert_eq!(
            code_from_pasted(&address["https://".len()..], "xyz").unwrap(),
            "0.AbC-d_1"
        );
        assert_eq!(
            code_from_pasted(
                "https://login.microsoftonline.com/common/oauth2/nativeclient#code=Q1&state=xyz",
                "xyz"
            )
            .unwrap(),
            "Q1"
        );
        // A bare code is taken as it is; an address from another attempt is not.
        assert_eq!(code_from_pasted("0.AbC-d_1", "xyz").unwrap(), "0.AbC-d_1");
        assert!(code_from_pasted(address, "other").is_err());
        // The out-of-band page shows only "code=…&session_state=…".
        assert_eq!(
            code_from_pasted("Success code=0.AbC-d_1&session_state=s", "xyz").unwrap(),
            "0.AbC-d_1"
        );
        assert_eq!(
            code_from_pasted("?code=0.AbC-d_1&session_state=s", "xyz").unwrap(),
            "0.AbC-d_1"
        );
        assert_eq!(
            code_from_pasted(
                "urn:ietf:wg:oauth:2.0:oob?code=0.AbC-d_1&session_state=s",
                "xyz"
            )
            .unwrap(),
            "0.AbC-d_1"
        );
        assert!(code_from_pasted("code=&session_state=s", "xyz").is_err());
        assert!(code_from_pasted("0.AbC&x=1", "xyz").is_err());
    }

    #[tokio::test]
    async fn the_token_request_repeats_the_out_of_band_redirect() {
        let server = FakeServer::start(|request| {
            let body = request.body_text();
            assert!(
                body.contains("redirect_uri=urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob"),
                "{body}"
            );
            assert!(
                body.contains("client_id=d3590ed6-52b3-4102-aeff-aad2292ab01c"),
                "{body}"
            );
            Reply::json(
                200,
                r#"{"access_token":"A","refresh_token":"R","expires_in":3599}"#,
            )
        })
        .await;
        let s = ExchangeSettings {
            cloud: crate::exchange::settings::ExchangeCloud::Custom,
            custom_login_url: server.url(),
            custom_graph_url: format!("{}/graph", server.url()),
            ..ExchangeSettings::default()
        };
        let request = build_authorization_request(&s, s.redirect_uri(), None, None).unwrap();
        let code = code_from_pasted("thecode", &request.state).unwrap();
        exchange_code(&s, &code, &request.verifier, &request.redirect_uri)
            .await
            .unwrap();
    }

    #[test]
    fn pkce_challenge_matches_the_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pasted_redirects_are_checked_against_the_state() {
        assert_eq!(
            code_from_pasted("http://localhost/?code=abc&state=xyz", "xyz").unwrap(),
            "abc"
        );
        assert!(code_from_pasted("http://localhost/?code=abc&state=other", "xyz").is_err());
        assert!(code_from_pasted("http://localhost/?state=xyz", "xyz").is_err());
        assert_eq!(code_from_pasted("  rawcode  ", "xyz").unwrap(), "rawcode");
        assert!(code_from_pasted("two words", "xyz").is_err());
        assert!(code_from_pasted("", "xyz").is_err());
        let error = code_from_pasted(
            "http://localhost/?error=access_denied&error_description=AADSTS65001%3A+consent&state=xyz",
            "xyz",
        )
        .unwrap_err();
        assert!(error.to_string().contains("AADSTS65001"));
    }

    #[test]
    fn aadsts_errors_get_actionable_text_without_echoing_secrets() {
        let consent = explain_token_error(
            r#"{"error":"invalid_grant","error_description":"AADSTS65001: The user or administrator has not consented. Trace ID: abc\r\nCorrelation ID: def"}"#,
        );
        let text = consent.to_string();
        assert!(
            text.contains("AADSTS65001") && text.contains("administrator must approve"),
            "{text}"
        );
        assert!(!text.contains("Trace ID"));
        let unknown_app = explain_token_error(
            r#"{"error":"unauthorized_client","error_description":"AADSTS700016: Application not found"}"#,
        )
        .to_string();
        assert!(unknown_app.contains("client) ID was not found"));
        let blocked = explain_token_error(
            r#"{"error":"interaction_required","error_description":"AADSTS53003: Access blocked"}"#,
        )
        .to_string();
        assert!(blocked.contains("conditional access"));
        // A revoked or expired grant asks for a new sign-in.
        assert!(matches!(
            explain_token_error(
                r#"{"error":"invalid_grant","error_description":"AADSTS70008: The refresh token has expired"}"#
            ),
            CoreError::NeedsReauth
        ));
        // While signing in, the same refusal keeps Microsoft's description.
        let refused = explain_sign_in_error(
            r#"{"error":"invalid_grant","error_description":"AADSTS54005: The code was already redeemed.\r\nTrace ID: x"}"#,
        )
        .to_string();
        assert!(refused.contains("invalid_grant") && refused.contains("already redeemed"));
        assert!(!refused.contains("Trace ID"));
        let plain = explain_token_error("not json").to_string();
        assert!(plain.starts_with("auth failed: Microsoft sign-in failed"));
    }

    #[test]
    fn token_debug_output_hides_secrets() {
        let tokens = TokenSet {
            access_token: "SECRET-ACCESS".into(),
            refresh_token: Some("SECRET-REFRESH".into()),
            expires_in: Some(3600),
            scope: None,
        };
        let text = format!("{tokens:?}");
        assert!(!text.contains("SECRET"));
    }

    #[tokio::test]
    async fn code_exchange_and_refresh_talk_to_the_configured_token_endpoint() {
        let server = FakeServer::start(|request| {
            assert_eq!(request.path, "/common/oauth2/v2.0/token");
            let body = request.body_text();
            if body.contains("grant_type=authorization_code") {
                assert!(body.contains("code_verifier=verif"));
                assert!(body.contains("code=thecode"));
                Reply::json(
                    200,
                    r#"{"access_token":"A1","refresh_token":"R1","expires_in":3599}"#,
                )
            } else {
                assert!(body.contains("refresh_token=R1"));
                Reply::json(
                    200,
                    r#"{"access_token":"A2","refresh_token":"R2","expires_in":3599}"#,
                )
            }
        })
        .await;
        let s = settings(&server.url());
        let first = exchange_code(&s, "thecode", "verif", "http://localhost:1")
            .await
            .unwrap();
        assert_eq!(first.access_token, "A1");
        let second = refresh_tokens(&s, "R1").await.unwrap();
        assert_eq!(second.refresh_token.as_deref(), Some("R2"));
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn code_exchange_without_refresh_token_is_an_error() {
        let server =
            FakeServer::start(|_| Reply::json(200, r#"{"access_token":"A1","expires_in":3599}"#))
                .await;
        let error = exchange_code(&settings(&server.url()), "c", "v", "http://localhost:1")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("offline refresh token"));
    }

    #[tokio::test]
    async fn token_endpoint_errors_surface_as_readable_auth_errors() {
        let server = FakeServer::start(|_| {
            Reply::json(400, r#"{"error":"invalid_client","error_description":"AADSTS7000218: The request body must contain client_assertion or client_secret"}"#)
        })
        .await;
        let error = refresh_tokens(&settings(&server.url()), "R")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Allow public client flows"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn device_code_flow_polls_until_the_user_finishes() {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = polls.clone();
        let server = FakeServer::start(move |request| {
            if request.path.ends_with("/devicecode") {
                return Reply::json(
                    200,
                    r#"{"device_code":"DC","user_code":"ABCD-EFGH","verification_uri":"http://127.0.0.1/devicelogin","expires_in":900,"interval":1}"#,
                );
            }
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match n {
                0 => Reply::json(400, r#"{"error":"authorization_pending"}"#),
                _ => Reply::json(200, r#"{"access_token":"A","refresh_token":"R","expires_in":3600}"#),
            }
        })
        .await;
        let s = settings(&server.url());
        let device = start_device_code(&s).await.unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        let tokens = poll_device_code(&s, &device, Some(Duration::from_millis(5)))
            .await
            .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("R"));
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn declined_device_code_stops_polling() {
        let server = FakeServer::start(|request| {
            if request.path.ends_with("/devicecode") {
                return Reply::json(200, r#"{"device_code":"DC","user_code":"U","verification_uri":"http://127.0.0.1/x","expires_in":900,"interval":1}"#);
            }
            Reply::json(400, r#"{"error":"authorization_declined"}"#)
        })
        .await;
        let s = settings(&server.url());
        let device = start_device_code(&s).await.unwrap();
        let error = poll_device_code(&s, &device, Some(Duration::from_millis(1)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("declined"));
    }

    #[cfg(desktop)]
    #[tokio::test]
    async fn access_tokens_refresh_from_the_store_and_rotate_the_refresh_token() {
        use crate::accounts::secrets::DevelopmentFileCredentialStore;
        let directory = tempfile::tempdir().unwrap();
        let store: CredentialStoreHandle = Arc::new(DevelopmentFileCredentialStore::new(
            directory.path().join("credentials.json"),
        ));
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let server = FakeServer::start(move |request| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let body = request.body_text();
            if n == 0 {
                assert!(body.contains("refresh_token=R0"));
                Reply::json(
                    200,
                    r#"{"access_token":"A1","refresh_token":"R1","expires_in":3600}"#,
                )
            } else {
                assert!(
                    body.contains("refresh_token=R1"),
                    "rotated token must be used: {body}"
                );
                Reply::json(200, r#"{"access_token":"A2","expires_in":3600}"#)
            }
        })
        .await;
        store.store(7, Slot::RefreshToken, "R0").unwrap();
        let tokens = ExchangeTokens::new(7, settings(&server.url()), store.clone());
        assert_eq!(tokens.access_token().await.unwrap(), "A1");
        // Cached: no second request.
        assert_eq!(tokens.access_token().await.unwrap(), "A1");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(store.load(7, Slot::RefreshToken).unwrap(), "R1");
        tokens.invalidate().await;
        assert_eq!(tokens.access_token().await.unwrap(), "A2");
    }

    #[cfg(desktop)]
    #[tokio::test]
    async fn missing_refresh_token_asks_for_a_new_sign_in() {
        use crate::accounts::secrets::DevelopmentFileCredentialStore;
        let directory = tempfile::tempdir().unwrap();
        let store: CredentialStoreHandle = Arc::new(DevelopmentFileCredentialStore::new(
            directory.path().join("credentials.json"),
        ));
        let tokens = ExchangeTokens::new(9, settings("http://127.0.0.1:9"), store);
        assert!(matches!(
            tokens.access_token().await,
            Err(CoreError::NeedsReauth)
        ));
    }

    fn query_of(url: &str) -> std::collections::HashMap<String, String> {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn the_resource_authorize_url_names_the_api_and_carries_no_scope() {
        let s = ExchangeSettings {
            endpoint: ExchangeEndpoint::Resource,
            ..settings("https://login.example.org")
        };
        let request =
            build_authorization_request(&s, "urn:ietf:wg:oauth:2.0:oob", None, None).unwrap();
        let url = url::Url::parse(&request.url).unwrap();
        assert_eq!(url.path(), "/common/oauth2/authorize");
        let query = query_of(&request.url);
        assert_eq!(query["resource"], "https://login.example.org/graph");
        assert!(!query.contains_key("scope"));
        assert_eq!(query["redirect_uri"], "urn:ietf:wg:oauth:2.0:oob");
        assert!(!query.contains_key("code_challenge"));
    }

    #[test]
    fn the_built_in_client_defaults_to_the_resource_endpoint_an_own_client_to_scopes() {
        let built_in = ExchangeSettings::default();
        assert!(built_in.uses_resource_endpoint());
        let own = settings("https://login.example.org");
        assert!(!own.uses_resource_endpoint());
        // An explicit choice wins either way.
        let forced_scopes = ExchangeSettings {
            endpoint: ExchangeEndpoint::Scopes,
            ..ExchangeSettings::default()
        };
        assert!(!forced_scopes.uses_resource_endpoint());
        let forced_resource = ExchangeSettings {
            endpoint: ExchangeEndpoint::Resource,
            ..own
        };
        assert!(forced_resource.uses_resource_endpoint());
        // The scopes style still talks to the v2.0 endpoints.
        let url = build_authorization_request(&forced_scopes, "http://localhost:1", None, None)
            .unwrap()
            .url;
        assert!(url.contains("/oauth2/v2.0/authorize"), "{url}");
        assert!(query_of(&url).contains_key("scope"));
        assert!(!query_of(&url).contains_key("resource"));
    }

    #[tokio::test]
    async fn the_resource_exchange_and_refresh_send_the_resource_and_the_same_redirect() {
        let server = FakeServer::start(|request| {
            assert_eq!(request.path, "/common/oauth2/token");
            let form = request.body_text();
            assert!(form.contains("resource=http%3A%2F%2F127.0.0.1"), "{form}");
            assert!(!form.contains("scope="), "{form}");
            if form.contains("grant_type=authorization_code") {
                assert!(form.contains("redirect_uri=urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob"));
                Reply::json(
                    200,
                    r#"{"access_token":"A1","refresh_token":"R1","expires_in":"3599","scope":"Mail.ReadWrite Mail.Send User.Read","resource":"x"}"#,
                )
            } else {
                assert!(form.contains("refresh_token=R1"));
                Reply::json(
                    200,
                    r#"{"access_token":"A2","refresh_token":"R2","expires_in":"3599"}"#,
                )
            }
        })
        .await;
        let s = ExchangeSettings {
            endpoint: ExchangeEndpoint::Resource,
            ..settings(&server.url())
        };
        let request = build_authorization_request(&s, s.redirect_uri(), None, None).unwrap();
        let first = exchange_code(&s, "thecode", &request.verifier, &request.redirect_uri)
            .await
            .unwrap();
        assert_eq!(first.access_token, "A1");
        // A string `expires_in` is read like a number.
        assert_eq!(first.expires_in, Some(3599));
        assert!(first.scope.as_deref().unwrap().contains("Mail.Send"));
        let second = refresh_tokens(&s, "R1").await.unwrap();
        assert_eq!(second.refresh_token.as_deref(), Some("R2"));
    }

    #[tokio::test]
    async fn a_resource_token_without_mail_access_is_refused_at_sign_in() {
        let server = FakeServer::start(|_| {
            Reply::json(
                200,
                r#"{"access_token":"A","refresh_token":"R","expires_in":"3600","scope":"User.Read openid"}"#,
            )
        })
        .await;
        let s = ExchangeSettings {
            endpoint: ExchangeEndpoint::Resource,
            ..settings(&server.url())
        };
        let error = exchange_code(&s, "c", "v", "urn:ietf:wg:oauth:2.0:oob")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not allowed to read and write mail"),
            "{error}"
        );
        // A token without a scope list is not judged: the sync will tell.
        let server = FakeServer::start(|_| {
            Reply::json(
                200,
                r#"{"access_token":"A","refresh_token":"R","expires_in":3600}"#,
            )
        })
        .await;
        let s = ExchangeSettings {
            endpoint: ExchangeEndpoint::Resource,
            ..settings(&server.url())
        };
        exchange_code(&s, "c", "v", "urn:ietf:wg:oauth:2.0:oob")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_resource_device_code_uses_the_classic_endpoints() {
        let server = FakeServer::start(|request| {
            let form = request.body_text();
            if request.path == "/common/oauth2/devicecode" {
                assert!(form.contains("resource="), "{form}");
                assert!(!form.contains("scope="), "{form}");
                Reply::json(
                    200,
                    r#"{"device_code":"DC","user_code":"ABCD-EFGH","verification_url":"http://127.0.0.1/devicelogin","expires_in":"900","interval":"1","message":"m"}"#,
                )
            } else {
                assert_eq!(request.path, "/common/oauth2/token");
                // The classic endpoint calls the device code `code`.
                assert!(form.contains("code=DC"), "{form}");
                assert!(!form.contains("device_code="), "{form}");
                assert!(form.contains("resource="), "{form}");
                Reply::json(
                    200,
                    r#"{"access_token":"A","refresh_token":"R","expires_in":"3600","scope":"Mail.ReadWrite"}"#,
                )
            }
        })
        .await;
        let s = ExchangeSettings {
            endpoint: ExchangeEndpoint::Resource,
            sign_in: ExchangeSignIn::DeviceCode,
            ..settings(&server.url())
        };
        let device = start_device_code(&s).await.unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        assert_eq!((device.expires_in, device.interval), (900, 1));
        let tokens = poll_device_code(&s, &device, Some(Duration::from_millis(5)))
            .await
            .unwrap();
        assert_eq!(tokens.access_token, "A");
    }

    #[test]
    fn classic_endpoint_errors_have_clear_text() {
        let text = explain_token_error(
            r#"{"error":"invalid_request","error_description":"AADSTS65002: Consent between first party application is required"}"#,
        )
        .to_string();
        assert!(text.contains("Resource sign-in style"), "{text}");
    }
}
