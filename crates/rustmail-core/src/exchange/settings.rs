//! Per-account settings of an Exchange Online (Microsoft 365) account that
//! talks to Microsoft Graph directly: application registration, tenant,
//! redirect URI, scopes, national cloud and sign-in method. Everything here is
//! pure (no I/O) so the setup form and the engine share one validation path.
//!
//! The values are public configuration, not secrets: tokens live only in the
//! credential store (`accounts::secrets`), never in these settings.

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};

/// Scopes requested when the account does not override them.
pub const DEFAULT_SCOPES: &str = "offline_access Mail.ReadWrite Mail.Send Calendars.ReadWrite Contacts.ReadWrite MailboxSettings.Read";
/// The same set for a delegated (shared) mailbox, which needs the `.Shared`
/// variants of the mail, calendar and contact permissions.
pub const DEFAULT_SHARED_SCOPES: &str = "offline_access Mail.ReadWrite.Shared Mail.Send.Shared Calendars.ReadWrite.Shared Contacts.ReadWrite.Shared MailboxSettings.Read";
/// Default tenant: any work or school account (and personal ones).
pub const DEFAULT_TENANT: &str = "common";
/// Application (client) ID used when the account does not set its own: the
/// public client ID of Microsoft Office. It is Microsoft's registration, not
/// Rustmail's: it appears as Microsoft Office in the tenant's sign-in logs, an
/// administrator can block it, and Microsoft can change what it allows. An
/// own app registration (the client ID setting) is the supported alternative.
pub const DEFAULT_CLIENT_ID: &str = "d3590ed6-52b3-4102-aeff-aad2292ab01c";
/// Default redirect URI of the Office client: out-of-band, nothing listens
/// for it. Microsoft's sign-in ends on a page whose address carries the code,
/// which the user pastes back (sign-in method "manual"); it cannot be used for
/// the browser method, which needs a loopback redirect to Rustmail.
pub const DEFAULT_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";
/// Redirect URI of the browser sign-in: a loopback listener on a free port.
pub const LOOPBACK_REDIRECT_URI: &str = "http://localhost";

const MAX_SETTING_BYTES: usize = 2048;
const MAX_SCOPES: usize = 32;

/// National cloud: sets the login host and the Graph host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExchangeCloud {
    #[default]
    Worldwide,
    /// US Government (GCC High / DoD use the `.us` hosts).
    UsGovernment,
    /// China, operated by 21Vianet.
    China,
    /// Germany (legacy national cloud, kept for tenants that still use it).
    Germany,
    /// Login and Graph URLs typed by the user (sovereign or test clouds).
    Custom,
}

impl ExchangeCloud {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Worldwide => "worldwide",
            Self::UsGovernment => "us-government",
            Self::China => "china",
            Self::Germany => "germany",
            Self::Custom => "custom",
        }
    }

    pub fn from_str_lossy(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "us-government" | "usgovernment" | "us" => Self::UsGovernment,
            "china" | "21vianet" => Self::China,
            "germany" => Self::Germany,
            "custom" => Self::Custom,
            _ => Self::Worldwide,
        }
    }

    fn login_base(self) -> &'static str {
        match self {
            Self::Worldwide | Self::Custom => "https://login.microsoftonline.com",
            Self::UsGovernment => "https://login.microsoftonline.us",
            Self::China => "https://login.partner.microsoftonline.cn",
            Self::Germany => "https://login.microsoftonline.de",
        }
    }

    fn graph_root(self) -> &'static str {
        match self {
            Self::Worldwide | Self::Custom => "https://graph.microsoft.com",
            Self::UsGovernment => "https://graph.microsoft.us",
            Self::China => "https://microsoftgraph.chinacloudapi.cn",
            Self::Germany => "https://graph.microsoft.de",
        }
    }
}

/// How the user signs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExchangeSignIn {
    /// System browser and a loopback redirect (authorization code + PKCE).
    #[default]
    Browser,
    /// Device code: sign in on any device with a short code (headless hosts).
    DeviceCode,
    /// The user pastes the redirected address (or code) from the browser,
    /// for locked-down hosts and the native-client redirect URI.
    Manual,
}

impl ExchangeSignIn {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::DeviceCode => "device-code",
            Self::Manual => "manual",
        }
    }

    pub fn from_str_lossy(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "device-code" | "devicecode" | "device" => Self::DeviceCode,
            "manual" | "paste" => Self::Manual,
            _ => Self::Browser,
        }
    }
}

/// Which Microsoft sign-in endpoints the account uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExchangeEndpoint {
    /// Legacy resource sign-in with the built-in client ID, scopes with an
    /// own client ID.
    #[default]
    Auto,
    /// Classic v1 endpoints (`/oauth2/authorize`, `/oauth2/token`) with a
    /// `resource` and no scopes: the token carries the permissions already
    /// configured on the app registration. This is what a first-party
    /// Microsoft client such as the built-in Office one is allowed to do.
    Resource,
    /// v2.0 endpoints with an explicit scope list (dynamic consent).
    Scopes,
}

impl ExchangeEndpoint {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Resource => "resource",
            Self::Scopes => "scopes",
        }
    }

    pub fn from_str_lossy(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "resource" | "legacy" => Self::Resource,
            "scopes" | "v2" => Self::Scopes,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExchangeSettings {
    /// Marks the account as Exchange Online (the account row stores `imap`).
    pub enabled: bool,
    /// Application (client) ID; empty selects [`DEFAULT_CLIENT_ID`].
    pub client_id: String,
    /// `common`, `organizations`, `consumers`, a tenant GUID or a domain.
    pub tenant: String,
    pub redirect_uri: String,
    /// Space-separated scopes; empty selects the defaults.
    pub scopes: String,
    pub cloud: ExchangeCloud,
    /// Used only with [`ExchangeCloud::Custom`], e.g. `https://login.example`.
    pub custom_login_url: String,
    /// Used only with [`ExchangeCloud::Custom`], e.g. `https://graph.example`.
    pub custom_graph_url: String,
    pub sign_in: ExchangeSignIn,
    /// Endpoint style; an explicit choice wins over the client-ID default.
    pub endpoint: ExchangeEndpoint,
    /// Address of a shared or delegated mailbox to open instead of the
    /// signed-in user's own; empty = own mailbox.
    pub shared_mailbox: String,
    /// HTTP proxy for every request of the account (`http://host:port`),
    /// taken from the account's proxy setting at run time; never stored here.
    #[serde(skip)]
    pub proxy_url: String,
}

impl Default for ExchangeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            tenant: DEFAULT_TENANT.to_owned(),
            redirect_uri: DEFAULT_REDIRECT_URI.to_owned(),
            scopes: String::new(),
            cloud: ExchangeCloud::Worldwide,
            custom_login_url: String::new(),
            custom_graph_url: String::new(),
            sign_in: ExchangeSignIn::Manual,
            endpoint: ExchangeEndpoint::Auto,
            shared_mailbox: String::new(),
            proxy_url: String::new(),
        }
    }
}

fn setting_error(message: &str) -> CoreError {
    CoreError::Other(message.to_owned())
}

/// Accept `https` URLs, and `http` only for a loopback host (local test
/// servers); no credentials, query or fragment, no trailing path for a root.
fn checked_base_url(value: &str, what: &str) -> Result<url::Url> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_SETTING_BYTES {
        return Err(setting_error(&format!("Enter the {what} URL.")));
    }
    let url = url::Url::parse(value)
        .map_err(|_| setting_error(&format!("The {what} URL is not a valid address.")))?;
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]"));
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !scheme_ok
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(setting_error(&format!(
            "The {what} URL must be an https address without credentials, query or fragment."
        )));
    }
    Ok(url)
}

fn tenant_is_valid(tenant: &str) -> bool {
    if matches!(tenant, "common" | "organizations" | "consumers") {
        return true;
    }
    let guid = tenant.len() == 36
        && tenant.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    let domain = tenant.len() <= 253
        && tenant.contains('.')
        && tenant.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    guid || domain
}

fn mailbox_is_valid(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !domain.is_empty()
        && domain.contains('.')
        && !address.chars().any(|ch| {
            ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '\\' | '?' | '#' | '%')
        })
        && address.matches('@').count() == 1
}

impl ExchangeSettings {
    /// The proxy to use, if the account has one.
    pub fn proxy(&self) -> Option<&str> {
        Some(self.proxy_url.as_str()).filter(|url| !url.is_empty())
    }

    /// Whether `other` needs a new sign-in: any value that decides which app,
    /// tenant, cloud or permissions the stored refresh token was issued for.
    pub fn sign_in_differs(&self, other: &Self) -> bool {
        self.client_id() != other.client_id()
            || self.tenant() != other.tenant()
            || self.redirect_uri() != other.redirect_uri()
            || (!self.uses_resource_endpoint()
                && self.effective_scopes() != other.effective_scopes())
            || self.cloud != other.cloud
            || self.custom_login_url.trim() != other.custom_login_url.trim()
            || self.custom_graph_url.trim() != other.custom_graph_url.trim()
            || self.uses_resource_endpoint() != other.uses_resource_endpoint()
    }

    pub fn tenant(&self) -> &str {
        let tenant = self.tenant.trim();
        if tenant.is_empty() {
            DEFAULT_TENANT
        } else {
            tenant
        }
    }

    /// The client ID in use: the account's own, or the default.
    pub fn client_id(&self) -> &str {
        let id = self.client_id.trim();
        if id.is_empty() { DEFAULT_CLIENT_ID } else { id }
    }

    /// The account uses the built-in (Microsoft Office) client ID.
    pub fn uses_default_client_id(&self) -> bool {
        self.client_id() == DEFAULT_CLIENT_ID
    }

    /// The classic resource sign-in (v1, no scopes) is in use: chosen, or the
    /// default for the built-in client ID.
    pub fn uses_resource_endpoint(&self) -> bool {
        match self.endpoint {
            ExchangeEndpoint::Resource => true,
            ExchangeEndpoint::Scopes => false,
            ExchangeEndpoint::Auto => self.uses_default_client_id(),
        }
    }

    pub fn redirect_uri(&self) -> &str {
        let uri = self.redirect_uri.trim();
        if uri.is_empty() {
            DEFAULT_REDIRECT_URI
        } else {
            uri
        }
    }

    pub fn shared_mailbox(&self) -> Option<&str> {
        let mailbox = self.shared_mailbox.trim();
        (!mailbox.is_empty()).then_some(mailbox)
    }

    /// The configured scopes (or the defaults), deduplicated, always with
    /// `offline_access` so a refresh token is issued.
    pub fn effective_scopes(&self) -> Vec<String> {
        let source = if self.scopes.trim().is_empty() {
            if self.shared_mailbox().is_some() {
                DEFAULT_SHARED_SCOPES
            } else {
                DEFAULT_SCOPES
            }
        } else {
            self.scopes.as_str()
        };
        let mut scopes: Vec<String> = Vec::new();
        for scope in source.split_whitespace() {
            if !scopes.iter().any(|known| known == scope) {
                scopes.push(scope.to_owned());
            }
        }
        if !scopes.iter().any(|scope| scope == "offline_access") {
            scopes.insert(0, "offline_access".to_owned());
        }
        scopes
    }

    /// Login host without a trailing slash.
    pub fn login_base(&self) -> Result<String> {
        if self.cloud == ExchangeCloud::Custom {
            let url = checked_base_url(&self.custom_login_url, "login")?;
            return Ok(url.as_str().trim_end_matches('/').to_owned());
        }
        Ok(self.cloud.login_base().to_owned())
    }

    /// Graph v1.0 base URL without a trailing slash.
    pub fn graph_base(&self) -> Result<String> {
        let root = if self.cloud == ExchangeCloud::Custom {
            let url = checked_base_url(&self.custom_graph_url, "Graph")?;
            url.as_str().trim_end_matches('/').to_owned()
        } else {
            self.cloud.graph_root().to_owned()
        };
        if root.ends_with("/v1.0") {
            Ok(root)
        } else {
            Ok(format!("{root}/v1.0"))
        }
    }

    /// The Graph host the resource sign-in asks a token for (no `/v1.0`).
    pub fn resource_url(&self) -> Result<String> {
        let base = self.graph_base()?;
        Ok(base.strip_suffix("/v1.0").unwrap_or(&base).to_owned())
    }

    /// `v2.0/` in the path of the scopes endpoints, nothing for the classic
    /// resource ones.
    fn oauth_path(&self, endpoint: &str) -> Result<String> {
        let version = if self.uses_resource_endpoint() {
            ""
        } else {
            "v2.0/"
        };
        Ok(format!(
            "{}/{}/oauth2/{version}{endpoint}",
            self.login_base()?,
            self.tenant()
        ))
    }

    pub fn authorize_url(&self) -> Result<String> {
        self.oauth_path("authorize")
    }

    pub fn token_url(&self) -> Result<String> {
        self.oauth_path("token")
    }

    pub fn device_code_url(&self) -> Result<String> {
        self.oauth_path("devicecode")
    }

    /// The native-client redirect of this cloud, for the Office client IDs.
    pub fn native_client_redirect(&self) -> Result<String> {
        Ok(format!("{}/common/oauth2/nativeclient", self.login_base()?))
    }

    /// Path prefix of the mailbox: `/me` or `/users/{shared mailbox}`.
    pub fn mailbox_path(&self) -> String {
        match self.shared_mailbox() {
            Some(mailbox) => {
                let mut path = String::from("/users/");
                path.extend(url::form_urlencoded::byte_serialize(mailbox.as_bytes()));
                path
            }
            None => "/me".to_owned(),
        }
    }

    /// Validate everything the sign-in and the engine rely on.
    pub fn validate(&self) -> Result<()> {
        self.validate_sign_in_fields()?;
        self.validate_redirect()
    }

    /// The checks that do not involve the redirect address (a browser
    /// sign-in uses the loopback listener's own address).
    pub fn validate_sign_in_fields(&self) -> Result<()> {
        crate::oauth::providers::validate_client_id(
            crate::models::Provider::Microsoft,
            self.client_id(),
        )
        .map_err(|_| {
            setting_error(
                "The Application (client) ID must be a nonzero GUID; leave it empty to use the built-in one.",
            )
        })?;
        let tenant = self.tenant();
        if !tenant_is_valid(tenant) {
            return Err(setting_error(
                "The tenant must be common, organizations, consumers, a tenant ID (GUID) or a domain such as contoso.com.",
            ));
        }
        self.login_base()?;
        self.graph_base()?;
        let scopes = self.effective_scopes();
        // The resource sign-in sends no scopes, so a stale list cannot fail it.
        if !self.uses_resource_endpoint()
            && (self.scopes.len() > MAX_SETTING_BYTES
                || scopes.len() > MAX_SCOPES
                || scopes.iter().any(|scope| {
                    scope.len() > 256
                        || scope
                            .chars()
                            .any(|ch| ch.is_control() || matches!(ch, '"' | '\\'))
                }))
        {
            return Err(setting_error("The scope list contains an invalid scope."));
        }
        if let Some(mailbox) = self.shared_mailbox()
            && !mailbox_is_valid(mailbox)
        {
            return Err(setting_error(
                "Enter the shared mailbox as an email address, or leave it empty for your own mailbox.",
            ));
        }
        Ok(())
    }

    fn validate_redirect(&self) -> Result<()> {
        let redirect = self.redirect_uri();
        if redirect.len() > MAX_SETTING_BYTES {
            return Err(setting_error("The redirect URI is too long."));
        }
        if redirect == DEFAULT_REDIRECT_URI {
            // Out-of-band: the browser method needs the loopback listener.
            if self.sign_in == ExchangeSignIn::Browser {
                return Err(setting_error(
                    "The out-of-band redirect cannot return to Rustmail; choose the paste or device-code sign-in method, or a loopback redirect.",
                ));
            }
            return Ok(());
        }
        let parsed = url::Url::parse(redirect)
            .map_err(|_| setting_error("The redirect URI is not a valid address."))?;
        let loopback = parsed.scheme() == "http"
            && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
            && parsed.query().is_none()
            && parsed.fragment().is_none();
        let native = parsed.scheme() == "https"
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.path().ends_with("/oauth2/nativeclient");
        if !loopback && !native {
            return Err(setting_error(
                "The redirect URI must be urn:ietf:wg:oauth:2.0:oob, a loopback address (http://localhost) or a native-client address ending in /oauth2/nativeclient.",
            ));
        }
        if native && self.sign_in == ExchangeSignIn::Browser {
            return Err(setting_error(
                "A native-client redirect cannot return to Rustmail; choose the manual or device-code sign-in method.",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: &str = "facd6cff-a294-4415-b59f-c5b01937d7bd";

    #[test]
    fn endpoint_style_resolves_urls_and_old_settings_keep_working() {
        // Settings stored before the field existed: the built-in client
        // resolves to the resource endpoints, an own client to scopes.
        let old: ExchangeSettings = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(old.endpoint, ExchangeEndpoint::Auto);
        assert!(old.uses_resource_endpoint());
        assert_eq!(
            old.authorize_url().unwrap(),
            "https://login.microsoftonline.com/common/oauth2/authorize"
        );
        assert_eq!(
            old.token_url().unwrap(),
            "https://login.microsoftonline.com/common/oauth2/token"
        );
        assert_eq!(
            old.device_code_url().unwrap(),
            "https://login.microsoftonline.com/common/oauth2/devicecode"
        );
        assert_eq!(old.resource_url().unwrap(), "https://graph.microsoft.com");
        let own: ExchangeSettings =
            serde_json::from_str(&format!(r#"{{"clientId":"{CLIENT}"}}"#)).unwrap();
        assert!(!own.uses_resource_endpoint());
        assert_eq!(
            own.token_url().unwrap(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/token"
        );
        let chosen: ExchangeSettings = serde_json::from_str(r#"{"endpoint":"scopes"}"#).unwrap();
        assert!(!chosen.uses_resource_endpoint());
        assert_eq!(
            ExchangeEndpoint::from_str_lossy("Resource"),
            ExchangeEndpoint::Resource
        );
        assert_eq!(
            ExchangeEndpoint::from_str_lossy("nonsense"),
            ExchangeEndpoint::Auto
        );
    }

    #[test]
    fn a_stale_scope_list_does_not_fail_the_resource_sign_in() {
        let resource = ExchangeSettings {
            endpoint: ExchangeEndpoint::Resource,
            scopes: "a\"b".into(),
            ..valid()
        };
        resource.validate_sign_in_fields().unwrap();
        let scopes = ExchangeSettings {
            endpoint: ExchangeEndpoint::Scopes,
            scopes: "a\"b".into(),
            ..valid()
        };
        assert!(scopes.validate_sign_in_fields().is_err());
    }

    fn valid() -> ExchangeSettings {
        ExchangeSettings {
            enabled: true,
            client_id: CLIENT.into(),
            ..ExchangeSettings::default()
        }
    }

    #[test]
    fn defaults_validate_with_a_client_id_and_build_worldwide_urls() {
        let settings = valid();
        settings.validate().unwrap();
        assert_eq!(
            settings.authorize_url().unwrap(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize"
        );
        assert_eq!(
            settings.token_url().unwrap(),
            "https://login.microsoftonline.com/common/oauth2/v2.0/token"
        );
        assert_eq!(
            settings.graph_base().unwrap(),
            "https://graph.microsoft.com/v1.0"
        );
        assert_eq!(settings.mailbox_path(), "/me");
    }

    #[test]
    fn empty_client_id_uses_the_office_default_and_bad_ones_are_rejected() {
        let mut settings = valid();
        settings.client_id.clear();
        settings.validate().unwrap();
        assert_eq!(settings.client_id(), DEFAULT_CLIENT_ID);
        assert_eq!(DEFAULT_CLIENT_ID, "d3590ed6-52b3-4102-aeff-aad2292ab01c");
        assert!(settings.uses_default_client_id());
        settings.client_id = "  ".into();
        assert!(settings.uses_default_client_id());
        settings.client_id = CLIENT.into();
        assert_eq!(settings.client_id(), CLIENT);
        assert!(!settings.uses_default_client_id());
        settings.client_id = "00000000-0000-0000-0000-000000000000".into();
        assert!(settings.validate().is_err());
        settings.client_id = "not-a-guid".into();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn tenant_accepts_keywords_guids_and_domains_only() {
        for good in [
            "common",
            "organizations",
            "consumers",
            "contoso.com",
            "11111111-2222-3333-4444-555555555555",
        ] {
            let mut settings = valid();
            settings.tenant = good.into();
            settings
                .validate()
                .unwrap_or_else(|e| panic!("{good}: {e}"));
        }
        for bad in [
            "con toso.com",
            "contoso",
            "a/b.com",
            "-x.com",
            "x..com",
            "x.com/../y",
        ] {
            let mut settings = valid();
            settings.tenant = bad.into();
            assert!(settings.validate().is_err(), "{bad}");
        }
        // An empty tenant falls back to the default.
        let mut settings = valid();
        settings.tenant.clear();
        assert_eq!(settings.tenant(), "common");
        settings.validate().unwrap();
    }

    #[test]
    fn national_clouds_select_their_hosts() {
        let mut settings = valid();
        settings.cloud = ExchangeCloud::UsGovernment;
        assert_eq!(
            settings.token_url().unwrap(),
            "https://login.microsoftonline.us/common/oauth2/v2.0/token"
        );
        assert_eq!(
            settings.graph_base().unwrap(),
            "https://graph.microsoft.us/v1.0"
        );
        settings.cloud = ExchangeCloud::China;
        assert_eq!(
            settings.graph_base().unwrap(),
            "https://microsoftgraph.chinacloudapi.cn/v1.0"
        );
        settings.cloud = ExchangeCloud::Germany;
        assert_eq!(
            settings.graph_base().unwrap(),
            "https://graph.microsoft.de/v1.0"
        );
    }

    #[test]
    fn custom_cloud_needs_safe_urls() {
        let mut settings = valid();
        settings.cloud = ExchangeCloud::Custom;
        assert!(settings.validate().is_err());
        settings.custom_login_url = "https://login.example.org/".into();
        settings.custom_graph_url = "https://graph.example.org/v1.0".into();
        settings.validate().unwrap();
        assert_eq!(
            settings.graph_base().unwrap(),
            "https://graph.example.org/v1.0"
        );
        assert_eq!(
            settings.authorize_url().unwrap(),
            "https://login.example.org/common/oauth2/v2.0/authorize"
        );
        // Plain http is only for a loopback test server.
        settings.custom_login_url = "http://login.example.org".into();
        assert!(settings.validate().is_err());
        settings.custom_login_url = "http://127.0.0.1:8080".into();
        settings.validate().unwrap();
        settings.custom_graph_url = "https://user:pw@graph.example.org".into();
        assert!(settings.validate().is_err());
        settings.custom_graph_url = "https://graph.example.org/?x=1".into();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn redirect_uri_is_loopback_or_native_client() {
        let mut settings = valid();
        settings.sign_in = ExchangeSignIn::Browser;
        settings.redirect_uri = "http://localhost".into();
        settings.validate().unwrap();
        settings.redirect_uri = "http://localhost:8400/cb".into();
        settings.validate().unwrap();
        settings.redirect_uri = "http://evil.example.org/cb".into();
        assert!(settings.validate().is_err());
        settings.redirect_uri = "https://example.org/cb".into();
        assert!(settings.validate().is_err());
        // The native-client URI cannot be caught by the browser listener.
        settings.redirect_uri = settings.native_client_redirect().unwrap();
        assert!(settings.validate().is_err());
        settings.sign_in = ExchangeSignIn::Manual;
        settings.validate().unwrap();
    }

    #[test]
    fn defaults_are_the_office_client_with_the_out_of_band_redirect_and_paste_sign_in() {
        let settings = ExchangeSettings::default();
        assert_eq!(settings.sign_in, ExchangeSignIn::Manual);
        assert_eq!(settings.redirect_uri(), "urn:ietf:wg:oauth:2.0:oob");
        assert_eq!(settings.client_id(), "d3590ed6-52b3-4102-aeff-aad2292ab01c");
        settings.validate().unwrap();
        // Empty fields fall back to the same defaults.
        let blank = ExchangeSettings {
            redirect_uri: String::new(),
            ..ExchangeSettings::default()
        };
        assert_eq!(blank.redirect_uri(), DEFAULT_REDIRECT_URI);
    }

    #[test]
    fn the_out_of_band_redirect_is_for_paste_and_device_code_only() {
        let mut settings = ExchangeSettings {
            sign_in: ExchangeSignIn::DeviceCode,
            ..ExchangeSettings::default()
        };
        settings.validate().unwrap();
        settings.sign_in = ExchangeSignIn::Manual;
        settings.validate().unwrap();
        // The browser method needs the loopback listener, which an
        // out-of-band redirect never reaches.
        settings.sign_in = ExchangeSignIn::Browser;
        let error = settings.validate().unwrap_err().to_string();
        assert!(error.contains("out-of-band"), "{error}");
        settings.redirect_uri = LOOPBACK_REDIRECT_URI.into();
        settings.validate().unwrap();
        // Close lookalikes are not accepted as the out-of-band address.
        settings.sign_in = ExchangeSignIn::Manual;
        for bad in [
            "urn:ietf:wg:oauth:2.0:oob:auto",
            "urn:other",
            "URN:ietf:wg:oauth:2.0:oob",
        ] {
            settings.redirect_uri = bad.into();
            assert!(settings.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn scopes_default_dedupe_and_always_include_offline_access() {
        let mut settings = valid();
        let defaults = settings.effective_scopes();
        assert!(defaults.contains(&"Mail.ReadWrite".to_owned()));
        assert!(!defaults.iter().any(|scope| scope.ends_with(".Shared")));
        settings.scopes = "Mail.Read Mail.Read User.Read".into();
        assert_eq!(
            settings.effective_scopes(),
            vec!["offline_access", "Mail.Read", "User.Read"]
        );
        settings.scopes = "Mail.Read \"x".into();
        assert!(settings.validate().is_err());
        settings.scopes.clear();
        settings.shared_mailbox = "team@contoso.com".into();
        assert!(
            settings
                .effective_scopes()
                .contains(&"Mail.ReadWrite.Shared".to_owned())
        );
    }

    #[test]
    fn shared_mailbox_changes_the_path_and_is_validated() {
        let mut settings = valid();
        settings.shared_mailbox = "team@contoso.com".into();
        settings.validate().unwrap();
        assert_eq!(settings.mailbox_path(), "/users/team%40contoso.com");
        for bad in [
            "team",
            "a@b",
            "a b@c.com",
            "a/b@c.com",
            "a@b@c.com",
            "@c.com",
        ] {
            settings.shared_mailbox = bad.into();
            assert!(settings.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_round_trip_with_missing_fields_defaulting() {
        let json = r#"{"enabled":true,"clientId":"facd6cff-a294-4415-b59f-c5b01937d7bd"}"#;
        let settings: ExchangeSettings = serde_json::from_str(json).unwrap();
        assert!(settings.enabled);
        assert_eq!(settings.tenant, "common");
        assert_eq!(settings.sign_in, ExchangeSignIn::Manual);
        assert_eq!(settings.redirect_uri(), DEFAULT_REDIRECT_URI);
        let again: ExchangeSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(settings, again);
    }
}
