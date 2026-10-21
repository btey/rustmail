use rustmail::{AccountRow, AppWindow};
use slint::platform::{
    Platform, WindowAdapter,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use slint::{ComponentHandle, ModelRc, Rgb8Pixel, VecModel};
use std::rc::Rc;

struct Headless(Rc<MinimalSoftwareWindow>);
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn deferred_mailbox_and_composer_can_be_created_again() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(window.clone()))).unwrap();
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1320, 800));
    app.set_startup_ready(true);
    app.set_active_view("mail".into());
    app.show().unwrap();
    let render = || {
        let size = app.window().size();
        let width = size.width as usize;
        let height = size.height as usize;
        slint::platform::update_timers_and_animations();
        app.window().request_redraw();
        let mut pixels = vec![Rgb8Pixel::default(); width * height];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        std::thread::sleep(std::time::Duration::from_millis(5));
        slint::platform::update_timers_and_animations();
        pixels
    };
    let welcome = render();
    let accounts = ModelRc::new(VecModel::from(vec![AccountRow {
        id: 1,
        name: "Memory test".into(),
        email: "memory@example.invalid".into(),
        ..Default::default()
    }]));
    app.set_connected_accounts(accounts.clone());
    let mailbox = render();
    assert!(welcome != mailbox);
    app.set_compose_to("recipient@example.invalid".into());
    app.set_compose_subject("Preserved draft".into());
    app.set_compose_body("Draft text".into());
    for _ in 0..2 {
        app.set_compose_open(true);
        let compose = render();
        render();
        assert!(compose != mailbox);
        assert!(app.get_compose_editor_width() > 1.0);
        assert!(app.get_compose_editor_viewport_height() > 1.0);
        app.set_compose_open(false);
        render();
        assert_eq!(app.get_compose_subject(), "Preserved draft");
        assert_eq!(app.get_compose_body(), "Draft text");
    }
    app.set_connected_accounts(ModelRc::default());
    render();
    app.set_connected_accounts(accounts);
    render();
    assert_eq!(app.get_compose_to(), "recipient@example.invalid");

    // The conditional dialog receives back requests through a root property.
    // Exercise that bridge again after destruction, including the draft guard.
    app.window().set_size(slint::PhysicalSize::new(390, 844));
    let weak = app.as_weak();
    app.on_close_compose(move || weak.upgrade().unwrap().set_compose_open(false));
    app.set_compose_to("".into());
    app.set_compose_subject("".into());
    app.set_compose_body("".into());
    for theme in ["light", "dark"] {
        app.set_theme_mode(theme.into());
        app.set_compose_open(true);
        render();
        assert!(app.invoke_system_back_requested());
        render();
        assert!(!app.get_compose_open(), "empty composer closes on back");

        app.set_compose_body("Preserved draft 世界".into());
        app.set_compose_open(true);
        let composer = render();
        assert!(
            app.get_compose_open(),
            "reopening must not replay an old back request"
        );
        assert!(app.invoke_system_back_requested());
        let confirmation = render();
        assert!(app.get_compose_open(), "back must protect a nonempty draft");
        assert_ne!(composer, confirmation, "back displays discard confirmation");
        assert_eq!(app.get_compose_body(), "Preserved draft 世界");

        app.set_compose_editor_scroll_y(200.0);
        app.set_compose_editor_preedit_text("未確定".into());
        app.set_compose_open(false);
        render();
        assert_eq!(app.get_compose_editor_scroll_y(), 0.0);
        assert_eq!(app.get_compose_editor_preedit_text(), "");
        assert_eq!(app.get_compose_body(), "Preserved draft 世界");
        app.set_compose_body("".into());
    }
}
