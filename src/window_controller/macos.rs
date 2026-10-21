//! Show the window when the user opens Rustmail again from the Dock,
//! Finder or Spotlight.
//!
//! macOS does not start a second process for an application that is already
//! running; it sends the running one a reopen Apple Event. AppKit forwards
//! that event to `applicationShouldHandleReopen:`, which winit's application
//! delegate does not implement, so a window hidden in the tray would stay
//! hidden. The event is handled here instead.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_foundation::{NSAppleEventManager, NSObject};
use std::cell::{OnceCell, RefCell};

/// `kCoreEventClass` and `kAEReopenApplication`.
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const REOPEN_APPLICATION: u32 = u32::from_be_bytes(*b"rapp");

thread_local! {
    /// NSAppleEventManager does not retain its handlers.
    static HANDLER: OnceCell<Retained<ReopenHandler>> = const { OnceCell::new() };
    static CALLBACK: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RustmailReopenHandler"]
    struct ReopenHandler;

    impl ReopenHandler {
        #[unsafe(method(handleReopen:withReplyEvent:))]
        fn handle_reopen(&self, _event: &AnyObject, _reply: &AnyObject) {
            CALLBACK.with(|callback| {
                if let Some(callback) = callback.borrow().as_ref() {
                    callback();
                }
            });
        }
    }
);

/// Call `callback` whenever the application is reopened. Install it once the
/// event loop runs: AppKit registers its own handler while it finishes
/// launching, and the later registration replaces it.
pub(super) fn on_reopen(callback: impl Fn() + 'static) {
    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    CALLBACK.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
    HANDLER.with(|handler| {
        let handler = handler.get_or_init(|| {
            // SAFETY: `init` is NSObject's designated initializer.
            unsafe { msg_send![ReopenHandler::alloc(main_thread), init] }
        });
        let manager = NSAppleEventManager::sharedAppleEventManager();
        // SAFETY: the handler implements the selector with the signature
        // NSAppleEventManager calls, and HANDLER keeps it alive for the life
        // of the thread.
        unsafe {
            let _: () = msg_send![
                &manager,
                setEventHandler: &**handler,
                andSelector: sel!(handleReopen:withReplyEvent:),
                forEventClass: CORE_EVENT_CLASS,
                andEventID: REOPEN_APPLICATION,
            ];
        }
    });
}
