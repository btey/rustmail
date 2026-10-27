//! Native desktop notifications. Text arrives already translated; the app
//! owns the on/off setting and passes only what should be shown.

/// Show one notification without blocking the UI thread. Failures (no
/// notification service, a sandbox without access) are logged and never
/// reach the user: the in-app card is the primary announcement.
///
/// `on_click` runs on the notification thread when the user clicks the
/// notification (Linux notification servers only; elsewhere it is dropped).
/// The thread stays alive until the server closes the notification.
pub(crate) fn show(
    summary: String,
    body: String,
    sound: bool,
    on_click: Option<(String, Box<dyn FnOnce() + Send>)>,
) {
    let spawned = std::thread::Builder::new()
        .name("desktop-notify".into())
        .spawn(move || {
            let mut notification = notify_rust::Notification::new();
            notification
                .appname("Rustmail")
                .summary(&summary)
                .body(&body);
            #[cfg(all(unix, not(target_os = "macos")))]
            if sound {
                notification.sound_name("message-new-instant");
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            let _ = sound;
            #[cfg(all(unix, not(target_os = "macos")))]
            if let Some((label, _)) = &on_click {
                notification.action("default", label);
            }
            let handle = match notification.show() {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::debug!("desktop notification not shown: {error}");
                    return;
                }
            };
            #[cfg(all(unix, not(target_os = "macos")))]
            if let Some((_, on_click)) = on_click {
                let mut on_click = Some(on_click);
                handle.wait_for_action(|action| {
                    if action == "default"
                        && let Some(on_click) = on_click.take()
                    {
                        on_click();
                    }
                });
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            let _ = (handle, on_click);
        });
    if let Err(error) = spawned {
        tracing::debug!("desktop notification thread not started: {error}");
    }
}
