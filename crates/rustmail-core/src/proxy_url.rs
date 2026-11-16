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
}
