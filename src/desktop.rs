#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Slint 1.18 keeps the native window and its rendering surface when
    // hidden on X11 and Windows. Use its window recreation path for tray mode,
    // as it already does on Wayland, so a hidden window holds no surface.
    // macOS keeps its window: AppKit attributes set after creation would be
    // lost on recreation.
    #[cfg(any(linux_desktop, target_os = "windows"))]
    if std::env::var_os("SLINT_DESTROY_WINDOW_ON_HIDE").is_none() {
        // SAFETY: the desktop entry point has not started any threads or
        // initialized libraries yet. Never mutate this from a tray callback.
        unsafe { std::env::set_var("SLINT_DESTROY_WINDOW_ON_HIDE", "1") };
    }
    #[cfg(desktop)]
    if let Some(code) = rustmail::pdf_preview::run_worker_if_requested() {
        std::process::exit(code);
    }
    rustmail::run_desktop(rustmail::platform::launch_context()?)
}
