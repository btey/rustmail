//! macOS keeps AppKit's traffic lights over a transparent, full-size title
//! bar, so the application's toolbars extend to the top of the window.
//!
//! The traffic lights are moved to the toolbar row, as Zed and Electron do:
//! AppKit keeps drawing and handling them, only their origin changes. AppKit
//! lays the title bar out again on resize and full-screen transitions, so the
//! position is re-applied after those events.
//!
//! Like Slint's own editor, the integrated window is not movable by AppKit:
//! the transparent title bar would otherwise claim clicks on toolbar controls
//! beneath it. Empty toolbar space moves the window through Slint instead,
//! and title-bar double-clicks follow the system preference.

use super::{TitleBarAction, WindowFrame};
use crate::{AppWindow, WindowChrome};
use objc2_app_kit::{
    NSView, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{NSPoint, NSRect, NSString, NSUserDefaults};
use slint::ComponentHandle;
use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::winit::window::WindowAttributes;
use slint::winit_030::{WinitWindowAccessor, winit};
use std::cell::Cell;

const BUTTONS: [NSWindowButton; 3] = [
    NSWindowButton::CloseButton,
    NSWindowButton::MiniaturizeButton,
    NSWindowButton::ZoomButton,
];

/// AppKit's own title-bar geometry, captured before the first move so the
/// system frame can be restored.
#[derive(Clone, Copy)]
struct NativeLayout {
    container: NSRect,
    buttons: [NSPoint; 3],
}

thread_local! {
    static INTEGRATED: Cell<bool> = const { Cell::new(false) };
    static NATIVE_LAYOUT: Cell<Option<NativeLayout>> = const { Cell::new(None) };
}

/// Create the window with the integrated title bar when it starts enabled, so
/// the first frame is already correct.
pub(super) fn window_attributes(
    attributes: WindowAttributes,
    startup: WindowFrame,
) -> WindowAttributes {
    if startup != WindowFrame::Integrated {
        return attributes;
    }
    attributes
        .with_titlebar_transparent(true)
        .with_title_hidden(true)
        .with_fullsize_content_view(true)
}

pub(super) fn register(app: &AppWindow) {
    // The toolbar row is shorter in compact layouts.
    let weak = app.as_weak();
    app.global::<WindowChrome>().on_geometry_changed(move || {
        if let Some(app) = weak.upgrade() {
            refresh(&app);
        }
    });
    // The native window is created when the event loop starts.
    let weak = app.as_weak();
    let _ = slint::spawn_local(async move {
        let Some(app) = weak.upgrade() else {
            return;
        };
        // Only wait for creation; the returned handle is dropped at once.
        if app.window().winit_window().await.is_ok() {
            apply(&app);
        }
    });
}

pub(super) fn set_integrated(app: &AppWindow, integrated: bool) {
    INTEGRATED.set(integrated);
    apply(app);
}

/// Re-apply the title-bar layout after AppKit may have reset it.
pub(super) fn refresh(app: &AppWindow) {
    if INTEGRATED.get() {
        apply(app);
    }
}

fn apply(app: &AppWindow) {
    let integrated = INTEGRATED.get();
    let chrome = app.global::<WindowChrome>();
    let band_height = f64::from(chrome.get_band_height());
    let traffic_light_x = f64::from(chrome.get_traffic_light_x());
    let native_metrics = app
        .window()
        .with_winit_window(|window: &winit::window::Window| {
            let ns_window = ns_window(window)?;
            let mut mask = ns_window.styleMask();
            mask.set(NSWindowStyleMask::FullSizeContentView, integrated);
            ns_window.setStyleMask(mask);
            ns_window.setTitlebarAppearsTransparent(integrated);
            ns_window.setTitleVisibility(if integrated {
                NSWindowTitleVisibility::Hidden
            } else {
                NSWindowTitleVisibility::Visible
            });
            ns_window.setMovable(!integrated);
            // Full screen shows AppKit's own title bar; leave it alone.
            if integrated && !mask.contains(NSWindowStyleMask::FullScreen) {
                place_traffic_lights(&ns_window, band_height, traffic_light_x)
            } else {
                restore_traffic_lights(&ns_window);
                None
            }
        })
        .flatten();
    // Toolbars reserve room from the system's own metrics, which differ
    // between macOS releases (14pt buttons 23pt apart on macOS 26).
    if let Some((pitch, size)) = native_metrics {
        chrome.set_traffic_light_spacing(pitch as f32);
        chrome.set_traffic_light_size(size as f32);
    }
}

fn ns_window(window: &winit::window::Window) -> Option<objc2::rc::Retained<NSWindow>> {
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: winit's content view outlives this call, and winit only hands
    // out its window on the main thread.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    view.window()
}

/// Centre the traffic lights vertically in the toolbar row, with the close
/// button at `x`. The buttons live in AppKit's title-bar container, which is
/// resized to the row height and kept at the top of the window. The group
/// moves as a whole: AppKit's own size and spacing are kept, as they change
/// between macOS releases. Returns the native button pitch and width.
fn place_traffic_lights(ns_window: &NSWindow, band_height: f64, x: f64) -> Option<(f64, f64)> {
    let buttons = traffic_lights(ns_window)?;
    let container = title_bar_container(&buttons[0])?;
    let native = NATIVE_LAYOUT.get().unwrap_or_else(|| {
        let native = NativeLayout {
            container: container.frame(),
            buttons: buttons.each_ref().map(|button| button.frame().origin),
        };
        NATIVE_LAYOUT.set(Some(native));
        native
    });
    let mut frame = container.frame();
    frame.size.height = band_height;
    frame.origin.y = ns_window.frame().size.height - band_height;
    container.setFrame(frame);
    for (button, origin) in buttons.iter().zip(native.buttons) {
        let size = button.frame().size;
        button.setFrameOrigin(NSPoint::new(
            x + origin.x - native.buttons[0].x,
            // The container is not flipped: y grows upwards.
            ((band_height - size.height) / 2.0).round(),
        ));
    }
    Some((
        native.buttons[1].x - native.buttons[0].x,
        buttons[0].frame().size.width,
    ))
}

fn restore_traffic_lights(ns_window: &NSWindow) {
    let Some(native) = NATIVE_LAYOUT.take() else {
        return;
    };
    let Some(buttons) = traffic_lights(ns_window) else {
        return;
    };
    if let Some(container) = title_bar_container(&buttons[0]) {
        let mut frame = native.container;
        frame.origin.y = ns_window.frame().size.height - frame.size.height;
        container.setFrame(frame);
    }
    for (button, origin) in buttons.iter().zip(native.buttons) {
        button.setFrameOrigin(origin);
    }
}

/// The traffic lights sit in an NSTitlebarView inside the title-bar
/// container that spans the top of the window.
fn title_bar_container(button: &NSView) -> Option<objc2::rc::Retained<NSView>> {
    // SAFETY: called on the main thread with no concurrent changes to the
    // view hierarchy; the returned views are retained.
    unsafe { button.superview()?.superview() }
}

fn traffic_lights(ns_window: &NSWindow) -> Option<[objc2::rc::Retained<NSView>; 3]> {
    let [close, minimize, zoom] =
        BUTTONS.map(|kind| ns_window.standardWindowButton(kind).map(|button| button.into_super().into_super()));
    Some([close?, minimize?, zoom?])
}

pub(super) fn title_bar_action(gesture: &str) -> Option<TitleBarAction> {
    if gesture != "double" {
        return None;
    }
    // System Settings > Desktop & Dock > "Double-click a window's title bar
    // to". "Fill" and an unset preference zoom, as AppKit does.
    let key = NSString::from_str("AppleActionOnDoubleClick");
    let action = NSUserDefaults::standardUserDefaults()
        .stringForKey(&key)
        .map(|value| value.to_string());
    match action.as_deref() {
        Some("None") => None,
        Some("Minimize") => Some(TitleBarAction::Minimize),
        _ => Some(TitleBarAction::ToggleMaximize),
    }
}
