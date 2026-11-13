//! The calendar event editor's reminder, repeat and "apply to" controls can
//! be reached with the pointer. The panel's layout depends on the font, so
//! each control is found by clicking across the panel instead of at a fixed
//! position.

use slint::platform::{
    Platform, PointerEventButton, WindowAdapter, WindowEvent,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use std::{cell::RefCell, rc::Rc};

slint::slint! {
    import { CalendarEventDialog } from "../ui/dialogs/calendar-event-dialog.slint";

    export component EditorHarness inherits Window {
        preferred-width: 800px;
        preferred-height: 1300px;
        background: white;
        in-out property <bool> editor-open: true;
        in-out property <bool> editing: false;
        in-out property <string> event-title: "Planning";
        in-out property <int> reminder: -1;
        in-out property <string> repeat: "none";
        in-out property <string> interval: "1";
        in-out property <int> weekdays: 0;
        in-out property <string> end-kind: "never";
        in-out property <string> end-value: "";
        in-out property <bool> more-open: false;
        in-out property <bool> series: false;
        in-out property <string> scope: "all";
        in property <string> busy;
        callback saved(string);
        CalendarEventDialog {
            x: 0px;
            y: 0px;
            width: parent.width;
            height: parent.height;
            accent: blue;
            border: gray;
            border-subtle: gray;
            control-bg: #eeeeee;
            muted-text: gray;
            surface: white;
            title-color: black;
            calendar_editor_open <=> root.editor-open;
            calendar_event_editing: root.editing;
            calendar_event_title <=> root.event-title;
            calendar_event_reminder <=> root.reminder;
            calendar_event_repeat <=> root.repeat;
            calendar_event_interval <=> root.interval;
            calendar_event_weekdays <=> root.weekdays;
            calendar_event_end_kind <=> root.end-kind;
            calendar_event_end_value <=> root.end-value;
            calendar_event_more_open <=> root.more-open;
            calendar_event_series: root.series;
            calendar_event_scope <=> root.scope;
            calendar_event_busy: root.busy;
            calendar-save-event(title, date, time, duration, color) => { root.saved(title); }
        }
    }
}

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn click(ui: &EditorHarness, x: f32, y: f32) {
    // A click on the backdrop closes the editor; it is opened again so the
    // scan can go on.
    ui.set_editor_open(true);
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

/// Click across the panel until `hit` reports the control was reached;
/// `reset` puts the state back before every try.
fn find(ui: &EditorHarness, reset: impl Fn(), hit: impl Fn() -> bool) -> bool {
    for y in (0..1300).step_by(4) {
        for x in (180..640).step_by(10) {
            reset();
            click(ui, x as f32, y as f32);
            if hit() {
                return true;
            }
        }
    }
    false
}

#[test]
fn reminder_repeat_and_scope_can_be_chosen_with_the_pointer() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(Headless(window))).unwrap();
    let ui = EditorHarness::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(800, 1300));
    ui.show().unwrap();

    // The folded section opens from its heading.
    assert!(
        find(&ui, || ui.set_more_open(false), || ui.get_more_open()),
        "the Reminder and repeat heading opens the section"
    );

    // With the section open: a reminder chip, a repeat chip.
    ui.set_more_open(true);
    assert!(
        find(&ui, || ui.set_reminder(-1), || ui.get_reminder() == 15),
        "the 15 min reminder can be chosen"
    );
    assert!(
        find(
            &ui,
            || ui.set_repeat("none".into()),
            || ui.get_repeat() == "weekly"
        ),
        "the Weekly repeat can be chosen"
    );

    // A weekly rule shows the weekday chips; one click on one sets a day, and
    // a second click on the same chip takes it away again.
    ui.set_weekdays(0);
    let mut toggled = None;
    'scan: for y in (0..1300).step_by(4) {
        for x in (180..640).step_by(6) {
            ui.set_weekdays(0);
            ui.set_repeat("weekly".into());
            click(&ui, x as f32, y as f32);
            if ui.get_weekdays() != 0 {
                toggled = Some((x as f32, y as f32, ui.get_weekdays()));
                break 'scan;
            }
        }
    }
    let (x, y, mask) = toggled.expect("a weekday chip can be clicked");
    assert!(mask.count_ones() == 1 && mask <= 64, "{mask}");
    ui.set_weekdays(mask);
    ui.set_repeat("weekly".into());
    click(&ui, x, y);
    assert_eq!(ui.get_weekdays(), 0, "clicking again clears the day");

    // The end choice: "After" asks for a number of events.
    assert!(
        find(
            &ui,
            || {
                ui.set_repeat("daily".into());
                ui.set_end_kind("never".into());
            },
            || ui.get_end_kind() == "count"
        ),
        "the After end can be chosen"
    );

    // Without a series there is no "apply to" choice; with one there is.
    ui.set_series(false);
    ui.set_scope("all".into());
    for y in (0..1300).step_by(4) {
        for x in (180..640).step_by(10) {
            click(&ui, x as f32, y as f32);
            assert_ne!(ui.get_scope(), "occurrence", "no scope without a series");
        }
    }
    ui.set_series(true);
    assert!(
        find(
            &ui,
            || ui.set_scope("all".into()),
            || ui.get_scope() == "occurrence"
        ),
        "This event only can be chosen for a series"
    );
}

#[test]
fn the_save_button_reports_the_title() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    slint::platform::set_platform(Box::new(Headless(window))).unwrap();
    let ui = EditorHarness::new().unwrap();
    ui.window().set_size(slint::PhysicalSize::new(800, 1300));
    ui.show().unwrap();
    let saved = Rc::new(RefCell::new(Vec::<String>::new()));
    let record = Rc::clone(&saved);
    ui.on_saved(move |title| record.borrow_mut().push(title.to_string()));
    // The button sits at the bottom right of the panel.
    'scan: for y in (0..1300).step_by(4) {
        for x in (300..640).step_by(8) {
            click(&ui, x as f32, y as f32);
            if !saved.borrow().is_empty() {
                break 'scan;
            }
        }
    }
    assert_eq!(saved.borrow().first().map(String::as_str), Some("Planning"));
}
