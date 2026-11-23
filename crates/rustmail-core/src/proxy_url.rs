//! The account's proxy setting as one URL for the HTTP clients (Exchange,
//! CalDAV, CardDAV, OAuth token requests). IMAP and SMTP dial the proxy
//! themselves (`imap/mod.rs`).

use crate::accounts::secrets::{self, CredentialStoreHandle};
use crate::db::Db;
use crate::error::{CoreError, Result};
use crate::models::{MailConnectionSettings, ProxyKind};

/// An HTTP (CONNECT) proxy, or a SOCKS5 proxy that resolves host names itself
/// (`socks5h`). A proxy login, when set, travels in the URL's user info.
/// `None` when the account has no proxy.
pub(crate) fn for_connection(connection: &MailConnectionSettings) -> Result<Option<String>> {
    let scheme = match connection.proxy {
        ProxyKind::None => return Ok(None),
        ProxyKind::Http => "http",
        ProxyKind::Socks5 => "socks5h",
    };
    let host = connection.proxy_host.trim();
    let mut url = url::Url::parse(&format!("{scheme}://{host}:{}", connection.proxy_port))
        .ok()
        .filter(|url| {
            url.host_str().is_some()
                && !host.is_empty()
                && connection.proxy_port != 0
                && url.username().is_empty()
        })
        .ok_or_else(|| {
            CoreError::Other("The proxy host or port of this account is not valid.".into())
        })?;
    if !connection.proxy_username.is_empty() {
        let login = url.set_username(&connection.proxy_username).is_ok()
            && url
                .set_password(Some(connection.proxy_password.expose()))
                .is_ok();
        if !login {
            return Err(CoreError::Other(
                "The proxy login of this account is not valid.".into(),
            ));
        }
    }
    Ok(Some(url.to_string()))
}

/// [`for_connection`] for a stored account, with the proxy password loaded
/// from the credential store.
pub(crate) async fn for_account(
    db: &Db,
    credentials: &CredentialStoreHandle,
    account_id: i64,
) -> Result<Option<String>> {
    let Some(config) = db
        .read(move |conn| crate::db::repo::accounts::get_config(conn, account_id))
        .await?
    else {
        return Ok(None);
    };
    let config = secrets::hydrate_account(credentials, &config).await;
    for_connection(&config.settings.connection)
}

/// The proxy for sign-ins that start before an account exists (Gmail and
/// Outlook add-account, Microsoft 365 sign-in without its own proxy). Set
/// from the settings when they load or change.
static SIGN_IN_PROXY: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

pub(crate) fn set_sign_in_proxy(value: &str) {
    let proxy = validate_sign_in_proxy(value)
        .ok()
        .filter(|url| !url.is_empty());
    if let Ok(mut slot) = SIGN_IN_PROXY.write() {
        *slot = proxy;
    }
}

pub(crate) fn sign_in_proxy() -> Option<String> {
    SIGN_IN_PROXY.read().ok().and_then(|slot| slot.clone())
}

/// Run a sign-in future through the sign-in proxy, when one is set.
pub(crate) async fn with_sign_in_proxy<F: std::future::Future>(future: F) -> F::Output {
    match sign_in_proxy() {
        Some(url) => scoped(Some(url), future).await,
        None => future.await,
    }
}

/// Check the setting of the sign-in proxy and return it in the form the HTTP
/// clients take: empty for none, otherwise `http://host:port` or
/// `socks5h://host:port` (`socks5` is read as `socks5h`, so the proxy resolves
/// names). A login in the URL is refused: this setting is stored with the
/// other settings, not in the credential store.
pub fn validate_sign_in_proxy(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let invalid = || {
        CoreError::Other(
            "The sign-in proxy must look like http://host:port or socks5://host:port.".into(),
        )
    };
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    let scheme = match url.scheme() {
        "http" => "http",
        "socks5" | "socks5h" => "socks5h",
        _ => return Err(invalid()),
    };
    if url.host_str().is_none_or(str::is_empty) || url.port_or_known_default().is_none() {
        return Err(invalid());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CoreError::Other(
            "The sign-in proxy cannot carry a login. Use a proxy that needs none.".into(),
        ));
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid());
    }
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or_default();
    Ok(if host.contains(':') {
        format!("{scheme}://[{}]:{port}", host.trim_matches(['[', ']']))
    } else {
        format!("{scheme}://{host}:{port}")
    })
}

tokio::task_local! {
    static SCOPED: Option<String>;
}

/// Run `future` with the account's proxy URL as the proxy of every HTTP
/// client built through [`current`] inside it (Graph and Google calendar
/// APIs, OAuth requests). The scope follows the awaited future only, not
/// tasks it spawns.
pub(crate) async fn scoped<F: std::future::Future>(proxy: Option<String>, future: F) -> F::Output {
    SCOPED.scope(proxy, future).await
}

/// The proxy URL set by the innermost [`scoped`] call, if any.
pub(crate) fn current() -> Option<String> {
    SCOPED.try_with(Clone::clone).ok().flatten()
}

/// `builder` with the scoped proxy applied, when there is one.
pub(crate) fn apply_scoped(builder: reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder> {
    match current() {
        Some(url) => Ok(builder.proxy(
            reqwest::Proxy::all(url)
                .map_err(|error| CoreError::Network(format!("proxy setting: {error}")))?,
        )),
        None => Ok(builder),
    }
}

#[cfg(test)]
pub(crate) mod test_proxy {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A plain-HTTP proxy that records the head of every request it gets
    /// (absolute-form request line and headers) and answers `reply_body`
    /// with status 200.
    pub(crate) async fn http_proxy(reply_body: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&heads);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let record = Arc::clone(&record);
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => seen.extend_from_slice(&chunk[..read]),
                        }
                    }
                    record
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&seen).into_owned());
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
                        reply_body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (port, heads)
    }

    pub(crate) fn basic(user: &str, password: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    }
}

#[cfg(test)]
mod tests {
    use super::test_proxy;
    use super::*;
    use crate::models::SecretString;

    fn connection(kind: ProxyKind, user: &str) -> MailConnectionSettings {
        MailConnectionSettings {
            proxy: kind,
            proxy_host: "proxy.example.org".into(),
            proxy_port: 1080,
            proxy_username: user.into(),
            proxy_password: SecretString::new("p@ss/word"),
            ..MailConnectionSettings::default()
        }
    }

    #[test]
    fn the_proxy_setting_becomes_one_url() {
        assert_eq!(
            for_connection(&connection(ProxyKind::None, "")).unwrap(),
            None
        );
        assert_eq!(
            for_connection(&connection(ProxyKind::Http, "")).unwrap(),
            Some("http://proxy.example.org:1080/".into())
        );
        assert_eq!(
            for_connection(&connection(ProxyKind::Socks5, "ben")).unwrap(),
            Some("socks5h://ben:p%40ss%2Fword@proxy.example.org:1080".into())
        );
        let mut bad = connection(ProxyKind::Http, "");
        bad.proxy_port = 0;
        assert!(for_connection(&bad).is_err());
    }

    #[test]
    fn the_sign_in_proxy_setting_is_checked_and_normalised() {
        assert_eq!(validate_sign_in_proxy("  ").unwrap(), "");
        assert_eq!(
            validate_sign_in_proxy(" http://proxy.example.org:3128 ").unwrap(),
            "http://proxy.example.org:3128"
        );
        assert_eq!(
            validate_sign_in_proxy("http://proxy.example.org").unwrap(),
            "http://proxy.example.org:80"
        );
        assert_eq!(
            validate_sign_in_proxy("socks5://127.0.0.1:9050").unwrap(),
            "socks5h://127.0.0.1:9050"
        );
        assert_eq!(
            validate_sign_in_proxy("socks5h://[::1]:1080").unwrap(),
            "socks5h://[::1]:1080"
        );
        for bad in [
            "proxy.example.org:3128",
            "ftp://proxy.example.org:21",
            "https://proxy.example.org:443",
            "socks5://proxy.example.org",
            "http://ben:secret@proxy.example.org:3128",
            "http://proxy.example.org:3128/path",
            "http://:3128",
        ] {
            assert!(validate_sign_in_proxy(bad).is_err(), "{bad}");
        }
    }

    /// The recorded head of the first request the fake proxy saw: HTTPS
    /// targets arrive as `CONNECT host:443` with the proxy login.
    fn assert_connect(heads: &std::sync::Mutex<Vec<String>>, target: &str) {
        let heads = heads.lock().unwrap();
        let head = heads.first().expect("the proxy saw no request");
        assert!(head.starts_with(&format!("CONNECT {target} ")), "{head}");
        let login = format!(
            "proxy-authorization: Basic {}",
            test_proxy::basic("ben", "sesame")
        );
        assert!(
            head.to_lowercase().contains(&login.to_lowercase()),
            "{head}"
        );
    }

    #[tokio::test]
    async fn graph_calls_inside_a_scope_go_through_the_proxy() {
        let (port, heads) = test_proxy::http_proxy("{}").await;
        let proxy = format!("http://ben:sesame@127.0.0.1:{port}/");
        // The fake proxy cannot speak TLS, so the call fails after CONNECT.
        let result = scoped(Some(proxy), crate::graph::list_calendars("token")).await;
        assert!(result.is_err());
        assert_connect(&heads, "graph.microsoft.com:443");
    }

    #[tokio::test]
    async fn google_calendar_calls_inside_a_scope_go_through_the_proxy() {
        let (port, heads) = test_proxy::http_proxy("{}").await;
        let proxy = format!("http://ben:sesame@127.0.0.1:{port}/");
        let result = scoped(Some(proxy), crate::googlecal::list_calendars("token")).await;
        assert!(result.is_err());
        assert_connect(&heads, "www.googleapis.com:443");
    }

    #[tokio::test]
    async fn oauth_token_requests_inside_a_scope_go_through_the_proxy() {
        let (port, heads) = test_proxy::http_proxy("{}").await;
        let proxy = format!("http://ben:sesame@127.0.0.1:{port}/");
        let form = [("grant_type".to_owned(), "authorization_code".to_owned())];
        let result = scoped(
            Some(proxy),
            crate::oauth::tokens::post_form("https://oauth2.googleapis.com/token", &form),
        )
        .await;
        assert!(result.is_err());
        assert_connect(&heads, "oauth2.googleapis.com:443");
    }

    #[tokio::test]
    async fn the_scope_ends_with_the_call_and_a_missing_proxy_is_no_proxy() {
        assert_eq!(current(), None);
        assert_eq!(scoped(None, async { current() }).await, None);
        assert_eq!(
            scoped(Some("http://p:1/".into()), async { current() }).await,
            Some("http://p:1/".to_owned())
        );
        assert_eq!(current(), None);
    }
}
