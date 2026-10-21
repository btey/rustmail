//! Hand a launcher's activation token to the running window.
//!
//! Desktops only let a window take focus on behalf of the user's action that
//! caused it. When a second launch forwards its request to the running
//! instance, the token the launcher gave that launch must be used by the
//! window that is actually raised:
//!
//! - Wayland: `xdg_activation_v1.activate(token, surface)`. Winit only applies
//!   tokens when it creates a window, and Slint creates it without one, so the
//!   request is sent on winit's own display connection through a separate
//!   event queue.
//! - X11: the token is a startup-notification id. Completing it stops the
//!   launcher's busy cursor; focus is then requested as usual.

use slint::winit_030::winit;
use std::error::Error;
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

/// The token this process was launched with, if any.
pub(crate) fn launch_token() -> Option<String> {
    ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|token| !token.is_empty())
}

/// Use `token` for the window. Returns true when the compositor was asked to
/// activate it, so a separate focus request is unnecessary.
pub(super) fn activate(window: &winit::window::Window, token: &str) -> bool {
    let (Ok(display), Ok(handle)) = (window.display_handle(), window.window_handle()) else {
        return false;
    };
    match (display.as_raw(), handle.as_raw()) {
        (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) => {
            // SAFETY: both pointers belong to winit's live window, which
            // outlives this synchronous call.
            let result = unsafe {
                wayland::activate(
                    display.display.as_ptr().cast(),
                    surface.surface.as_ptr().cast(),
                    token,
                )
            };
            result
                .inspect_err(|error| eprintln!("could not activate the window: {error}"))
                .is_ok()
        }
        (_, RawWindowHandle::Xlib(handle)) => {
            complete_startup(u32::try_from(handle.window).ok(), token);
            false
        }
        (_, RawWindowHandle::Xcb(handle)) => {
            complete_startup(Some(handle.window.get()), token);
            false
        }
        _ => false,
    }
}

fn complete_startup(window: Option<u32>, startup_id: &str) {
    let Some(window) = window else {
        return;
    };
    if let Err(error) = x11::complete_startup(window, startup_id) {
        eprintln!("could not complete startup notification: {error}");
    }
}

mod wayland {
    use super::Error;
    use std::ffi::c_void;
    use wayland_client::{
        Connection, Dispatch, Proxy, QueueHandle,
        backend::{Backend, ObjectId},
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_registry::WlRegistry, wl_surface::WlSurface},
    };
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::{
        self, XdgActivationV1,
    };

    struct State;

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: <WlRegistry as Proxy>::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<XdgActivationV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &XdgActivationV1,
            _: xdg_activation_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    /// # Safety
    ///
    /// `display` and `surface` must be the live `wl_display` and `wl_surface`
    /// of a window on this thread.
    pub(super) unsafe fn activate(
        display: *mut c_void,
        surface: *mut c_void,
        token: &str,
    ) -> Result<(), Box<dyn Error>> {
        // SAFETY: guaranteed by the caller; the guest backend does not take
        // ownership of the display.
        let connection =
            Connection::from_backend(unsafe { Backend::from_foreign_display(display.cast()) });
        let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
        let activation: XdgActivationV1 = globals.bind(&queue.handle(), 1..=1, ())?;
        // SAFETY: guaranteed by the caller.
        let surface = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.cast()) }?;
        let surface = WlSurface::from_id(&connection, surface)?;
        activation.activate(token.to_owned(), &surface);
        activation.destroy();
        queue.roundtrip(&mut State)?;
        Ok(())
    }
}

mod x11 {
    use super::Error;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{ClientMessageEvent, ConnectionExt, EventMask};

    /// Broadcast `remove: ID=…` as the startup-notification specification
    /// describes: 20-byte client messages, the first marked as the beginning.
    pub(super) fn complete_startup(window: u32, startup_id: &str) -> Result<(), Box<dyn Error>> {
        let (connection, screen) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let begin = connection
            .intern_atom(false, b"_NET_STARTUP_INFO_BEGIN")?
            .reply()?
            .atom;
        let more = connection
            .intern_atom(false, b"_NET_STARTUP_INFO")?
            .reply()?
            .atom;
        for (index, chunk) in message(startup_id).chunks(20).enumerate() {
            let mut data = [0_u8; 20];
            data[..chunk.len()].copy_from_slice(chunk);
            let kind = if index == 0 { begin } else { more };
            let event = ClientMessageEvent::new(8, window, kind, data);
            // Wait for each request to be processed: this connection closes
            // right after, and errors would otherwise go unreported.
            connection
                .send_event(false, root, EventMask::PROPERTY_CHANGE, event)?
                .check()?;
        }
        Ok(())
    }

    pub(super) fn message(startup_id: &str) -> Vec<u8> {
        let mut message = String::from("remove: ID=\"");
        for character in startup_id.chars() {
            if matches!(character, '"' | '\\') {
                message.push('\\');
            }
            message.push(character);
        }
        message.push('"');
        let mut bytes = message.into_bytes();
        bytes.push(0);
        bytes
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn startup_ids_are_quoted_and_terminated() {
            assert_eq!(
                super::message(r#"app"1\2_TIME3"#),
                b"remove: ID=\"app\\\"1\\\\2_TIME3\"\0"
            );
        }
    }
}
