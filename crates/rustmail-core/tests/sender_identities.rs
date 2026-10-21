use rustmail_core::Core;
use rustmail_core::config::Paths;
use rustmail_core::models::SenderIdentitySource;

async fn setup() -> (tempfile::TempDir, Core) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::for_tests(tmp.path());
    let core = Core::start(paths).await.unwrap();
    core.db
        .write(|conn| {
            conn.execute_batch(
                "INSERT INTO accounts
                   (id,email,display_name,provider,auth_kind,username,imap_host,imap_port,
                    smtp_host,smtp_port,created_at)
                 VALUES (1,'me@example.com','Me','imap','password','me','127.0.0.1',1,
                         '127.0.0.1',1,0);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    (tmp, core)
}

fn emails(identities: &[rustmail_core::models::SenderIdentity]) -> Vec<&str> {
    identities
        .iter()
        .map(|identity| identity.email.as_str())
        .collect()
}

#[tokio::test]
async fn refresh_returns_every_saved_identity_and_keeps_the_chosen_default() {
    let (_tmp, core) = setup().await;
    core.save_sender_identity(
        1,
        " alias@example.com ".into(),
        Some("Alias".into()),
        Some("desk@example.com".into()),
    )
    .await
    .unwrap();
    core.set_default_sender_identity(1, "ALIAS@example.com".into())
        .await
        .unwrap();

    let refreshed = core.refresh_sender_identities(1).await.unwrap();
    let cached = core.list_sender_identities(1).await.unwrap();
    // Settings and the composer project the refresh result directly, so it
    // must be the persisted list rather than only what the provider reported.
    assert_eq!(refreshed, cached);
    assert_eq!(emails(&refreshed), ["me@example.com", "alias@example.com"]);
    let alias = &refreshed[1];
    assert_eq!(alias.source, SenderIdentitySource::Manual);
    assert_eq!(alias.display_name.as_deref(), Some("Alias"));
    assert_eq!(alias.reply_to_email.as_deref(), Some("desk@example.com"));
    assert!(alias.is_selectable());
    assert_eq!(
        core.default_sender_identity(1).await.unwrap().email,
        "alias@example.com"
    );

    // Refreshing again is idempotent.
    assert_eq!(
        emails(&core.refresh_sender_identities(1).await.unwrap()),
        emails(&cached)
    );

    // Removing the default falls back to the authenticated address.
    core.remove_sender_identity(1, "Alias@Example.com".into())
        .await
        .unwrap();
    let refreshed = core.refresh_sender_identities(1).await.unwrap();
    assert_eq!(emails(&refreshed), ["me@example.com"]);
    assert_eq!(
        core.default_sender_identity(1).await.unwrap().email,
        "me@example.com"
    );
}

#[tokio::test]
async fn manual_identities_reject_invalid_input_and_provider_addresses() {
    let (_tmp, core) = setup().await;
    for email in ["", "not-an-address", "*@example.com", "a@b@example.com"] {
        assert!(
            core.save_sender_identity(1, email.into(), None, None)
                .await
                .is_err(),
            "{email:?} was accepted"
        );
    }
    assert!(
        core.save_sender_identity(1, "ok@example.com".into(), None, Some("bad".into()))
            .await
            .is_err()
    );
    // The authenticated address belongs to the provider.
    assert!(
        core.save_sender_identity(1, "ME@example.com".into(), None, None)
            .await
            .is_err()
    );
    assert!(
        core.remove_sender_identity(1, "me@example.com".into())
            .await
            .is_err()
    );
    assert!(
        core.set_default_sender_identity(1, "unknown@example.com".into())
            .await
            .is_err()
    );
    assert_eq!(
        emails(&core.list_sender_identities(1).await.unwrap()),
        ["me@example.com"]
    );
}
