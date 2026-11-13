//! The Exchange account form: the sign-in method buttons and the per-account
//! offline switch can be reached with the pointer. The form's height depends
//! on wrapped text, so the controls are found by clicking down the form
//! rather than at fixed positions.

use slint::platform::{
    Platform, PointerEventButton, WindowAdapter, WindowEvent,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use std::rc::Rc;

slint::slint! {
    import { ExchangeSetupFields, ExchangeSetupUi } from "../ui/components/exchange-setup.slint";
    export { ExchangeSetupUi }

    export component ExchangeFormHarness inherits Window {
        preferred-width: 520px;
        preferred-height: 3000px;
        background: white;
        ExchangeSetupFields {
            x: 0px;
            y: 0px;
            width: parent.width;
            label-color: black;
            body-text: black;
            muted-text: gray;
            accent: blue;
        }
    }
}

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn click(ui: &ExchangeFormHarness, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

#[test]
fn the_sign_in_buttons_and_the_offline_switch_work_with_the_pointer() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(Headless(window))).unwrap();
    let ui = ExchangeFormHarness::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(520, 3000));
    ui.show().unwrap();
    let setup = ui.global::<ExchangeSetupUi>();

    // Adding an account: one of the buttons is "Browser", which also moves
    // the redirect address to the loopback one the browser can reach.
    let mut browser_found = false;
    'scan: for y in (0..1000).step_by(3) {
        setup.set_sign_in("manual".into());
        setup.set_redirect_uri("".into());
        click(&ui, 440.0, y as f32);
        if setup.get_sign_in() == "browser" {
            assert_eq!(setup.get_redirect_uri(), "http://localhost");
            browser_found = true;
            break 'scan;
        }
    }
    assert!(browser_found, "the Browser button can be clicked");
    // The button before it brings the paste method back and clears that
    // redirect again.
    let mut paste_found = false;
    for y in (0..1000).step_by(3) {
        setup.set_sign_in("browser".into());
        setup.set_redirect_uri("http://localhost".into());
        click(&ui, 90.0, y as f32);
        if setup.get_sign_in() == "manual" {
            assert_eq!(setup.get_redirect_uri(), "");
            paste_found = true;
            break;
        }
    }
    assert!(paste_found, "the Paste address button can be clicked");

    // A new account has no offline switch to hit.
    setup.set_sign_in("manual".into());
    setup.set_edit_account_id(0);
    for y in (0..3000).step_by(5) {
        for x in [40.0, 200.0, 440.0, 500.0] {
            click(&ui, x, y as f32);
            assert!(
                !setup.get_work_offline(),
                "no offline switch while adding at ({x}, {y})"
            );
        }
    }

    // Editing an existing account adds it, and it toggles.
    setup.set_edit_account_id(3);
    setup.set_work_offline(false);
    let mut toggled = false;
    'rows: for y in (0..3000).step_by(5) {
        for x in [40.0, 200.0, 440.0, 500.0] {
            setup.set_work_offline(false);
            click(&ui, x, y as f32);
            if setup.get_work_offline() {
                toggled = true;
                break 'rows;
            }
        }
    }
    assert!(toggled, "the offline switch can be turned on while editing");
}
