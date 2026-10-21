//! Offscreen renders of the account form (edit dialog and first-run screen),
//! used to check its layout without a display. Run with
//! `RUSTMAIL_ACCOUNT_FORM_SCREENSHOTS=<dir> cargo test -p rustmail --lib
//! account_form_screenshots -- --ignored`.
use super::*;
use screenshot_fixture::ScreenshotWindow;

#[test]
#[ignore = "writes screenshots; run manually"]
fn account_form_screenshots() {
    let Some(out) = std::env::var_os("RUSTMAIL_ACCOUNT_FORM_SCREENSHOTS")
        .map(std::path::PathBuf::from)
    else {
        panic!("set RUSTMAIL_ACCOUNT_FORM_SCREENSHOTS to the output directory");
    };
    std::fs::create_dir_all(&out).unwrap();
    let screen = ScreenshotWindow::bare(1.0);
    let app = &screen.app;
    let save = |name: &str, width: u32, height: u32| {
        screen.capture(width, height);
        screen.capture(width, height).save(out.join(format!("{name}.png"))).unwrap();
    };

    app.set_startup_ready(true);
    app.set_startup_hydrated(true);
    // First-run screen: only the groups that can block a first connection.
    app.set_onboarding_form_open(true);
    app.set_account_email("me@example.com".into());
    app.set_imap_host("localhost".into());
    app.set_smtp_host("localhost".into());
    save("first-run-wide", 1320, 800);
    save("first-run-narrow", 560, 800);

    // Settings > Actions.
    {
        let ui = app.global::<UserActions>();
        ui.set_editor_command("alacritty -e nvim %f".into());
        ui.set_rows(ModelRc::new(VecModel::from(vec![
            UserActionRow {
                id: "a".into(),
                name: "Open in Thunderbird".into(),
                command: "thunderbird -file %f".into(),
            },
            UserActionRow {
                id: "b".into(),
                name: "Report as spam".into(),
                command: "spamc -r < %f".into(),
            },
        ])));
    }
    // Settings > Filters.
    {
        let ui = app.global::<FilterRules>();
        ui.set_rows(ModelRc::new(VecModel::from(vec![
            FilterRuleRow {
                id: "1".into(),
                name: "Receipts".into(),
                summary: "If From contains “billing@” or Subject contains “invoice” → move to Receipts, mark read (then stop)".into(),
                enabled: true,
            },
            FilterRuleRow {
                id: "2".into(),
                name: "Big attachments".into(),
                summary: "If Size over 5000 KB and Attachment: yes → star".into(),
                enabled: false,
            },
        ])));
        ui.set_edit_name("Newsletters".into());
        ui.set_c1_field("list_mail".into());
        ui.set_c1_op("yes".into());
        ui.set_c2_field("subject".into());
        ui.set_c2_op("regex".into());
        ui.set_c2_value("weekly|digest".into());
        ui.set_a1_kind("move".into());
        ui.set_a1_value("Newsletters".into());
        ui.set_a2_kind("mark_read".into());
    }
    app.set_toast(UiMessage::arguments(
        "Action “{}” finished: {}",
        "Notify me",
        "Quarterly report: numbers attached",
    ));
    save("toast", 1320, 800);
    app.set_toast(UiMessage::EMPTY);
    // Settings > Accounts > edit: every group.
    app.set_connected_accounts(ModelRc::new(VecModel::from(vec![AccountRow::default()])));
    app.set_settings_open(true);
    app.set_settings_tab("Accounts".into());
    app.set_settings_tab("Filters".into());
    save("settings-filters", 1320, 1000);
    app.set_settings_tab("Actions".into());
    save("settings-actions", 1320, 900);
    app.set_settings_tab("Accounts".into());
    app.set_account_form_open(true);
    save("settings-account-wide", 1320, 800);
    for section in ["security", "sign-in", "identity", "folders", "network"] {
        app.set_account_form_section(section.into());
        save(&format!("settings-account-{section}"), 1320, 800);
    }
    app.set_account_form_section("account".into());
    app.set_account_protocol("jmap".into());
    save("settings-account-jmap", 1320, 800);
    app.set_account_protocol("local".into());
    let mut advanced = app.get_advanced();
    advanced.local_path = "/home/you/Mail".into();
    app.set_advanced(advanced);
    save("settings-account-local", 1320, 800);
    app.set_account_form_section("security".into());
    save("settings-account-local-security", 1320, 800);
    app.set_account_form_section("account".into());
    app.set_account_protocol("imap".into());
    save("settings-account-narrow", 700, 760);
}
