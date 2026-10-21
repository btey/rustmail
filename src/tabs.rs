//! Browser-style tabs.
//!
//! A tab is a saved location: a workspace plus, for mail, the mailbox,
//! search, filter and open conversation. Only the active tab is live. An
//! inactive mail tab keeps a copy of its list rows without message bodies, so
//! switching back restores the list and conversation at once while a
//! background refresh brings them up to date. Rendered email documents are
//! never kept per tab.

use super::*;
use chrono::NaiveDate;
use std::path::{Path, PathBuf};

/// Tabs beyond this many are refused; each inactive mail tab keeps its rows.
const MAX_TABS: usize = 32;
const SAVE_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "workspace", rename_all = "lowercase")]
pub(crate) enum Location {
    Mail {
        #[serde(deserialize_with = "saved_scope")]
        scope: Scope,
        #[serde(default)]
        query: String,
        #[serde(default = "default_filter")]
        filter: String,
        /// Open conversation (thread row id); `None` shows an empty reader.
        #[serde(default)]
        conversation: Option<i32>,
    },
    Calendar {
        date: NaiveDate,
        mode: String,
    },
    Contacts,
    Files,
}

fn default_filter() -> String {
    "All mail".to_owned()
}

/// Tabs saved before account mailboxes were keyed by id named them by display
/// name. Reopen such a tab at the inbox instead of discarding every tab.
fn saved_scope<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Scope, D::Error> {
    let key = <String as serde::Deserialize>::deserialize(deserializer)?;
    Ok(key.parse().unwrap_or_default())
}

impl Location {
    fn inbox() -> Self {
        Self::Mail {
            scope: Scope::UNIFIED_INBOX,
            query: String::new(),
            filter: default_filter(),
            conversation: None,
        }
    }

    fn workspace(&self) -> &'static str {
        match self {
            Self::Mail { .. } => "mail",
            Self::Calendar { .. } => "calendar",
            Self::Contacts => "contacts",
            Self::Files => "files",
        }
    }
}

/// List rows an inactive mail tab returns to, without message bodies.
struct CachedRows {
    messages: Vec<MailMessage>,
    next_cursor: Option<MailCursor>,
}

struct Tab {
    id: i32,
    location: Location,
    /// Title shown while the tab is inactive; the active tab shows live state.
    title: String,
    rows: Option<Rc<CachedRows>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedTab {
    location: Location,
    #[serde(default)]
    title: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedTabs {
    tabs: Vec<SavedTab>,
    #[serde(default)]
    active: usize,
}

impl Default for Location {
    fn default() -> Self {
        Self::inbox()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Closed {
    Missing,
    Inactive,
    /// The active tab closed; the tab at this index becomes active.
    Active(usize),
}

struct TabStrip {
    tabs: Vec<Tab>,
    active: usize,
    next_id: i32,
}

impl TabStrip {
    fn new(location: Location) -> Self {
        let mut strip = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
        };
        strip.insert(0, location, String::new(), None);
        strip
    }

    fn restore(saved: SavedTabs) -> Self {
        let mut tabs = saved.tabs;
        tabs.truncate(MAX_TABS);
        let Some(first) = tabs.first() else {
            return Self::new(Location::inbox());
        };
        let mut strip = Self::new(first.location.clone());
        strip.tabs[0].title = first.title.clone();
        for tab in tabs.into_iter().skip(1) {
            let index = strip.tabs.len();
            strip.insert(index, tab.location, tab.title, None);
        }
        strip.active = saved.active.min(strip.tabs.len() - 1);
        strip
    }

    fn saved(&self) -> SavedTabs {
        SavedTabs {
            tabs: self
                .tabs
                .iter()
                .map(|tab| SavedTab {
                    location: tab.location.clone(),
                    title: tab.title.clone(),
                })
                .collect(),
            active: self.active,
        }
    }

    fn insert(
        &mut self,
        index: usize,
        location: Location,
        title: String,
        rows: Option<Rc<CachedRows>>,
    ) -> usize {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.tabs.insert(
            index,
            Tab {
                id,
                location,
                title,
                rows,
            },
        );
        if index <= self.active && self.tabs.len() > 1 {
            self.active += 1;
        }
        index
    }

    /// Open a tab to the right of the active one, as browsers do.
    fn open(
        &mut self,
        location: Location,
        title: String,
        rows: Option<Rc<CachedRows>>,
    ) -> Option<usize> {
        if self.tabs.len() >= MAX_TABS {
            return None;
        }
        Some(self.insert(self.active + 1, location, title, rows))
    }

    fn index_of(&self, id: i32) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// Close a tab. The last tab is never removed; callers reset it instead.
    fn close(&mut self, id: i32) -> Closed {
        let Some(index) = self.index_of(id) else {
            return Closed::Missing;
        };
        if self.tabs.len() == 1 {
            return Closed::Missing;
        }
        self.tabs.remove(index);
        if index == self.active {
            // Browsers activate the tab that took the closed tab's place.
            self.active = index.min(self.tabs.len() - 1);
            Closed::Active(self.active)
        } else {
            if index < self.active {
                self.active -= 1;
            }
            Closed::Inactive
        }
    }

    fn move_tab(&mut self, id: i32, to: usize) {
        let Some(from) = self.index_of(id) else {
            return;
        };
        let to = to.min(self.tabs.len() - 1);
        if from == to {
            return;
        }
        let active_id = self.tabs[self.active].id;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = self.index_of(active_id).unwrap_or(0);
    }

    fn cycled(&self, delta: isize) -> usize {
        let count = self.tabs.len() as isize;
        (self.active as isize + delta).rem_euclid(count) as usize
    }
}

/// Keyboard commands, matching browser conventions on each platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    New,
    Close,
    Next,
    Previous,
    /// Zero-based tab index.
    Select(usize),
    Last,
}

impl Command {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "new" => Self::New,
            "close" => Self::Close,
            "next" => Self::Next,
            "previous" => Self::Previous,
            "last" => Self::Last,
            index => Self::Select(index.parse::<usize>().ok()?),
        })
    }

    #[cfg(any(test, desktop))]
    pub(crate) fn key(self) -> String {
        match self {
            Self::New => "new".into(),
            Self::Close => "close".into(),
            Self::Next => "next".into(),
            Self::Previous => "previous".into(),
            Self::Last => "last".into(),
            Self::Select(index) => index.to_string(),
        }
    }
}

/// Map a key press to a tab command. `primary` is Command on macOS and
/// Control elsewhere; Control+Tab cycles tabs on every platform.
#[cfg(desktop)]
pub(crate) fn shortcut(
    key: &slint::winit_030::winit::keyboard::Key,
    modifiers: slint::winit_030::winit::keyboard::ModifiersState,
) -> Option<Command> {
    use slint::winit_030::winit::keyboard::{Key, NamedKey};
    let primary = if cfg!(target_os = "macos") {
        modifiers.super_key()
    } else {
        modifiers.control_key()
    };
    let shift = modifiers.shift_key();
    match key {
        Key::Named(NamedKey::Tab) if modifiers.control_key() && !modifiers.alt_key() => {
            Some(if shift {
                Command::Previous
            } else {
                Command::Next
            })
        }
        Key::Named(NamedKey::PageDown) if modifiers.control_key() && !shift => Some(Command::Next),
        Key::Named(NamedKey::PageUp) if modifiers.control_key() && !shift => {
            Some(Command::Previous)
        }
        Key::Character(text) if primary && !modifiers.alt_key() => {
            let text = text.to_lowercase();
            match (text.as_str(), shift) {
                ("t", false) => Some(Command::New),
                ("w", false) => Some(Command::Close),
                // Safari and Chrome on macOS also cycle with Command+Shift+[ ].
                ("[" | "{", true) if cfg!(target_os = "macos") => Some(Command::Previous),
                ("]" | "}", true) if cfg!(target_os = "macos") => Some(Command::Next),
                ("9", false) => Some(Command::Last),
                (digit, false) => digit
                    .parse::<usize>()
                    .ok()
                    .filter(|digit| (1..=8).contains(digit))
                    .map(|digit| Command::Select(digit - 1)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn load(path: &Path) -> Option<SavedTabs> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 256 * 1024 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn save(path: &Path, tabs: &SavedTabs) -> Result<(), Box<dyn std::error::Error>> {
    let parent = path.parent().ok_or("tab state has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, tabs)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

/// Everything a tab switch needs from the mail, calendar and window state.
pub(crate) struct TabContext {
    pub(crate) app: slint::Weak<AppWindow>,
    pub(crate) state: Rc<RefCell<InboxState>>,
    pub(crate) runtime: Rc<tokio::runtime::Runtime>,
    pub(crate) calendar: Rc<RefCell<LocalCalendarState>>,
    pub(crate) mail_pagination_generation: Rc<Cell<u64>>,
    pub(crate) mail_pagination_in_progress: Rc<Cell<bool>>,
    pub(crate) path: PathBuf,
}

struct Tabs {
    context: TabContext,
    strip: RefCell<TabStrip>,
    save_timer: slint::Timer,
}

pub(crate) fn register(app: &AppWindow, context: TabContext) {
    let strip =
        load(&context.path).map_or_else(|| TabStrip::new(Location::inbox()), TabStrip::restore);
    let tabs = Rc::new(Tabs {
        context,
        strip: RefCell::new(strip),
        save_timer: slint::Timer::default(),
    });
    tabs.publish(app);

    // The UI callbacks own the tab state: nothing else keeps it alive, and
    // `Tabs` refers back to the window only weakly, so there is no cycle.
    let global = app.global::<TabsUi>();
    let owner = Rc::clone(&tabs);
    global.on_select(move |id| {
        // Release the strip borrow before activating, which mutates it.
        let index = owner.strip.borrow().index_of(id);
        if let Some(index) = index {
            owner.activate(index);
        }
    });
    let owner = Rc::clone(&tabs);
    global.on_close(move |id| owner.close(id));
    let owner = Rc::clone(&tabs);
    global.on_create(move || owner.create());
    let owner = Rc::clone(&tabs);
    global.on_move(move |id, to| {
        owner
            .strip
            .borrow_mut()
            .move_tab(id, usize::try_from(to).unwrap_or(0));
        owner.changed();
    });
    let owner = Rc::clone(&tabs);
    global.on_open_conversation(move |id, activate| owner.open_conversation(id, activate));
    let owner = Rc::clone(&tabs);
    global.on_command(move |command| {
        if let Some(command) = Command::parse(&command) {
            owner.command(command);
        }
    });
    let owner = Rc::clone(&tabs);
    global.on_reset(move || owner.reset());
    global.on_restore_active(move || {
        let active = tabs.strip.borrow().active;
        tabs.restore(active);
    });
}

impl Tabs {
    fn app(&self) -> Option<AppWindow> {
        self.context.app.upgrade()
    }

    fn command(&self, command: Command) {
        let Some(app) = self.app() else {
            return;
        };
        // Tabs live in the desktop top bar; the modal composer keeps its keys.
        if !app.global::<TabsUi>().get_enabled() || app.get_compose_open() {
            return;
        }
        let (count, active) = {
            let strip = self.strip.borrow();
            (strip.tabs.len(), strip.active)
        };
        match command {
            Command::New => self.create(),
            Command::Close => {
                let id = self.strip.borrow().tabs[active].id;
                self.close(id);
            }
            Command::Next => {
                let index = self.strip.borrow().cycled(1);
                self.activate(index);
            }
            Command::Previous => {
                let index = self.strip.borrow().cycled(-1);
                self.activate(index);
            }
            Command::Last => self.activate(count - 1),
            Command::Select(index) if index < count => self.activate(index),
            Command::Select(_) => {}
        }
    }

    fn create(&self) {
        let opened = self
            .strip
            .borrow_mut()
            .open(Location::inbox(), String::new(), None);
        if let Some(index) = opened {
            self.activate(index);
        }
    }

    /// Open a conversation from the current mail list in a new tab.
    fn open_conversation(&self, id: i32, activate: bool) {
        let Some(app) = self.app() else {
            return;
        };
        let (location, title, rows) = {
            let state = self.context.state.borrow();
            let Some(row) = state.messages.iter().find(|row| row.id == id) else {
                return;
            };
            (
                Location::Mail {
                    scope: state.scope,
                    query: state.query.clone(),
                    filter: state.search_filter.clone(),
                    conversation: Some(id),
                },
                row.subject.clone(),
                cached_rows(&state),
            )
        };
        let opened = self.strip.borrow_mut().open(location, title, Some(rows));
        match opened {
            Some(index) if activate => self.activate(index),
            Some(_) => self.changed(),
            None => app.set_sync_status(UiMessage::detail(
                "At most {} tabs can be open. Close a tab to open another.",
                MAX_TABS,
            )),
        }
    }

    fn close(&self, id: i32) {
        let closed = self.strip.borrow_mut().close(id);
        match closed {
            Closed::Active(index) => {
                self.restore(index);
                self.changed();
            }
            Closed::Inactive => self.changed(),
            Closed::Missing => {
                // Closing the only tab returns it to the inbox.
                let only = {
                    let strip = self.strip.borrow();
                    strip.tabs.len() == 1 && strip.tabs[0].id == id
                };
                if only {
                    {
                        let mut strip = self.strip.borrow_mut();
                        strip.tabs[0].location = Location::inbox();
                        strip.tabs[0].rows = None;
                        strip.tabs[0].title.clear();
                    }
                    self.restore(0);
                    self.changed();
                }
            }
        }
    }

    fn activate(&self, index: usize) {
        {
            let strip = self.strip.borrow();
            if index == strip.active || index >= strip.tabs.len() {
                return;
            }
        }
        self.capture();
        self.strip.borrow_mut().active = index;
        self.restore(index);
        self.changed();
    }

    /// Save the live location into the active tab before leaving it.
    fn capture(&self) {
        let Some(app) = self.app() else {
            return;
        };
        let view = app.get_active_view();
        let (location, title, rows) = match view.as_str() {
            "calendar" => {
                let calendar = self.context.calendar.borrow();
                (
                    Location::Calendar {
                        date: calendar.selected_date,
                        mode: calendar.view_mode.clone(),
                    },
                    String::new(),
                    None,
                )
            }
            "contacts" => (Location::Contacts, String::new(), None),
            "files" => (Location::Files, String::new(), None),
            _ => {
                let state = self.context.state.borrow();
                let conversation = state.selected_id.filter(|_| !state.preview_closed);
                let title = conversation
                    .and_then(|id| state.messages.iter().find(|row| row.id == id))
                    .map(|row| row.subject.clone())
                    .unwrap_or_else(|| app.get_selected_scope_title().to_string());
                (
                    Location::Mail {
                        scope: state.scope,
                        query: state.query.clone(),
                        filter: state.search_filter.clone(),
                        conversation,
                    },
                    title,
                    state.using_core.then(|| cached_rows(&state)),
                )
            }
        };
        let mut strip = self.strip.borrow_mut();
        let active = strip.active;
        let tab = &mut strip.tabs[active];
        tab.location = location;
        tab.title = title;
        tab.rows = rows;
    }

    /// Show a tab's location through the app's normal navigation state.
    fn restore(&self, index: usize) {
        let Some(app) = self.app() else {
            return;
        };
        let (location, mut rows) = {
            let strip = self.strip.borrow();
            let tab = &strip.tabs[index];
            (tab.location.clone(), tab.rows.clone())
        };
        // A tab restored after a restart has no rows of its own; reuse the
        // loaded list when it already shows the same mailbox and search.
        if rows.is_none()
            && let Location::Mail { scope, query, .. } = &location
        {
            let state = self.context.state.borrow();
            if state.using_core
                && state.scope == scope.visible_for(&state.connected_accounts)
                && &state.query == query
            {
                rows = Some(cached_rows(&state));
            }
        }
        app.set_settings_open(false);
        match location {
            Location::Mail {
                scope,
                query,
                filter,
                conversation,
            } => self.restore_mail(&app, scope, query, filter, conversation, rows),
            Location::Calendar { date, mode } => {
                {
                    let mut calendar = self.context.calendar.borrow_mut();
                    calendar.selected_date = date;
                    calendar.visible_month = first_of_month(date);
                    calendar.view_mode = mode;
                }
                if app.get_active_view() == "calendar" {
                    let mut calendar = self.context.calendar.borrow_mut();
                    let core = self.context.state.borrow().core.clone();
                    if let Some(core) = core
                        && let Err(error) = refresh_calendar_events(
                            &core,
                            &self.context.runtime,
                            &mut calendar,
                            &self.context.state.borrow().connected_accounts,
                        )
                    {
                        app.set_sync_status(UiMessage::detail(
                            "Could not load calendar: {}",
                            error,
                        ));
                    }
                    apply_calendar(&app, &calendar, Local::now().date_naive());
                } else {
                    // Entering the calendar loads the visible month.
                    app.set_active_view("calendar".into());
                }
            }
            Location::Contacts | Location::Files => {
                app.set_active_view(location.workspace().into());
            }
        }
    }

    fn restore_mail(
        &self,
        app: &AppWindow,
        scope: Scope,
        query: String,
        filter: String,
        conversation: Option<i32>,
        rows: Option<Rc<CachedRows>>,
    ) {
        let context = &self.context;
        context
            .mail_pagination_generation
            .set(context.mail_pagination_generation.get().wrapping_add(1));
        context.mail_pagination_in_progress.set(false);
        app.set_mail_loading_more(false);
        let using_core = {
            let mut state = context.state.borrow_mut();
            mail_work::invalidate(&mut state);
            state.set_scope(scope);
            state.query = query;
            state.search_filter = filter;
            state.mail_groups.clear();
            state.page = 1;
            state.checked_ids.clear();
            state.preview_closed = conversation.is_none();
            match rows.as_deref() {
                Some(rows) => {
                    state.messages = rows.messages.clone();
                    state.next_cursor = rows.next_cursor;
                    state.selected_id = conversation;
                    state.pending_selection = None;
                }
                None => {
                    state.messages.clear();
                    state.next_cursor = None;
                    state.selected_id = None;
                    state.pending_selection = conversation;
                }
            }
            state.using_core
        };
        app.set_active_view("mail".into());
        match (conversation, rows.is_some()) {
            // Opening the conversation goes through the normal selection
            // path, including mark-as-read when that preference is on.
            (Some(id), true) => app.invoke_select_email(id),
            _ => {
                if let Err(error) = render_current(app, &context.state, &context.runtime) {
                    app.set_render_status(UiMessage::detail("Mail refresh failed: {}", error));
                }
            }
        }
        if using_core
            && let Err(error) =
                refresh_from_source(app, &context.state, &context.runtime, rows.is_some(), &[])
        {
            app.set_render_status(UiMessage::detail("Mail refresh failed: {}", error));
        }
    }

    /// Deleting local data also deletes saved tabs, whose titles are message
    /// subjects. A pending save is cancelled so it cannot write them back.
    fn reset(&self) {
        self.save_timer.stop();
        *self.strip.borrow_mut() = TabStrip::new(Location::inbox());
        if let Some(app) = self.app() {
            self.publish(&app);
        }
        let path = self.context.path.clone();
        self.context.runtime.spawn_blocking(move || {
            if let Err(error) = std::fs::remove_file(&path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                eprintln!("could not delete saved tabs: {error}");
            }
        });
    }

    /// Publish rows, then save once the tab strip has been quiet briefly.
    fn changed(&self) {
        if let Some(app) = self.app() {
            self.publish(&app);
        }
        let saved = self.strip.borrow().saved();
        let path = self.context.path.clone();
        let runtime = Rc::clone(&self.context.runtime);
        self.save_timer
            .start(slint::TimerMode::SingleShot, SAVE_DELAY, move || {
                let path = path.clone();
                let saved = saved.clone();
                runtime.spawn_blocking(move || {
                    if let Err(error) = save(&path, &saved) {
                        eprintln!("could not save tabs: {error}");
                    }
                });
            });
    }

    fn publish(&self, app: &AppWindow) {
        let strip = self.strip.borrow();
        let rows = strip
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabRow {
                id: tab.id,
                workspace: tab.location.workspace().into(),
                title: tab.title.as_str().into(),
                active: index == strip.active,
            })
            .collect::<Vec<_>>();
        app.global::<TabsUi>()
            .set_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    }
}

/// Copy the current list rows without message bodies.
fn cached_rows(state: &InboxState) -> Rc<CachedRows> {
    let mut messages = state.messages.clone();
    release_unselected_bodies(&mut messages, None);
    Rc::new(CachedRows {
        messages,
        next_cursor: state.next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Scope = Scope::Folder(1);
    const B: Scope = Scope::Folder(2);
    const C: Scope = Scope::Folder(3);

    fn mail(scope: Scope) -> Location {
        Location::Mail {
            scope,
            query: String::new(),
            filter: default_filter(),
            conversation: None,
        }
    }

    fn scopes(strip: &TabStrip) -> Vec<Scope> {
        strip
            .tabs
            .iter()
            .filter_map(|tab| match tab.location {
                Location::Mail { scope, .. } => Some(scope),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tabs_open_beside_the_active_tab_and_close_like_a_browser() {
        let mut strip = TabStrip::new(mail(A));
        strip.open(mail(B), String::new(), None);
        strip.open(mail(C), String::new(), None);
        // Each opens right after the active tab, which stays A.
        assert_eq!(scopes(&strip), [A, C, B]);
        assert_eq!(strip.active, 0);

        strip.active = 1;
        let c = strip.tabs[1].id;
        // The tab that takes the closed tab's place becomes active.
        assert_eq!(strip.close(c), Closed::Active(1));
        assert_eq!(scopes(&strip), [A, B]);
        assert_eq!(strip.active, 1);

        // Closing the last tab in the strip activates its left neighbour.
        let b = strip.tabs[1].id;
        assert_eq!(strip.close(b), Closed::Active(0));
        let a = strip.tabs[0].id;
        assert_eq!(strip.close(a), Closed::Missing, "the last tab stays");
        assert_eq!(strip.close(9999), Closed::Missing);
    }

    #[test]
    fn closing_an_inactive_tab_keeps_the_active_tab() {
        let mut strip = TabStrip::new(mail(A));
        strip.open(mail(B), String::new(), None);
        strip.active = 1;
        let a = strip.tabs[0].id;
        assert_eq!(strip.close(a), Closed::Inactive);
        assert_eq!(strip.active, 0);
        assert_eq!(scopes(&strip), [B]);
    }

    #[test]
    fn moving_tabs_keeps_the_active_tab_and_cycling_wraps() {
        let mut strip = TabStrip::new(mail(A));
        strip.open(mail(C), String::new(), None);
        strip.open(mail(B), String::new(), None);
        let a = strip.tabs[0].id;
        strip.move_tab(a, 9);
        assert_eq!(scopes(&strip), [B, C, A]);
        assert_eq!(strip.active, 2);
        assert_eq!(strip.cycled(1), 0);
        assert_eq!(strip.cycled(-1), 1);
    }

    #[test]
    fn the_number_of_tabs_is_bounded() {
        let mut strip = TabStrip::new(mail(A));
        for _ in 1..MAX_TABS {
            assert!(strip.open(mail(B), String::new(), None).is_some());
        }
        assert!(strip.open(mail(B), String::new(), None).is_none());
    }

    #[test]
    fn saved_tabs_round_trip_and_recover_from_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tabs.json");
        assert!(load(&path).is_none());

        let mut strip = TabStrip::new(mail(Scope::UNIFIED_INBOX));
        strip.open(
            Location::Calendar {
                date: NaiveDate::from_ymd_opt(2026, 9, 9).unwrap(),
                mode: "week".into(),
            },
            String::new(),
            None,
        );
        strip.open(
            Location::Mail {
                scope: Scope::Folder(7),
                query: "invoice".into(),
                filter: "Unread".into(),
                conversation: Some(42),
            },
            "Invoice".into(),
            None,
        );
        strip.active = 1;
        save(&path, &strip.saved()).unwrap();
        let restored = TabStrip::restore(load(&path).unwrap());
        assert_eq!(restored.saved(), strip.saved());

        std::fs::write(&path, "{").unwrap();
        assert!(load(&path).is_none());
        let empty = TabStrip::restore(SavedTabs::default());
        assert_eq!(empty.tabs.len(), 1);
        let out_of_range = TabStrip::restore(SavedTabs {
            tabs: vec![SavedTab::default()],
            active: 7,
        });
        assert_eq!(out_of_range.active, 0);

        // Tabs saved before account mailboxes were keyed by id keep their
        // place and reopen at the inbox; the rest of the strip survives.
        std::fs::write(
            &path,
            r#"{"tabs":[
                {"location":{"workspace":"mail","scope":"Person / Sent"},"title":"Sent"},
                {"location":{"workspace":"mail","scope":"Folder:7","query":"invoice"}}
            ],"active":1}"#,
        )
        .unwrap();
        let legacy = TabStrip::restore(load(&path).unwrap());
        assert_eq!(scopes(&legacy), [Scope::UNIFIED_INBOX, Scope::Folder(7)]);
        assert_eq!(legacy.tabs[0].title, "Sent");
        assert_eq!(legacy.active, 1);
    }

    #[test]
    fn commands_parse_from_ui_keys() {
        for command in [
            Command::New,
            Command::Close,
            Command::Next,
            Command::Previous,
            Command::Last,
            Command::Select(3),
        ] {
            assert_eq!(Command::parse(&command.key()), Some(command));
        }
        assert_eq!(Command::parse("bogus"), None);
    }

    /// Drives the registered callbacks the way the tab strip and keyboard
    /// shortcuts do, so a strip borrow held across activation would panic.
    #[test]
    fn selecting_cycling_and_closing_tabs_through_the_ui() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Platform, WindowAdapter};
        struct Headless(Rc<MinimalSoftwareWindow>);
        impl Platform for Headless {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Headless(window))).unwrap();
        let app = AppWindow::new().unwrap();
        app.set_startup_ready(true);
        let runtime = Rc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let (favicons, _) = bounded_ui_channel();
        let (avatars, _) = bounded_ui_channel();
        let state = Rc::new(RefCell::new(InboxState::empty(
            None,
            UiSender::new(favicons, UiWake::new(app.as_weak(), |_| {})),
            None,
            UiSender::new(avatars, UiWake::new(app.as_weak(), |_| {})),
            false,
            false,
            WarmStartCacheWriter::spawn(&runtime, dir.path().join("warm.json")),
        )));
        register(
            &app,
            TabContext {
                app: app.as_weak(),
                state,
                runtime,
                calendar: Rc::new(RefCell::new(LocalCalendarState::new(
                    NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
                ))),
                mail_pagination_generation: Rc::new(Cell::new(0)),
                mail_pagination_in_progress: Rc::new(Cell::new(false)),
                path: dir.path().join("tabs.json"),
            },
        );
        let ui = app.global::<TabsUi>();
        ui.set_enabled(true);
        let active = || {
            ui.get_rows()
                .iter()
                .position(|row| row.active)
                .unwrap()
        };
        let id = |index: usize| ui.get_rows().row_data(index).unwrap().id;

        ui.invoke_create();
        ui.invoke_create();
        assert_eq!(ui.get_rows().row_count(), 3);
        assert_eq!(active(), 2);
        ui.invoke_select(id(0));
        assert_eq!(active(), 0);
        ui.invoke_command("next".into());
        assert_eq!(active(), 1);
        ui.invoke_command("previous".into());
        ui.invoke_command("previous".into());
        assert_eq!(active(), 2, "cycling wraps around");
        ui.invoke_command("0".into());
        assert_eq!(active(), 0);
        ui.invoke_command("close".into());
        assert_eq!(ui.get_rows().row_count(), 2);
        ui.invoke_move(id(0), 1);
        ui.invoke_close(id(0));
        ui.invoke_close(id(0));
        assert_eq!(ui.get_rows().row_count(), 1, "the last tab stays open");

        ui.invoke_create();
        ui.invoke_reset();
        assert_eq!(ui.get_rows().row_count(), 1, "reset leaves one Inbox tab");
        assert_eq!(ui.get_rows().row_data(0).unwrap().title, "");
    }

    #[cfg(desktop)]
    #[test]
    fn shortcuts_follow_browser_conventions() {
        use slint::winit_030::winit::keyboard::{Key, ModifiersState, NamedKey};
        let primary = if cfg!(target_os = "macos") {
            ModifiersState::SUPER
        } else {
            ModifiersState::CONTROL
        };
        let character = |text: &str| Key::Character(text.into());
        assert_eq!(shortcut(&character("t"), primary), Some(Command::New));
        assert_eq!(shortcut(&character("W"), primary), Some(Command::Close));
        assert_eq!(shortcut(&character("1"), primary), Some(Command::Select(0)));
        assert_eq!(shortcut(&character("9"), primary), Some(Command::Last));
        assert_eq!(shortcut(&character("t"), ModifiersState::empty()), None);
        assert_eq!(
            shortcut(&Key::Named(NamedKey::Tab), ModifiersState::CONTROL),
            Some(Command::Next)
        );
        assert_eq!(
            shortcut(
                &Key::Named(NamedKey::Tab),
                ModifiersState::CONTROL | ModifiersState::SHIFT
            ),
            Some(Command::Previous)
        );
        assert_eq!(
            shortcut(&Key::Named(NamedKey::Tab), ModifiersState::empty()),
            None
        );
    }
}
