//! Exchange Online (Microsoft 365) account setup: reads the form fields of the
//! `ExchangeSetupUi` global, runs the chosen sign-in (pasted address, device
//! code or system browser) and adds the account once Microsoft Graph accepts
//! the sign-in. The tokens only ever go to the core, which stores them in the
//! credential store.
use super::*;
use rustmail_core::exchange::{
    AddExchangeAccountArgs, ExchangeCloud, ExchangeEndpoint, ExchangeSettings, ExchangeSignIn,
    auth::{AuthorizationRequest, TokenSet},
    settings::LOOPBACK_REDIRECT_URI,
};

/// A manual sign-in that waits for the user to paste the browser's address.
pub(super) struct PendingManual {
    core: CoreMailSource,
    settings: ExchangeSettings,
    request: AuthorizationRequest,
    email: String,
    display_name: Option<String>,
}

pub(super) type Pending = Rc<RefCell<Option<PendingManual>>>;

/// The settings typed into the form. Empty fields keep the engine defaults.
pub(super) fn settings_from_form(app: &AppWindow) -> ExchangeSettings {
    let ui = app.global::<ExchangeSetupUi>();
    let sign_in = ExchangeSignIn::from_str_lossy(ui.get_sign_in().as_str());
    let mut redirect_uri = ui.get_redirect_uri().trim().to_owned();
    if redirect_uri.is_empty() && sign_in == ExchangeSignIn::Browser {
        redirect_uri = LOOPBACK_REDIRECT_URI.to_owned();
    }
    ExchangeSettings {
        enabled: true,
        client_id: ui.get_client_id().trim().to_owned(),
        tenant: ui.get_tenant().trim().to_owned(),
        redirect_uri,
        scopes: ui.get_scopes().trim().to_owned(),
        cloud: ExchangeCloud::from_str_lossy(ui.get_cloud().as_str()),
        custom_login_url: ui.get_login_url().trim().to_owned(),
        custom_graph_url: ui.get_graph_url().trim().to_owned(),
        sign_in,
        endpoint: ExchangeEndpoint::from_str_lossy(ui.get_endpoint().as_str()),
        shared_mailbox: ui.get_mailbox().trim().to_owned(),
    }
}

fn failed(error: impl std::fmt::Display) -> UiTaskUpdate {
    UiTaskUpdate {
        message: UiMessage::detail("Exchange sign-in failed: {}", error),
        accounts: None,
        calendar_connections: None,
        carddav_connections: None,
        calendar_error: None,
        account_removal: None,
        clear_account_form: false,
        finishes_account_setup: true,
        finishes_oauth: false,
        close_to_tray: None,
    }
}

fn reset_steps(app: slint::Weak<AppWindow>) {
    let _ = app.upgrade_in_event_loop(|app| {
        let ui = app.global::<ExchangeSetupUi>();
        ui.set_step("".into());
        ui.set_link("".into());
        ui.set_device_code("".into());
        ui.set_pasted("".into());
    });
}

/// Verify the sign-in with Graph, store the tokens and add the account.
async fn finish(
    core: CoreMailSource,
    args: AddExchangeAccountArgs,
    tokens: TokenSet,
    app: slint::Weak<AppWindow>,
    updates: UiSender<UiTaskUpdate>,
) {
    reset_steps(app.clone());
    let _ = app.upgrade_in_event_loop(|app| {
        app.set_sync_status(UiMessage::plain(
            "Checking the sign-in with Microsoft Graph…",
        ));
    });
    let update = match core.file_core().add_exchange_account(args, tokens).await {
        Ok(account) => UiTaskUpdate {
            message: UiMessage::detail(
                "Connected {}. Initial sync is running in the background.",
                account.email,
            ),
            accounts: core.load_account_snapshot().await.ok(),
            calendar_connections: None,
            carddav_connections: None,
            calendar_error: None,
            account_removal: None,
            clear_account_form: true,
            finishes_account_setup: true,
            finishes_oauth: false,
            close_to_tray: None,
        },
        Err(error) => failed(error),
    };
    let _ = updates.send(update).await;
}

fn open_in_thread(opener: ExternalUrlOpener, url: String) {
    std::thread::spawn(move || {
        let _ = opener.open_authorization(&url);
    });
}

/// Start the sign-in chosen in the form. Called by the account form's add
/// button when the Exchange type is selected.
pub(super) fn begin(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
    updates: &UiSender<UiTaskUpdate>,
    opener: &ExternalUrlOpener,
    pending: &Pending,
    email: &str,
) {
    if app.get_account_setup_in_progress() {
        return;
    }
    let email = email.trim().to_owned();
    if email.is_empty() {
        app.set_sync_status(UiMessage::plain("Enter the Exchange mailbox address first."));
        return;
    }
    let Some(core) = state.borrow().core.clone() else {
        app.set_sync_status(UiMessage::plain(
            "Local mail data is unavailable. Retry startup.",
        ));
        return;
    };
    let settings = settings_from_form(app);
    if let Err(error) = settings.validate() {
        app.set_sync_status(UiMessage::detail("Exchange sign-in failed: {}", error));
        return;
    }
    let display_name = Some(app.get_advanced().display_name.trim().to_owned())
        .filter(|name| !name.is_empty());
    let ui = app.global::<ExchangeSetupUi>();
    app.set_account_setup_in_progress(true);
    match settings.sign_in {
        ExchangeSignIn::Manual => match core.file_core().exchange_begin_manual(&settings, Some(&email)) {
            Ok(request) => {
                ui.set_link(request.url.clone().into());
                ui.set_pasted("".into());
                ui.set_step("paste".into());
                app.set_sync_status(UiMessage::plain(
                    "Open the sign-in address, then paste the address the browser ends on.",
                ));
                *pending.borrow_mut() = Some(PendingManual {
                    core,
                    settings,
                    request,
                    email,
                    display_name,
                });
            }
            Err(error) => {
                app.set_account_setup_in_progress(false);
                app.set_sync_status(UiMessage::detail("Exchange sign-in failed: {}", error));
            }
        },
        ExchangeSignIn::DeviceCode => {
            let weak = app.as_weak();
            let updates = updates.clone();
            app.set_sync_status(UiMessage::plain(
                "Enter the code at the sign-in address, then wait here.",
            ));
            runtime.spawn(async move {
                let device = match core.file_core().exchange_begin_device_code(&settings).await {
                    Ok(device) => device,
                    Err(error) => {
                        reset_steps(weak);
                        let _ = updates.send(failed(error)).await;
                        return;
                    }
                };
                let (code, link) = (device.user_code.clone(), device.verification_uri.clone());
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let ui = app.global::<ExchangeSetupUi>();
                    ui.set_device_code(code.into());
                    ui.set_link(link.into());
                    ui.set_step("device".into());
                });
                match core.file_core().exchange_finish_device_code(&settings, &device).await {
                    Ok(tokens) => {
                        let args = AddExchangeAccountArgs {
                            email,
                            display_name,
                            exchange: settings,
                        };
                        finish(core, args, tokens, weak, updates).await;
                    }
                    Err(error) => {
                        reset_steps(weak);
                        let _ = updates.send(failed(error)).await;
                    }
                }
            });
        }
        ExchangeSignIn::Browser => {
            let weak = app.as_weak();
            let updates = updates.clone();
            let opener = opener.clone();
            ui.set_step("browser".into());
            app.set_sync_status(UiMessage::plain(
                "Complete sign-in in your browser, or choose Cancel here.",
            ));
            runtime.spawn(async move {
                let open_weak = weak.clone();
                let result = core
                    .file_core()
                    .exchange_sign_in(&settings, Some(&email), move |url| {
                        let link = url.clone();
                        let _ = open_weak.upgrade_in_event_loop(move |app| {
                            app.global::<ExchangeSetupUi>().set_link(link.into());
                        });
                        open_in_thread(opener, url);
                        Ok(())
                    })
                    .await;
                match result {
                    Ok(tokens) => {
                        let args = AddExchangeAccountArgs {
                            email,
                            display_name,
                            exchange: settings,
                        };
                        finish(core, args, tokens, weak, updates).await;
                    }
                    Err(error) => {
                        reset_steps(weak);
                        let _ = updates.send(failed(error)).await;
                    }
                }
            });
        }
    }
}

pub(super) fn register(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
    updates: &UiSender<UiTaskUpdate>,
    opener: &ExternalUrlOpener,
) -> impl Fn(&AppWindow, &str) + 'static {
    let pending: Pending = Rc::new(RefCell::new(None));
    let ui = app.global::<ExchangeSetupUi>();

    let weak = app.as_weak();
    let (pending_for_submit, runtime_for_submit, updates_for_submit) =
        (pending.clone(), runtime.clone(), updates.clone());
    ui.on_submit_pasted(move |pasted| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let Some(manual) = pending_for_submit.borrow_mut().take() else {
            return;
        };
        let (weak, updates) = (app.as_weak(), updates_for_submit.clone());
        app.set_sync_status(UiMessage::plain(
            "Checking the sign-in with Microsoft Graph…",
        ));
        let pasted = pasted.to_string();
        runtime_for_submit.spawn(async move {
            let PendingManual {
                core,
                settings,
                request,
                email,
                display_name,
            } = manual;
            match core
                .file_core()
                .exchange_finish_manual(&settings, &request, &pasted)
                .await
            {
                Ok(tokens) => {
                    let args = AddExchangeAccountArgs {
                        email,
                        display_name,
                        exchange: settings,
                    };
                    finish(core, args, tokens, weak, updates).await;
                }
                Err(error) => {
                    reset_steps(weak);
                    let _ = updates.send(failed(error)).await;
                }
            }
        });
    });

    let weak = app.as_weak();
    let opener_for_link = opener.clone();
    ui.on_open_link(move || {
        if let Some(app) = weak.upgrade() {
            let link = app.global::<ExchangeSetupUi>().get_link().to_string();
            if !link.is_empty() {
                open_in_thread(opener_for_link.clone(), link);
            }
        }
    });

    let weak = app.as_weak();
    let (state_for_cancel, pending_for_cancel) = (Rc::clone(state), pending.clone());
    ui.on_cancel(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        pending_for_cancel.borrow_mut().take();
        if let Some(core) = state_for_cancel.borrow().core.as_ref() {
            core.file_core().cancel_oauth();
        }
        let ui = app.global::<ExchangeSetupUi>();
        ui.set_step("".into());
        ui.set_link("".into());
        ui.set_device_code("".into());
        ui.set_pasted("".into());
        app.set_account_setup_in_progress(false);
        app.set_sync_status(UiMessage::plain("Exchange sign-in cancelled."));
    });

    let (state, runtime, updates, opener) = (
        Rc::clone(state),
        runtime.clone(),
        updates.clone(),
        opener.clone(),
    );
    move |app: &AppWindow, email: &str| {
        begin(app, &state, &runtime, &updates, &opener, &pending, email);
    }
}
