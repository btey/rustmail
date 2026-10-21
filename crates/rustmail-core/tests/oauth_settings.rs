use rustmail_core::{Core, config::Paths, models::Provider, oauth::providers};

// Keep this sequence in one test: configured OAuth registrations are global.
#[tokio::test]
async fn oauth_keys_persist_clear_and_remain_unchanged_after_failed_save() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(tmp.path());
    let core = Core::start(paths.clone()).await.unwrap();
    let defaults = providers::resolve_credentials(Provider::Gmail).ok();
    let mut settings = core.get_settings().await.unwrap();
    settings.google_client_id = "custom-google-id".into();
    settings.google_client_secret = "custom-google-secret".into();
    settings.ms_client_id = "11111111-2222-4333-8444-555555555555".into();
    core.set_settings(settings.clone()).await.unwrap();
    let saved = core
        .set_oauth_apps(
            Some((
                "  custom-google-id  ".into(),
                "  custom-google-secret  ".into(),
            )),
            Some("  11111111-2222-4333-8444-555555555555  ".into()),
        )
        .await
        .unwrap();
    assert_eq!(saved.google_client_id, "custom-google-id");
    assert_eq!(saved.ms_client_id, "11111111-2222-4333-8444-555555555555");
    assert_eq!(
        providers::resolve_credentials(Provider::Gmail).unwrap(),
        (
            "custom-google-id".into(),
            Some("custom-google-secret".into())
        )
    );
    assert_eq!(
        providers::resolve_credentials(Provider::Microsoft).unwrap(),
        ("11111111-2222-4333-8444-555555555555".into(), None)
    );

    // Invalid user input must not persist or partially update the other provider.
    for id in ["00000000-0000-0000-0000-000000000000", "not-an-app-id"] {
        assert!(
            core.set_oauth_apps(
                Some(("must-not-be-saved".into(), "secret".into())),
                Some(id.into()),
            )
            .await
            .is_err()
        );
        let stored = core.get_settings().await.unwrap();
        assert_eq!(stored.google_client_id, "custom-google-id");
        assert_eq!(stored.ms_client_id, "11111111-2222-4333-8444-555555555555");
        let mut invalid_settings = stored;
        invalid_settings.ms_client_id = id.into();
        assert!(core.set_settings(invalid_settings).await.is_err());
        assert_eq!(
            providers::resolve_credentials(Provider::Microsoft)
                .unwrap()
                .0,
            "11111111-2222-4333-8444-555555555555"
        );
    }
    assert!(
        core.set_oauth_apps(
            Some(("test.apps.googleusercontent.com".into(), String::new())),
            None,
        )
        .await
        .is_err()
    );

    drop(core);
    let core = Core::start(paths.clone()).await.unwrap();
    assert_eq!(
        core.get_settings().await.unwrap().google_client_id,
        "custom-google-id"
    );
    assert_eq!(
        providers::resolve_credentials(Provider::Gmail).unwrap().0,
        "custom-google-id"
    );

    // Force the actual SQLite write to fail, leaving reads available.
    core.db
        .write(|conn| {
            conn.execute_batch("PRAGMA query_only = ON")?;
            Ok(())
        })
        .await
        .unwrap();
    settings.google_client_id = "must-not-become-active".into();
    assert!(core.set_settings(settings.clone()).await.is_err());
    assert!(
        core.set_oauth_apps(
            Some(("must-not-become-active".into(), "wrong-secret".into())),
            Some("44444444-5555-4666-8777-888888888888".into()),
        )
        .await
        .is_err()
    );
    assert_eq!(
        providers::resolve_credentials(Provider::Microsoft)
            .unwrap()
            .0,
        "11111111-2222-4333-8444-555555555555"
    );
    assert_eq!(
        providers::resolve_credentials(Provider::Gmail).unwrap().0,
        "custom-google-id"
    );
    assert_eq!(
        core.get_settings().await.unwrap().google_client_id,
        "custom-google-id"
    );
    core.db
        .write(|conn| {
            conn.execute_batch("PRAGMA query_only = OFF")?;
            Ok(())
        })
        .await
        .unwrap();

    // A single-provider update keeps the other provider and unrelated settings.
    settings = core.get_settings().await.unwrap();
    settings.theme = "dark".into();
    core.set_settings(settings).await.unwrap();
    let saved = core
        .set_oauth_apps(Some(("  ".into(), "discard-me".into())), None)
        .await
        .unwrap();
    assert!(saved.google_client_secret.is_empty());
    assert_eq!(saved.theme, "dark");
    assert_eq!(saved.ms_client_id, "11111111-2222-4333-8444-555555555555");
    assert_eq!(
        providers::resolve_credentials(Provider::Gmail).ok(),
        defaults
    );
    // Clearing one provider must not disable another provider's custom keys.
    assert_eq!(
        providers::resolve_credentials(Provider::Microsoft)
            .unwrap()
            .0,
        "11111111-2222-4333-8444-555555555555"
    );
    // A busy writer must suspend the save, leaving the async executor free.
    let db = core.db.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocked_writer = tokio::spawn(async move {
        db.write(move |_| {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            Ok(())
        })
        .await
        .unwrap();
    });
    entered_rx.await.unwrap();
    let mut save = Box::pin(core.set_oauth_apps(
        Some(("delayed-google".into(), "secret".into())),
        Some("22222222-3333-4444-8555-666666666666".into()),
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut save)
            .await
            .is_err()
    );
    release_tx.send(()).unwrap();
    let saved = save.await.unwrap();
    assert_eq!(saved.google_client_id, "delayed-google");
    assert_eq!(saved.ms_client_id, "22222222-3333-4444-8555-666666666666");
    blocked_writer.await.unwrap();

    // Retry both providers after the write failure; both must be saved together.
    core.set_oauth_apps(
        Some(("retry-google".into(), "retry-secret".into())),
        Some("33333333-4444-4555-8666-777777777777".into()),
    )
    .await
    .unwrap();
    let saved = core.get_settings().await.unwrap();
    assert_eq!(saved.google_client_id, "retry-google");
    assert_eq!(saved.ms_client_id, "33333333-4444-4555-8666-777777777777");
    core.set_oauth_apps(Some((String::new(), String::new())), Some(String::new()))
        .await
        .unwrap();

    // Emulate configuration persisted by an older version, bypassing new input
    // validation. Startup must remain usable and must never launch a browser
    // with the placeholder from issue #65.
    core.db
        .write(|conn| {
            let mut settings = rustmail_core::db::repo::settings::get(conn)?;
            settings.ms_client_id = "00000000-0000-0000-0000-000000000000".into();
            rustmail_core::db::repo::settings::set(conn, &settings)
        })
        .await
        .unwrap();
    drop(core);
    let core = Core::start(paths).await.unwrap();
    assert!(providers::resolve_credentials(Provider::Microsoft).is_err());
    let result = rustmail_core::oauth::flow::authorize(Provider::Microsoft, |_| {
        panic!("invalid OAuth registration must never open the browser")
    })
    .await;
    assert!(result.is_err());
    let mut settings = core.get_settings().await.unwrap();
    settings.theme = "light".into();
    core.set_settings(settings).await.unwrap();
    core.set_oauth_apps(None, Some("11111111-2222-4333-8444-555555555555".into()))
        .await
        .unwrap();
    assert!(providers::resolve_credentials(Provider::Microsoft).is_ok());
    core.set_oauth_apps(None, Some(String::new()))
        .await
        .unwrap();
}
