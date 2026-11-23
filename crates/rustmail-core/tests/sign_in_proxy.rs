use rustmail_core::{Core, config::Paths, exchange::ExchangeSettings};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;

/// A listener that records the first bytes of every connection. It never
/// answers, so a request through it fails, but the CONNECT line is kept.
async fn recording_proxy() -> (u16, Arc<Mutex<Vec<String>>>) {
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
                let mut buffer = [0u8; 2048];
                if let Ok(read) = stream.read(&mut buffer).await {
                    record
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
                }
                // Hold the connection until the client gives up.
                let _ = stream.read(&mut buffer).await;
            });
        }
    });
    (port, heads)
}

// One test: the sign-in proxy is process-wide, like the OAuth keys.
#[tokio::test]
async fn the_sign_in_proxy_is_checked_saved_and_used_before_an_account_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let core = Core::start(Paths::for_tests(tmp.path())).await.unwrap();
    let (port, heads) = recording_proxy().await;

    // A bad proxy is refused and nothing is written.
    for bad in [
        "proxy.example.org:3128",
        "http://user:secret@proxy.example.org:3128",
        "ftp://proxy.example.org:21",
    ] {
        assert!(
            core.set_oauth_apps_with_proxy(None, None, Some(bad.into()))
                .await
                .is_err(),
            "{bad}"
        );
    }
    assert_eq!(core.get_settings().await.unwrap().sign_in_proxy, "");
    let mut settings = core.get_settings().await.unwrap();
    settings.sign_in_proxy = "gopher://x:1".into();
    assert!(core.set_settings(settings).await.is_err());

    // Without a proxy the device-code request does not touch the listener.
    let exchange = ExchangeSettings {
        enabled: true,
        client_id: "facd6cff-a294-4415-b59f-c5b01937d7bd".into(),
        ..ExchangeSettings::default()
    };
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        core.exchange_begin_device_code(&exchange),
    )
    .await;
    assert!(heads.lock().unwrap().is_empty());

    // Saved (and normalised), the proxy is used by the sign-in.
    let saved = core
        .set_oauth_apps_with_proxy(None, None, Some(format!(" socks5://127.0.0.1:{port} ")))
        .await
        .unwrap();
    assert_eq!(saved.sign_in_proxy, format!("socks5h://127.0.0.1:{port}"));
    let saved = core
        .set_oauth_apps_with_proxy(None, None, Some(format!("http://127.0.0.1:{port}")))
        .await
        .unwrap();
    assert_eq!(saved.sign_in_proxy, format!("http://127.0.0.1:{port}"));
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.exchange_begin_device_code(&exchange),
    )
    .await;
    let seen = heads.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|head| head.starts_with("CONNECT login.microsoftonline.com:443 ")),
        "{seen:?}"
    );

    // An account's own proxy setting still wins over the sign-in proxy: here
    // the request goes to a different (closed) address, not to the listener.
    let before = heads.lock().unwrap().len();
    let own = ExchangeSettings {
        proxy_url: "http://127.0.0.1:9".into(),
        ..exchange
    };
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.exchange_begin_device_code(&own),
    )
    .await;
    assert_eq!(heads.lock().unwrap().len(), before);

    // An empty value clears it.
    let saved = core
        .set_oauth_apps_with_proxy(None, None, Some(String::new()))
        .await
        .unwrap();
    assert_eq!(saved.sign_in_proxy, "");
    // Leaving the proxy argument out keeps what is saved.
    core.set_oauth_apps_with_proxy(None, None, Some(format!("http://127.0.0.1:{port}")))
        .await
        .unwrap();
    let kept = core
        .set_oauth_apps(Some((String::new(), String::new())), None)
        .await
        .unwrap();
    assert_eq!(kept.sign_in_proxy, format!("http://127.0.0.1:{port}"));
}
