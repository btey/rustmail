//! Drive real keyboard events through the mail components without a display.
use slint::platform::{
    Platform, PointerEventButton, WindowAdapter, WindowEvent,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use slint::{ComponentHandle, Model, ModelRc, Rgb8Pixel, VecModel};
use std::{cell::Cell, rc::Rc};

slint::slint! {
    import { Palette } from "std-widgets.slint";
    import { EmailRow, MailListEntry } from "../ui/models.slint";
    export { MailListNavigation, MailNavigationTarget, EmailReader, ReaderItem } from "../ui/models.slint";
    export { AppTheme, MotionSettings } from "../ui/design-system.slint";
    import { AppTheme } from "../ui/design-system.slint";
    import { MailListPane } from "../ui/views/mail-list-pane.slint";
    import { MailView } from "../ui/views/mail-view.slint";

    export component MailKeyboardHarness inherits Window {
        preferred-width: 500px;
        preferred-height: 400px;
        in-out property <[EmailRow]> emails;
        in-out property <[MailListEntry]> entries;
        in-out property <bool> preview: true;
        in-out property <bool> dark: false;
        in-out property <bool> more: false;
        in-out property <string> scope: "Inbox";
        out property <int> cursor: list.keyboard-email-id;
        out property <length> scroll-y: list.scroll-y;
        out property <bool> focused: list.keyboard-focused;
        callback selected(int);
        callback opened(int);
        callback checked(int, bool);
        callback page-requested();
        callback searched(string);
        public function focus-list() { list.focus-list(true); }
        init => { Palette.color-scheme = ColorScheme.light; }
        changed dark => { Palette.color-scheme = root.dark ? ColorScheme.dark : ColorScheme.light; }
        list := MailListPane {
            width: parent.width;
            height: parent.height;
            emails: root.emails;
            list_entries: root.entries;
            preview-on-navigation: root.preview;
            selected_scope: root.scope;
            can_load_more: root.more;
            surface: AppTheme.surface;
            heading: AppTheme.heading;
            body-text: AppTheme.body;
            secondary-text: AppTheme.secondary;
            muted-text: AppTheme.muted;
            faint-text: AppTheme.faint;
            border: AppTheme.border;
            border-subtle: AppTheme.border-subtle;
            accent: AppTheme.accent;
            accent-soft: AppTheme.accent-soft;
            selected-row: AppTheme.selected-row;
            control-bg: AppTheme.control-bg;
            avatar-bg: AppTheme.avatar-bg;
            avatar-selected-bg: AppTheme.avatar-selected-bg;
            select_email(id) => { root.selected(id); }
            open_email(id) => { root.opened(id); }
            set_email_checked(id, value) => { root.checked(id, value); }
            load_more => { root.page-requested(); }
            search_changed(query) => { root.searched(query); }
        }
    }

    export component MailViewKeyboardHarness inherits Window {
        preferred-width: 1200px;
        preferred-height: 600px;
        in-out property <[EmailRow]> emails;
        in-out property <[MailListEntry]> entries;
        in-out property <string> layout-mode: "default";
        in-out property <bool> compact: false;
        in property <bool> dark: false;
        in-out property <bool> text-mode: false;
        in-out property <bool> source-mode: false;
        in-out property <bool> settings-open: false;
        out property <string> page: view.compact-page;
        out property <bool> list-ring: view.list-focus-visible;
        callback selected(int);
        callback closed();
        init => { Palette.color-scheme = ColorScheme.light; }
        changed dark => { Palette.color-scheme = root.dark ? ColorScheme.dark : ColorScheme.light; }
        public function focus-list() { view.focus-list(true); }
        view := MailView {
            width: parent.width;
            height: parent.height;
            available-width: parent.width;
            emails: root.emails;
            list_entries: root.entries;
            layout-mode: root.layout-mode;
            compact: root.compact;
            text_mode: root.text-mode;
            source_mode: root.source-mode;
            settings_open <=> root.settings-open;
            has_selected: true;
            selected_sender: "Navigation sender";
            selected_subject: "Keyboard navigation test";
            selected_plain_text: "A fictional message for keyboard testing.";
            surface: AppTheme.surface;
            surface-raised: AppTheme.surface-raised;
            sidebar-bg: AppTheme.sidebar-bg;
            heading: AppTheme.heading;
            title-color: AppTheme.heading;
            body-text: AppTheme.body;
            secondary-text: AppTheme.secondary;
            muted-text: AppTheme.muted;
            border: AppTheme.border;
            border-subtle: AppTheme.border-subtle;
            accent: AppTheme.accent;
            accent-soft: AppTheme.accent-soft;
            selected-row: AppTheme.selected-row;
            control-bg: AppTheme.control-bg;
            select_email(id) => { root.selected(id); }
            close_preview => { root.closed(); }
        }
    }
}

// Compile the production resolver against this harness's generated UI types.
#[path = "../src/mail_navigation.rs"]
mod mail_navigation;

struct Headless(Rc<MinimalSoftwareWindow>);
impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn message(id: i32) -> MailListEntry {
    MailListEntry {
        show_row: true,
        email_index: id - 1,
        email: EmailRow {
            id,
            sender: format!("Sender {id}").into(),
            subject: format!("Message {id}").into(),
            account: "Keyboard test".into(),
            initials: "KT".into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn header(key: &str) -> MailListEntry {
    MailListEntry {
        is_header: true,
        group_key: key.into(),
        group_kind: "today".into(),
        expanded: true,
        ..Default::default()
    }
}

#[test]
fn targets_follow_visible_message_order_and_identity() {
    let mut selected = message(7);
    selected.email.selected = true;
    let mut hidden = message(8);
    hidden.show_row = false;
    let entries = ModelRc::new(VecModel::from(vec![
        header("today"),
        selected,
        hidden,
        header("earlier"),
        message(3),
        message(20),
    ]));
    let next = mail_navigation::target(entries.clone(), 7, "next".into(), 1);
    assert_eq!(next.id, 3, "skip group headers and collapsed rows");
    assert_eq!(next.entry_index, 4);
    assert_eq!(next.preceding_headers, 2);
    assert_eq!(next.preceding_messages, 1);
    let previous = mail_navigation::target(entries.clone(), 3, "previous".into(), 1);
    assert_eq!(previous.id, 7);
    assert_eq!(
        mail_navigation::target(entries.clone(), 999, "next".into(), 1).id,
        3,
        "fall back to the selected message after the cursor disappears"
    );
    assert_eq!(
        mail_navigation::target(entries.clone(), 3, "first".into(), 1).id,
        7
    );
    assert_eq!(
        mail_navigation::target(entries.clone(), 7, "last".into(), 1).id,
        20
    );
    assert_eq!(
        mail_navigation::target(entries.clone(), 7, "next".into(), 50).id,
        20
    );
    assert_eq!(
        mail_navigation::target(entries.clone(), 20, "previous".into(), 50).id,
        7
    );
    assert_eq!(
        mail_navigation::target(entries, 7, "previous".into(), 1).id,
        7
    );
    assert_eq!(
        mail_navigation::target(ModelRc::default(), 7, "next".into(), 1).id,
        -1
    );
}

fn key(window: &slint::Window, key: impl Into<slint::SharedString>) {
    let text = key.into();
    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window.dispatch_event(WindowEvent::KeyReleased { text });
}

fn render(window: &Rc<MinimalSoftwareWindow>, width: usize, height: usize, name: &str) {
    slint::platform::update_timers_and_animations();
    window.request_redraw();
    let mut pixels = vec![Rgb8Pixel::default(); width * height];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    if let Some(directory) = std::env::var_os("RUSTMAIL_KEYBOARD_SCREENSHOT_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let bytes = pixels
            .iter()
            .flat_map(|p| [p.r, p.g, p.b])
            .collect::<Vec<_>>();
        image::save_buffer(
            directory.join(format!("{name}.png")),
            &bytes,
            width as u32,
            height as u32,
            image::ColorType::Rgb8,
        )
        .unwrap();
    }
}

#[test]
fn keyboard_navigation_keeps_focus_scrolls_and_respects_text_inputs() {
    use slint::platform::Key;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(window.clone()))).unwrap();
    let ui = MailKeyboardHarness::new().unwrap();
    ui.global::<MailListNavigation>()
        .on_target(mail_navigation::target);
    ui.global::<MotionSettings>().set_enabled(false);
    ui.window().set_size(slint::PhysicalSize::new(500, 400));
    let mut projection = vec![header("today")];
    projection.extend((1..=30).map(message));
    let entries = Rc::new(VecModel::from(projection));
    ui.set_emails(ModelRc::new(VecModel::from(
        (1..=30).map(|id| message(id).email).collect::<Vec<_>>(),
    )));
    ui.set_entries(entries.clone().into());
    let selected = Rc::new(Cell::new(-1));
    let opened = Rc::new(Cell::new(-1));
    let checked = Rc::new(Cell::new((-1, false)));
    let pages = Rc::new(Cell::new(0));
    let count = selected.clone();
    ui.on_selected(move |id| count.set(id));
    let count = opened.clone();
    ui.on_opened(move |id| count.set(id));
    let count = checked.clone();
    ui.on_checked(move |id, value| count.set((id, value)));
    let count = pages.clone();
    ui.on_page_requested(move || count.set(count.get() + 1));
    ui.show().unwrap();
    render(&window, 500, 400, "initial");
    ui.invoke_focus_list();
    assert_eq!(ui.get_cursor(), 1);
    key(ui.window(), Key::Tab);
    assert!(!ui.get_focused(), "Tab reaches the date-group control");
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    key(ui.window(), Key::Tab);
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
    assert!(
        ui.get_focused(),
        "Shift+Tab returns to the list keyboard target"
    );
    key(ui.window(), Key::DownArrow);
    assert_eq!(selected.get(), 2);
    assert!(ui.get_focused());
    key(ui.window(), Key::UpArrow);
    assert_eq!(selected.get(), 1);
    key(ui.window(), Key::PageDown);
    assert!(ui.get_cursor() > 2);
    key(ui.window(), Key::PageUp);
    assert_eq!(ui.get_cursor(), 1);
    ui.set_more(true);
    key(ui.window(), Key::End);
    render(&window, 500, 400, "last-message");
    assert_eq!(ui.get_cursor(), 30);
    assert!(
        ui.get_scroll_y() < -2000.0,
        "End must reveal a virtualized row"
    );
    assert_eq!(
        pages.get(),
        1,
        "keyboard scrolling participates in pagination"
    );
    key(ui.window(), Key::Home);
    render(&window, 500, 400, "keyboard-light");
    assert_eq!(ui.get_cursor(), 1);
    assert!(ui.get_scroll_y() >= -40.0);
    key(ui.window(), Key::Space);
    assert_eq!(checked.get(), (1, true));
    let mut first = entries.row_data(1).unwrap();
    first.email.checked = true;
    entries.set_row_data(1, first);
    key(ui.window(), Key::Space);
    assert_eq!(checked.get(), (1, false));
    key(ui.window(), Key::Return);
    assert_eq!(opened.get(), 1);

    ui.set_preview(false);
    selected.set(-1);
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    key(ui.window(), Key::DownArrow);
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    assert_eq!(
        ui.get_cursor(),
        1,
        "modified navigation is not a mail action"
    );
    key(ui.window(), Key::DownArrow);
    assert_eq!(ui.get_cursor(), 2);
    assert_eq!(
        selected.get(),
        -1,
        "minimal navigation waits for Enter to open"
    );
    let mut inserted = message(99);
    inserted.email_index = 0;
    entries.insert(1, inserted);
    key(ui.window(), Key::DownArrow);
    assert_eq!(
        ui.get_cursor(),
        3,
        "new mail must not move the cursor by index"
    );
    ui.set_dark(true);
    render(&window, 500, 400, "keyboard-dark");

    // A pointer-selected message remains the starting point for keyboard use.
    let position = slint::LogicalPosition::new(220.0, 245.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    assert_eq!(ui.get_cursor(), 2);
    key(ui.window(), Key::DownArrow);
    assert_eq!(ui.get_cursor(), 3);

    let position = slint::LogicalPosition::new(150.0, 32.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    assert!(
        !ui.get_focused(),
        "clicking search moves focus out of the list"
    );
    let cursor = ui.get_cursor();
    key(ui.window(), "x");
    key(ui.window(), Key::Home);
    key(ui.window(), Key::DownArrow);
    key(ui.window(), Key::Return);
    assert_eq!(
        ui.get_cursor(),
        cursor,
        "search navigation must not move the mail cursor"
    );
    key(ui.window(), Key::Tab);
    assert!(!ui.get_focused(), "Tab follows the toolbar controls");

    ui.invoke_focus_list();
    ui.set_scope("Archive".into());
    entries.set_vec(Vec::new());
    key(ui.window(), Key::DownArrow);
    key(ui.window(), Key::Space);
    key(ui.window(), Key::Return);
    assert_eq!(ui.get_cursor(), -1, "empty folders have no keyboard target");
    ui.hide().unwrap();

    let workspace = MailViewKeyboardHarness::new().unwrap();
    workspace
        .global::<MailListNavigation>()
        .on_target(mail_navigation::target);
    workspace.global::<MotionSettings>().set_enabled(false);
    workspace
        .window()
        .set_size(slint::PhysicalSize::new(1200, 600));
    workspace.set_emails(ModelRc::new(VecModel::from(vec![
        message(1).email,
        message(2).email,
    ])));
    workspace.set_entries(ModelRc::new(VecModel::from(vec![message(1), message(2)])));
    let selected = Rc::new(Cell::new(-1));
    let count = selected.clone();
    workspace.on_selected(move |id| count.set(id));
    let closed = Rc::new(Cell::new(0));
    let count = closed.clone();
    workspace.on_closed(move || count.set(count.get() + 1));
    workspace.show().unwrap();
    render(&window, 1200, 600, "workspace");
    assert!(
        !workspace.get_list_ring(),
        "opening the mail view focuses the list without an outline"
    );
    key(workspace.window(), Key::DownArrow);
    assert!(
        workspace.get_list_ring(),
        "keyboard use reveals the outline"
    );
    workspace.invoke_focus_list();
    key(workspace.window(), Key::Return);
    selected.set(-1);
    key(workspace.window(), Key::DownArrow);
    assert_eq!(selected.get(), -1, "Enter transfers focus to the reader");
    key(workspace.window(), Key::F6);
    assert!(workspace.get_list_ring(), "F6 shows where focus landed");
    key(workspace.window(), Key::DownArrow);
    assert_eq!(selected.get(), 2, "F6 restores message-list navigation");
    workspace.set_layout_mode("minimal".into());
    assert!(
        !workspace.get_list_ring(),
        "layout changes restore list focus without an outline"
    );
    render(&window, 1200, 600, "workspace-minimal");
    selected.set(-1);
    key(workspace.window(), Key::DownArrow);
    assert_eq!(workspace.get_page(), "list");
    assert_eq!(selected.get(), -1);
    key(workspace.window(), Key::Return);
    assert_eq!(selected.get(), 2);
    assert_eq!(workspace.get_page(), "message");
    key(workspace.window(), Key::Escape);
    assert_eq!(workspace.get_page(), "list");
    key(workspace.window(), Key::UpArrow);
    key(workspace.window(), Key::Return);
    assert_eq!(selected.get(), 1, "Escape restores usable list focus");

    key(workspace.window(), Key::Escape);
    workspace.set_settings_open(true);
    workspace.set_layout_mode("default".into());
    workspace.set_settings_open(false);
    render(&window, 1200, 600, "workspace-settings-return");
    selected.set(-1);
    key(workspace.window(), Key::DownArrow);
    assert_eq!(selected.get(), 2, "closing Settings restores list focus");

    for (text_mode, source_mode) in [(true, false), (false, true)] {
        workspace.set_text_mode(text_mode);
        workspace.set_source_mode(source_mode);
        workspace.invoke_focus_list();
        key(workspace.window(), Key::Return);
        selected.set(-1);
        key(workspace.window(), Key::DownArrow);
        assert_eq!(selected.get(), -1, "alternate message views retain focus");
        key(workspace.window(), Key::F6);
        key(workspace.window(), Key::UpArrow);
        assert_eq!(selected.get(), 1, "F6 returns from alternate message views");
        key(workspace.window(), Key::DownArrow);
    }
    workspace.set_text_mode(false);
    workspace.set_source_mode(false);
    workspace
        .global::<EmailReader>()
        .set_items(ModelRc::new(VecModel::from(
            (1..=40)
                .map(|index| ReaderItem {
                    name: format!("Fictional Reader paragraph {index}").into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
    workspace.global::<EmailReader>().set_reader_mode(true);
    workspace.invoke_focus_list();
    key(workspace.window(), Key::Return);
    let close_count = closed.get();
    key(workspace.window(), Key::Return);
    assert_eq!(
        closed.get(),
        close_count,
        "Reader mode focuses content instead of the close button"
    );
    key(workspace.window(), Key::End);
    render(&window, 1200, 600, "workspace-reader-mode");
    key(workspace.window(), Key::F6);
    selected.set(-1);
    key(workspace.window(), Key::UpArrow);
    assert_eq!(selected.get(), 1, "F6 returns from Reader mode");
    key(workspace.window(), Key::DownArrow);
    workspace.global::<EmailReader>().set_reader_mode(false);
    workspace.set_compact(true);
    workspace
        .window()
        .set_size(slint::PhysicalSize::new(397, 600));
    render(&window, 397, 600, "workspace-compact");
    selected.set(-1);
    key(workspace.window(), Key::Return);
    assert_eq!(
        selected.get(),
        1,
        "resizing into compact mode preserves navigation"
    );
    assert_eq!(workspace.get_page(), "message");
    key(workspace.window(), Key::Escape);
    workspace.set_compact(false);
    workspace
        .window()
        .set_size(slint::PhysicalSize::new(1200, 600));
    render(&window, 1200, 600, "workspace-expanded");
    selected.set(-1);
    key(workspace.window(), Key::DownArrow);
    assert_eq!(
        selected.get(),
        2,
        "resizing back restores desktop navigation"
    );
    for (width, height, name) in [(800, 1024, "tablet")] {
        workspace.set_compact(true);
        workspace
            .window()
            .set_size(slint::PhysicalSize::new(width, height));
        workspace.invoke_focus_list();
        for dark in [false, true] {
            workspace.set_dark(dark);
            render(
                &window,
                width as usize,
                height as usize,
                &format!("{name}-{}", if dark { "dark" } else { "light" }),
            );
        }
    }
}
