//! Native desktop notifications. Text arrives already translated; the app
//! owns the on/off setting and passes only what should be shown.

/// Show one notification without blocking the UI thread. Failures (no
/// notification service, a sandbox without access) are logged and never
/// reach the user: the in-app card is the primary announcement.
pub(crate) fn show(summary: String, body: String, sound: bool) {
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
            if let Err(error) = notification.show() {
                tracing::debug!("desktop notification not shown: {error}");
            }
        });
    if let Err(error) = spawned {
        tracing::debug!("desktop notification thread not started: {error}");
    }
}
