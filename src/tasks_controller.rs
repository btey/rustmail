//! Tasks UI controller. Callbacks only capture input and schedule work on the
//! runtime; the database is never touched on the UI thread. Every command
//! reloads one `Snapshot` (plain `Send` data) that is applied to the models.
use crate::{
    ActivityRow, AppWindow, ChecklistRow, InboxState, LinkRow, PickerDayRow, TaskCardRow, TaskColumnRow,
    LinkedTask, TaskChipEntry, TaskChips, TaskChipRow, TaskDetailRow, TaskFromMail, TaskNavRow, TaskReplyUi, TaskSettingsUi, TasksUi,
};
use crate::ui_message::AppWindowMessages;
use chrono::{Local, TimeZone};
use rustmail_core::Core;
use rustmail_core::models::{ActionKind, PerformActionArgs};
use rustmail_core::tasks::{
    ColumnKind, ColumnPatch, LinkedTo, NewTask, Task, TaskColumn, TaskDraft, TaskFilter, TaskFocus, TaskLabel,
    TaskPatch,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

const DEFAULT_COLOUR: &str = "#6b7280";

/// How a flat task list is split into sections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Group {
    #[default]
    None,
    Status,
    Project,
    Context,
    Label,
    Due,
}

impl Group {
    fn parse(token: &str) -> Self {
        match token {
            "status" => Self::Status,
            "project" => Self::Project,
            "context" => Self::Context,
            "label" => Self::Label,
            "due" => Self::Due,
            _ => Self::None,
        }
    }

    fn token(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Status => "status",
            Self::Project => "project",
            Self::Context => "context",
            Self::Label => "label",
            Self::Due => "due",
        }
    }
}

/// How the flat lists are ordered; the board keeps its manual order. Each
/// order but Manual has a reversed twin, reached by choosing it again.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Sort {
    /// The order tasks were put in.
    #[default]
    Manual,
    Due,
    DueReversed,
    Priority,
    PriorityReversed,
    Title,
    TitleReversed,
}

impl Sort {
    fn parse(token: &str) -> Self {
        match token {
            "due" => Self::Due,
            "due-reversed" => Self::DueReversed,
            "priority" => Self::Priority,
            "priority-reversed" => Self::PriorityReversed,
            "title" => Self::Title,
            "title-reversed" => Self::TitleReversed,
            _ => Self::Manual,
        }
    }

    fn token(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Due => "due",
            Self::DueReversed => "due-reversed",
            Self::Priority => "priority",
            Self::PriorityReversed => "priority-reversed",
            Self::Title => "title",
            Self::TitleReversed => "title-reversed",
        }
    }

    /// The other direction of the same order.
    fn reversed(self) -> Self {
        match self {
            Self::Manual => Self::Manual,
            Self::Due => Self::DueReversed,
            Self::DueReversed => Self::Due,
            Self::Priority => Self::PriorityReversed,
            Self::PriorityReversed => Self::Priority,
            Self::Title => Self::TitleReversed,
            Self::TitleReversed => Self::Title,
        }
    }

    /// The order without its direction, which names a menu entry.
    fn base(self) -> Self {
        match self {
            Self::DueReversed => Self::Due,
            Self::PriorityReversed => Self::Priority,
            Self::TitleReversed => Self::Title,
            other => other,
        }
    }

    /// Choosing an order: the same one again turns it around.
    fn chosen(self, token: &str) -> Self {
        let wanted = Self::parse(token);
        if self.base() == wanted {
            self.reversed()
        } else {
            wanted
        }
    }

    /// Stable sort: ties keep their manual order. Due dates ascend with
    /// undated tasks last (latest first when reversed, still undated last),
    /// priority runs from high to low (low to high when reversed), titles
    /// ignore case and run A to Z (Z to A when reversed).
    fn apply(self, tasks: &mut [Task]) {
        match self {
            Self::Manual => {}
            Self::Due => tasks.sort_by_key(|t| (t.due_at.is_none(), t.due_at)),
            Self::DueReversed => {
                tasks.sort_by_key(|t| (t.due_at.is_none(), std::cmp::Reverse(t.due_at)))
            }
            Self::Priority => tasks.sort_by_key(|t| std::cmp::Reverse(t.priority)),
            Self::PriorityReversed => tasks.sort_by_key(|t| t.priority),
            Self::Title => tasks.sort_by_key(|t| t.title.to_lowercase()),
            Self::TitleReversed => {
                tasks.sort_by_key(|t| std::cmp::Reverse(t.title.to_lowercase()))
            }
        }
    }
}

/// The text with a line through every letter: Slint's `Text` cannot strike
/// through, so a done task's title carries U+0336 after each character that
/// is not a space.
fn strike_through(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let line = (!c.is_whitespace()).then_some('\u{336}');
            std::iter::once(c).chain(line)
        })
        .collect()
}

/// The name of a "Linked to" choice, as shown in the chip and the menu.
fn linked_label(what: LinkedTo) -> &'static str {
    match what {
        LinkedTo::Mail => "Mail",
        LinkedTo::Event => "Event",
        LinkedTo::Task => "Task",
    }
}

/// The filter chips beyond label and context.
#[derive(Clone, Default)]
struct Extra {
    /// The assignee key (see `assignee_of`).
    assignee: Option<String>,
    linked: Option<LinkedTo>,
    /// Display menu: leave the completed tasks off the board.
    hide_done: bool,
    /// Show the project as a timeline instead of a board or list.
    timeline: bool,
}

/// Who a task is assigned to: a key (the email in lower case, else the
/// name) and the text to show. `None` for an unassigned task.
fn assignee_of(task: &Task) -> Option<(String, String)> {
    let email = task.assignee_email.as_deref().map(str::trim).filter(|e| !e.is_empty());
    let name = task.assignee_name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    match (email, name) {
        (Some(email), name) => Some((email.to_lowercase(), name.unwrap_or(email).to_owned())),
        (None, Some(name)) => Some((name.to_lowercase(), name.to_owned())),
        (None, None) => None,
    }
}

/// Cards beyond this many linked tasks show no link row.
const MAX_CARD_LINK_LOOKUPS: usize = 150;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;


#[derive(Default)]
struct Session {
    selection: String,
    query: String,
    detail_id: Option<i64>,
    /// Filter chips: label and context narrow the current list.
    label: Option<i64>,
    context: Option<i64>,
    /// Filter chip: the assignee (email, else name) the list is narrowed to.
    assignee: Option<String>,
    /// Filter chip: only tasks with a mail, event or task link.
    linked: Option<LinkedTo>,
    /// Project shown as one flat list instead of columns.
    list: bool,
    /// Project shown as a timeline (Gantt).
    timeline: bool,
    /// Sidebar search over project and context names.
    nav: String,
    group: Group,
    sort: Sort,
}

struct DetailSnap {
    id: i64,
    title: String,
    description: String,
    due: String,
    /// First day of work, `YYYY-MM-DD`; empty when unset.
    start: String,
    high: bool,
    move_back: bool,
    done: bool,
    project: String,
    column: String,
    context: String,
    assignee: String,
    labels: String,
    /// `(menu id, title)`: "ctx:<id>" / "ctx:none", "lbl:<id>" (ticked when set).
    context_menu: Vec<(String, String)>,
    label_menu: Vec<(String, String)>,
    checklist: Vec<(i64, String, bool)>,
    /// `(link id, kind token, text, target found, open target)`; the open
    /// target is "task:<id>" for a linked task and empty otherwise.
    /// (link id, kind, text, found, open target, note). The note is "done" or
    /// "open" for a linked task, the start date for an event, else empty.
    links: Vec<(i64, String, String, bool, String, String)>,
    activity: Vec<(String, String)>,
}

#[derive(Clone)]
struct CardSnap {
    id: i64,
    title: String,
    due: String,
    due_warn: bool,
    high: bool,
    labels: String,
    checklist: (i64, i64),
    links: i64,
    done: bool,
    from_mail: bool,
    context: String,
    assignee: String,
    /// Column name, filled in for a project shown as a list.
    status: String,
    /// Whole days since the sent mail while no reply has come; -1 = none.
    no_reply_days: i64,
    replied: bool,
    quick_capture: bool,
    /// The first linked mail or event, for the card's link row.
    link: Option<(String, String)>,
    /// Further linked mails and events beyond the first.
    link_more: i32,
}

struct ColumnSnap {
    id: i64,
    name: String,
    icon: String,
    colour: String,
    count: String,
    over_wip: bool,
    collapsed: bool,
    /// 0 = no limit.
    wip: i64,
    /// Built-in column (Inbox, Done, ...): it cannot be deleted.
    fixed: bool,
    cards: Vec<CardSnap>,
}

struct NavSnap {
    key: String,
    label: String,
    count: i64,
    colour: String,
}

#[derive(Default)]
struct Snapshot {
    selection: String,
    title: String,
    task_count: i64,
    focus: Vec<NavSnap>,
    projects: Vec<NavSnap>,
    contexts: Vec<NavSnap>,
    columns: Vec<ColumnSnap>,
    detail: Option<DetailSnap>,
    /// `(entry id, column name)` for the card move menu.
    move_targets: Vec<(i64, String)>,
    /// Active filter chips `(key, text)` and the "Add filter" entries.
    chips: Vec<(String, String)>,
    list_mode: bool,
    /// The project is shown as a timeline; `timeline` has the chart.
    timeline_mode: bool,
    timeline: Option<crate::tasks_timeline::TimelineSnap>,
    can_switch_view: bool,
    /// `(menu id, title)` for the Group menu; empty when the view is a board.
    group_menu: Vec<(String, String)>,
    /// Token of the order of a flat list (`Sort::token`), for the header row.
    sort: String,
    filter_menu: Vec<(String, String)>,
}

fn colour(hex: &str) -> slint::Color {
    let h = hex.trim_start_matches('#');
    let channel = |i: usize| h.get(i..i + 2).and_then(|c| u8::from_str_radix(c, 16).ok());
    match (channel(0), channel(2), channel(4)) {
        (Some(r), Some(g), Some(b)) => slint::Color::from_rgb_u8(r, g, b),
        _ => colour(DEFAULT_COLOUR),
    }
}

fn today_end_ms() -> i64 {
    let end = Local::now().date_naive().and_hms_opt(23, 59, 59).unwrap();
    Local
        .from_local_datetime(&end)
        .latest()
        .map_or(i64::MAX, |t| t.timestamp_millis())
}

fn iso_date(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn focus_for(key: &str) -> Option<TaskFocus> {
    Some(match key {
        "inbox" => TaskFocus::Inbox,
        "today" => TaskFocus::Today,
        "upcoming" => TaskFocus::Upcoming,
        "waiting" => TaskFocus::WaitingFor,
        "email" => TaskFocus::FromEmail,
        "someday" => TaskFocus::Someday,
        _ => return None,
    })
}

fn card(
    task: &Task,
    labels: &HashMap<i64, TaskLabel>,
    contexts: &HashMap<i64, String>,
    today_end: i64,
) -> CardSnap {
    CardSnap {
        id: task.id,
        title: task.title.clone(),
        due: task.due_at.map(iso_date).unwrap_or_default(),
        due_warn: task.completed_at.is_none() && task.due_at.is_some_and(|d| d <= today_end),
        high: task.priority >= 3,
        labels: task
            .label_ids
            .iter()
            .filter_map(|id| labels.get(id).map(|l| l.name.as_str()))
            .collect::<Vec<_>>()
            .join(", "),
        checklist: (task.checklist_done, task.checklist_total),
        links: task.link_count,
        done: task.completed_at.is_some(),
        from_mail: task.source == rustmail_core::tasks::TaskSource::Mail,
        context: task
            .context_id
            .and_then(|id| contexts.get(&id).cloned())
            .unwrap_or_default(),
        assignee: initials(
            task.assignee_name.as_deref().filter(|n| !n.trim().is_empty()),
            task.assignee_email.as_deref(),
        ),
        status: String::new(),
        no_reply_days: -1,
        replied: false,
        quick_capture: task.quick_capture,
        link: None,
        link_more: 0,
    }
}

/// Up to two capital letters for an avatar: the first letters of the first
/// and last word of the name, or the first letter of the email.
fn initials(name: Option<&str>, email: Option<&str>) -> String {
    let from_name = name.map(|n| {
        let mut words = n.split_whitespace().filter_map(|w| w.chars().next());
        let first = words.next();
        let last = words.next_back();
        first.into_iter().chain(last).collect::<String>()
    });
    from_name
        .filter(|i| !i.is_empty())
        .or_else(|| email.and_then(|e| e.chars().next()).map(String::from))
        .unwrap_or_default()
        .to_uppercase()
}

const DUE_ORDER: [&str; 5] = ["Overdue", "Today", "Next 7 days", "Later", "No due date"];

struct GroupNames<'a> {
    columns: &'a HashMap<i64, String>,
    projects: &'a HashMap<i64, String>,
    contexts: &'a HashMap<i64, String>,
    labels: &'a HashMap<i64, TaskLabel>,
}

/// The sections a task belongs to. A task with several labels appears in
/// each label's section; an empty key list never happens.
fn group_keys(task: &Task, group: Group, names: &GroupNames, today_end: i64) -> Vec<String> {
    let one = |text: String| vec![text];
    match group {
        Group::None => one(String::new()),
        Group::Status => one(names.columns.get(&task.column_id).cloned().unwrap_or_default()),
        Group::Project => one(names.projects.get(&task.project_id).cloned().unwrap_or_default()),
        Group::Context => one(
            task.context_id
                .and_then(|c| names.contexts.get(&c))
                .map_or_else(|| "No context".to_owned(), |n| format!("@{n}")),
        ),
        Group::Label => {
            let found: Vec<String> = task
                .label_ids
                .iter()
                .filter_map(|l| names.labels.get(l).map(|l| l.name.clone()))
                .collect();
            if found.is_empty() { one("No label".to_owned()) } else { found }
        }
        Group::Due => one(
            match task.due_at {
                None => "No due date",
                Some(d) if d < today_end - DAY_MS + 1 => "Overdue",
                Some(d) if d <= today_end => "Today",
                Some(d) if d <= today_end + 7 * DAY_MS => "Next 7 days",
                Some(_) => "Later",
            }
            .to_owned(),
        ),
    }
}

fn column_count(column: &TaskColumn) -> String {
    match column.wip_limit {
        Some(limit) => format!("{} / {limit}", column.task_count),
        None => column.task_count.to_string(),
    }
}

async fn load_detail(
    core: &Core,
    id: i64,
    projects: &[rustmail_core::tasks::TaskProject],
) -> Option<(DetailSnap, Vec<(i64, String)>)> {
    let task = core.task_get(id).await.ok()?;
    let columns = core.task_columns(task.project_id).await.ok()?;
    let checklist = core.task_checklist(id).await.ok()?;
    let links = core.task_links(id).await.ok()?;
    let activity = core.task_activity(id).await.ok()?;
    let column = columns.iter().find(|c| c.id == task.column_id)?;
    let contexts = core.task_contexts().await.ok()?;
    let all_labels = core.task_labels().await.ok()?;
    let mut context_menu = vec![("ctx:none".to_owned(), "No context".to_owned())];
    context_menu.extend(contexts.iter().map(|c| {
        let tick = if task.context_id == Some(c.id) { "✓ " } else { "" };
        (format!("ctx:{}", c.id), format!("{tick}{}", c.name))
    }));
    let label_menu = all_labels
        .iter()
        .map(|l| {
            let tick = if task.label_ids.contains(&l.id) { "✓ " } else { "" };
            (format!("lbl:{}", l.id), format!("{tick}{}", l.name))
        })
        .collect();
    let labels = all_labels
        .iter()
        .filter(|l| task.label_ids.contains(&l.id))
        .map(|l| l.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let context = contexts
        .iter()
        .find(|c| Some(c.id) == task.context_id)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    // An event link holds the calendar UID; show the event's title.
    let mut event_titles = HashMap::new();
    for l in &links {
        if let (rustmail_core::tasks::LinkTarget::Event, Some(uid)) = (&l.target, &l.link.event_uid)
            && let Ok(Some(title)) = core.task_event_title(uid.clone()).await
        {
            event_titles.insert(uid.clone(), title);
        }
    }
    // A linked task shows whether it is done; an event shows its start day.
    let mut notes: HashMap<i64, String> = HashMap::new();
    for l in &links {
        use rustmail_core::tasks::LinkTarget;
        let note = match &l.target {
            LinkTarget::Task { task_id, .. } => core.task_get(*task_id).await.ok().map(|t| {
                if t.completed_at.is_some() { "done" } else { "open" }.to_owned()
            }),
            LinkTarget::Event => match &l.link.event_uid {
                Some(uid) => core.task_event_start(uid.clone()).await.ok().flatten().map(iso_date),
                None => None,
            },
            _ => None,
        };
        if let Some(note) = note {
            notes.insert(l.link.id, note);
        }
    }
    let links = links
        .into_iter()
        .map(|l| {
            use rustmail_core::tasks::LinkTarget;
            let (text, found, open) = match &l.target {
                LinkTarget::Mail {
                    subject, thread_id, ..
                } => (subject.clone(), true, format!("mail:{thread_id}")),
                LinkTarget::Task { task_id, title } => {
                    (title.clone(), true, format!("task:{task_id}"))
                }
                LinkTarget::Event => {
                    let uid = l.link.event_uid.clone().unwrap_or_default();
                    let open = format!("event:{uid}");
                    (event_titles.get(&uid).cloned().unwrap_or(uid), true, open)
                }
                LinkTarget::MailNotFound | LinkTarget::TaskNotFound => {
                    (String::new(), false, String::new())
                }
            };
            let note = notes.remove(&l.link.id).unwrap_or_default();
            (l.link.id, l.link.kind.as_str().to_owned(), text, found, open, note)
        })
        .collect();
    let detail = DetailSnap {
        id,
        title: task.title.clone(),
        description: task.description.clone(),
        due: task.due_at.map(iso_date).unwrap_or_default(),
        start: task.start_at.map(iso_date).unwrap_or_default(),
        high: task.priority >= 3,
        move_back: task.move_back_on_reply,
        done: task.completed_at.is_some(),
        project: projects
            .iter()
            .find(|p| p.id == task.project_id)
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        column: column.name.clone(),
        context,
        assignee: match (&task.assignee_name, &task.assignee_email) {
            (Some(n), Some(e)) if !n.is_empty() => format!("{n} <{e}>"),
            (_, Some(e)) => e.clone(),
            _ => String::new(),
        },
        labels,
        context_menu,
        label_menu,
        checklist: checklist.into_iter().map(|c| (c.id, c.text, c.done)).collect(),
        links,
        activity: activity
            .into_iter()
            .map(|a| {
                let at = Local
                    .timestamp_millis_opt(a.at)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                (a.text, at)
            })
            .collect(),
    };
    Some((detail, columns.into_iter().map(|c| (c.id, c.name)).collect()))
}

async fn load(
    core: &Core,
    selection: &str,
    query: &str,
    detail_id: Option<i64>,
    chips: (Option<i64>, Option<i64>, bool, Group, Sort, Extra),
) -> rustmail_core::error::Result<Snapshot> {
    core.tasks_open().await?;
    let today_end = today_end_ms();
    let projects = core.task_projects().await?;
    let contexts = core.task_contexts().await?;
    let labels = core.task_labels().await?;
    let counts = core.task_counts(today_end).await?;
    let label_map: HashMap<i64, TaskLabel> = labels.into_iter().map(|l| (l.id, l)).collect();
    let context_map: HashMap<i64, String> =
        contexts.iter().map(|c| (c.id, c.name.clone())).collect();

    let (kind, id) = selection.split_once(':').unwrap_or(("focus", "inbox"));
    let mut selection = selection.to_owned();
    let mut project_id = None;
    if kind == "project" {
        project_id = id.parse::<i64>().ok().filter(|p| projects.iter().any(|x| x.id == *p));
        if project_id.is_none() {
            project_id = projects.first().map(|p| p.id);
        }
        if let Some(p) = project_id {
            selection = format!("project:{p}");
        }
    }
    let mut filter = TaskFilter {
        today_end_ms: today_end,
        text: Some(query.to_owned()).filter(|q| !q.trim().is_empty()),
        ..Default::default()
    };
    let (label_chip, context_chip, list_mode, group, sort, extra) = chips;
    let assignee_chip = extra.assignee;
    filter.label_id = label_chip.filter(|l| label_map.contains_key(l));
    filter.linked_to = extra.linked;
    let hide_done = extra.hide_done;
    let want_timeline = extra.timeline;
    if kind != "context" {
        filter.context_id = context_chip.filter(|c| context_map.contains_key(c));
    }
    let mut title = String::new();
    match kind {
        "project" => {
            filter.project_id = project_id;
            title = projects
                .iter()
                .find(|p| Some(p.id) == project_id)
                .map(|p| p.name.clone())
                .unwrap_or_default();
        }
        "context" => {
            let cid = id.parse::<i64>().ok();
            filter.context_id = cid;
            title = cid
                .and_then(|c| context_map.get(&c).map(|n| format!("@{n}")))
                .unwrap_or_default();
        }
        _ => filter.focus = focus_for(id),
    }

    let mut snap = Snapshot {
        selection: selection.clone(),
        title,
        ..Default::default()
    };
    if let Some(l) = filter.label_id.and_then(|l| label_map.get(&l)) {
        snap.chips.push(("label".into(), format!("Label: {}", l.name)));
    }
    if let Some(n) = filter.context_id.and_then(|c| context_map.get(&c)) {
        snap.chips.push(("context".into(), format!("Context: @{n}")));
    }
    match filter.linked_to {
        Some(what) => snap
            .chips
            .push(("linked".into(), format!("Linked to: {}", linked_label(what)))),
        None => snap.filter_menu.extend(
            [LinkedTo::Mail, LinkedTo::Event, LinkedTo::Task]
                .map(|w| (format!("linked:{}", w.token()), format!("Linked to: {}", linked_label(w)))),
        ),
    }
    let mut labels_sorted: Vec<&TaskLabel> = label_map.values().collect();
    labels_sorted.sort_by(|a, b| a.name.cmp(&b.name));
    if filter.label_id.is_none() {
        snap.filter_menu
            .extend(labels_sorted.iter().map(|l| (format!("label:{}", l.id), format!("Label: {}", l.name))));
    }
    if kind != "context" && filter.context_id.is_none() {
        snap.filter_menu
            .extend(contexts.iter().map(|c| (format!("context:{}", c.id), format!("Context: @{}", c.name))));
    }
    let focus_count = |key: &str| match key {
        "inbox" => counts.inbox,
        "today" => counts.today,
        "upcoming" => counts.upcoming,
        "waiting" => counts.waiting_for,
        "email" => counts.from_email,
        _ => counts.someday,
    };
    snap.focus = ["inbox", "today", "upcoming", "waiting", "email", "someday"]
        .into_iter()
        .map(|k| NavSnap {
            key: format!("focus:{k}"),
            label: String::new(),
            count: focus_count(k),
            colour: DEFAULT_COLOUR.into(),
        })
        .collect();
    let per = |pairs: &[(i64, i64)], id: i64| {
        pairs.iter().find(|(i, _)| *i == id).map_or(0, |(_, n)| *n)
    };
    snap.projects = projects
        .iter()
        .map(|p| NavSnap {
            key: format!("project:{}", p.id),
            label: p.name.clone(),
            count: per(&counts.per_project, p.id),
            colour: if p.colour.is_empty() { DEFAULT_COLOUR.into() } else { p.colour.clone() },
        })
        .collect();
    snap.contexts = contexts
        .iter()
        .map(|c| NavSnap {
            key: format!("context:{}", c.id),
            label: c.name.clone(),
            count: per(&counts.per_context, c.id),
            colour: DEFAULT_COLOUR.into(),
        })
        .collect();

    let timeline_mode = kind == "project" && want_timeline;
    let board = kind == "project" && filter.text.is_none() && !list_mode && !timeline_mode;
    snap.list_mode = kind == "project" && list_mode && !timeline_mode;
    snap.timeline_mode = timeline_mode;
    snap.can_switch_view = kind == "project";
    filter.include_done = (board || timeline_mode) && !hide_done;
    let mut tasks = core.task_list(filter).await?;
    // Assignee chip: the people on the tasks listed here, one entry each.
    let mut assignees: Vec<(String, String)> = Vec::new();
    for task in &tasks {
        if let Some((key, name)) = assignee_of(task)
            && !assignees.iter().any(|(k, _)| *k == key)
        {
            assignees.push((key, name));
        }
    }
    assignees.sort_by_key(|(_, name)| name.to_lowercase());
    match assignee_chip
        .as_deref()
        .and_then(|key| assignees.iter().find(|(k, _)| k == key))
    {
        Some((key, name)) => {
            snap.chips.push(("assignee".into(), format!("Assignee: {name}")));
            tasks.retain(|t| assignee_of(t).is_some_and(|(k, _)| k == *key));
        }
        None => snap.filter_menu.extend(
            assignees
                .iter()
                .map(|(key, name)| (format!("assignee:{key}"), format!("Assignee: {name}"))),
        ),
    }
    snap.task_count = tasks.iter().filter(|t| t.completed_at.is_none()).count() as i64;
    if board {
        let columns = core.task_columns(project_id.unwrap_or_default()).await?;
        snap.move_targets = columns.iter().map(|c| (c.id, c.name.clone())).collect();
        snap.columns = columns
            .iter()
            .map(|c| ColumnSnap {
                id: c.id,
                name: c.name.clone(),
                icon: c.icon.clone(),
                colour: if c.colour.is_empty() { DEFAULT_COLOUR.into() } else { c.colour.clone() },
                count: column_count(c),
                over_wip: c.over_wip,
                collapsed: c.collapsed,
                wip: c.wip_limit.unwrap_or(0),
                fixed: c.kind != ColumnKind::Custom,
                cards: tasks
                    .iter()
                    .filter(|t| t.column_id == c.id)
                    .map(|t| card(t, &label_map, &context_map, today_end))
                    .collect(),
            })
            .collect();
    } else if timeline_mode {
        let dependencies = core.task_dependencies(project_id.unwrap_or_default()).await?;
        snap.timeline = Some(crate::tasks_timeline::build(
            &tasks,
            Local::now().date_naive(),
            &dependencies,
        ));
    } else if !tasks.is_empty() || kind != "project" {
        // Focus, context and search views are one flat list, optionally
        // split into sections.
        let column_names: HashMap<i64, String> = match (kind == "project", project_id) {
            (true, Some(p)) => core
                .task_columns(p)
                .await?
                .into_iter()
                .map(|c| (c.id, c.name))
                .collect(),
            _ => HashMap::new(),
        };
        let project_names: HashMap<i64, String> =
            projects.iter().map(|p| (p.id, p.name.clone())).collect();
        let mut options = vec![Group::None];
        if kind == "project" {
            options.push(Group::Status);
        } else {
            options.push(Group::Project);
        }
        options.extend([Group::Context, Group::Label, Group::Due]);
        let group = if options.contains(&group) { group } else { Group::None };
        let titles = |g: Group| match g {
            Group::None => "No grouping",
            Group::Status => "Group by status",
            Group::Project => "Group by project",
            Group::Context => "Group by context",
            Group::Label => "Group by label",
            Group::Due => "Group by due date",
        };
        snap.sort = sort.token().to_owned();
        snap.group_menu = options
            .iter()
            .map(|g| {
                let tick = if *g == group { "✓ " } else { "" };
                (g.token().to_owned(), format!("{tick}{}", titles(*g)))
            })
            .chain(
                [
                    (Sort::Manual, "Manual order"),
                    (Sort::Due, "Sort by due date"),
                    (Sort::Priority, "Sort by priority"),
                    (Sort::Title, "Sort by title"),
                ]
                .map(|(option, title)| {
                    // A tick marks the active order, an arrow a reversed one.
                    let tick = match (option == sort.base(), option == sort) {
                        (true, true) => "✓ ",
                        (true, false) => "✓↓ ",
                        _ => "",
                    };
                    (format!("sort:{}", option.token()), format!("{tick}{title}"))
                }),
            )
            .collect();
        sort.apply(&mut tasks);
        let cards: Vec<(&Task, CardSnap)> = tasks
            .iter()
            .map(|t| {
                let mut k = card(t, &label_map, &context_map, today_end);
                if list_mode {
                    k.status = column_names.get(&t.column_id).cloned().unwrap_or_default();
                }
                (t, k)
            })
            .collect();
        let names = GroupNames {
            columns: &column_names,
            projects: &project_names,
            contexts: &context_map,
            labels: &label_map,
        };
        let mut sections: Vec<(String, Vec<CardSnap>)> = Vec::new();
        for (task, snap_card) in cards {
            for key in group_keys(task, group, &names, today_end) {
                match sections.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, list)) => list.push(snap_card.clone()),
                    None => sections.push((key, vec![snap_card.clone()])),
                }
            }
        }
        if group == Group::Due {
            sections.sort_by_key(|(k, _)| DUE_ORDER.iter().position(|d| d == k));
        }
        snap.columns = sections
            .into_iter()
            .map(|(name, cards)| ColumnSnap {
                id: -1,
                name,
                icon: String::new(),
                colour: DEFAULT_COLOUR.into(),
                count: cards.len().to_string(),
                over_wip: false,
                collapsed: false,
                wip: 0,
                fixed: true,
                cards,
            })
            .collect();
    }
    // Each card shows its first linked mail or event. One read per linked
    // task, so a very large board shows the rows for the first cards only.
    let mut looked_up = 0;
    for k in snap.columns.iter_mut().flat_map(|c| c.cards.iter_mut()) {
        if k.links == 0 || looked_up >= MAX_CARD_LINK_LOOKUPS {
            continue;
        }
        looked_up += 1;
        let all = core.task_links(k.id).await?;
        let shown = |l: &rustmail_core::tasks::ResolvedLink| {
            matches!(
                l.target,
                rustmail_core::tasks::LinkTarget::Mail { .. } | rustmail_core::tasks::LinkTarget::Event
            )
        };
        k.link_more = all.iter().filter(|l| shown(l)).count().saturating_sub(1) as i32;
        let found = all.into_iter().find_map(|l| {
            use rustmail_core::tasks::LinkTarget;
            let text = match &l.target {
                LinkTarget::Mail { subject, .. } => subject.clone(),
                LinkTarget::Event => l.link.event_uid.clone().unwrap_or_default(),
                _ => return None,
            };
            Some((l.link.kind.as_str().to_owned(), text, matches!(l.target, LinkTarget::Event)))
        });
        k.link = match found {
            // An event link holds the calendar UID; show the event's title.
            Some((kind, uid, true)) => {
                let title = core.task_event_title(uid.clone()).await?;
                Some((kind, title.unwrap_or(uid)))
            }
            Some((kind, text, false)) => Some((kind, text)),
            None => None,
        };
    }
    let waiting: HashMap<i64, i64> = core
        .task_no_reply_days(chrono::Utc::now().timestamp_millis())
        .await?
        .into_iter()
        .collect();
    if !waiting.is_empty() {
        for k in snap.columns.iter_mut().flat_map(|c| c.cards.iter_mut()) {
            if let Some(days) = waiting.get(&k.id) {
                k.no_reply_days = *days;
            }
        }
    }
    let replied: std::collections::HashSet<i64> = core.task_replied().await?.into_iter().collect();
    if !replied.is_empty() {
        for k in snap.columns.iter_mut().flat_map(|c| c.cards.iter_mut()) {
            k.replied = replied.contains(&k.id);
        }
    }
    if let Some(id) = detail_id
        && let Some((detail, targets)) = load_detail(core, id, &projects).await
    {
        // The drawer's task may live in another project than the board.
        if snap.move_targets.is_empty() || !board {
            snap.move_targets = targets;
        }
        snap.detail = Some(detail);
    }
    Ok(snap)
}

/// Keep the projects and contexts whose name contains `text` (ignoring
/// case); the open list always stays so the selection never disappears.
fn filter_nav(snap: &mut Snapshot, text: &str) {
    let text = text.trim().to_lowercase();
    if text.is_empty() {
        return;
    }
    let selection = snap.selection.clone();
    let keep = |row: &NavSnap| row.key == selection || row.label.to_lowercase().contains(&text);
    snap.projects.retain(keep);
    snap.contexts.retain(keep);
}

fn nav_rows(rows: Vec<NavSnap>) -> ModelRc<TaskNavRow> {
    ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|r| TaskNavRow {
                key: r.key.into(),
                label: r.label.into(),
                count: r.count.to_string().into(),
                colour: colour(&r.colour),
            })
            .collect::<Vec<_>>(),
    ))
}

fn apply(ui: &TasksUi<'_>, snap: Snapshot) {
    ui.set_selection(snap.selection.into());
    ui.set_title(snap.title.into());
    ui.set_task_count(snap.task_count as i32);
    ui.set_focus_rows(nav_rows(snap.focus));
    ui.set_projects(nav_rows(snap.projects));
    ui.set_contexts(nav_rows(snap.contexts));
    ui.set_today_label(Local::now().format("%a, %b %-d").to_string().into());
    ui.set_week_label(Local::now().format("W%V").to_string().into());
    ui.set_list_mode(snap.list_mode);
    ui.set_timeline_mode(snap.timeline_mode);
    match snap.timeline {
        Some(timeline) => {
            ui.set_timeline_title(timeline.title.into());
            ui.set_timeline_today(timeline.today_index);
            ui.set_timeline_undated(timeline.undated as i32);
            ui.set_timeline_days(ModelRc::new(VecModel::from(
                timeline
                    .days
                    .into_iter()
                    .map(|d| crate::TimelineDayRow {
                        label: d.label.into(),
                        weekend: d.weekend,
                        today: d.today,
                        week: d.week.into(),
                    })
                    .collect::<Vec<_>>(),
            )));
            ui.set_timeline_edges(ModelRc::new(VecModel::from(
                timeline
                    .edges
                    .iter()
                    .map(|&(from_bar, to_bar)| crate::TimelineEdgeRow { from_bar, to_bar })
                    .collect::<Vec<_>>(),
            )));
            ui.set_timeline_bars(ModelRc::new(VecModel::from(
                timeline
                    .bars
                    .into_iter()
                    .map(|b| crate::TimelineBarRow {
                        id: b.id as i32,
                        title: b.title.into(),
                        start_text: b.start_text.into(),
                        due_text: b.due_text.into(),
                        first_day: b.first_day,
                        last_day: b.last_day,
                        milestone: b.milestone,
                        done: b.done,
                        overdue: b.overdue,
                    })
                    .collect::<Vec<_>>(),
            )));
        }
        None => {
            ui.set_timeline_days(ModelRc::default());
            ui.set_timeline_bars(ModelRc::default());
            ui.set_timeline_edges(ModelRc::default());
        }
    }
    ui.set_sort(snap.sort.into());
    ui.set_can_switch_view(snap.can_switch_view);
    // The snapshot carries English names; the menu shows the translated ones,
    // keeping the tick of the active entry.
    let group_menu = snap
        .group_menu
        .into_iter()
        .map(|(id, title)| {
            // Titles start with letters; a tick and an arrow come before.
            let tick: String = title.chars().take_while(|c| !c.is_alphabetic()).collect();
            let name = ui.invoke_group_title(id.as_str().into());
            crate::ActionMenuEntry {
                id: id.into(),
                title: format!("{tick}{name}").into(),
                icon: Default::default(),
                disabled: false,
                destructive: false,
            }
        })
        .collect::<Vec<_>>();
    ui.set_group_menu(ModelRc::new(VecModel::from(group_menu)));
    ui.set_filter_chips(ModelRc::new(VecModel::from(
        snap.chips
            .into_iter()
            .map(|(key, text)| TaskChipRow { key: key.into(), text: text.into() })
            .collect::<Vec<_>>(),
    )));
    ui.set_filter_menu(ModelRc::new(VecModel::from(
        snap.filter_menu
            .into_iter()
            .map(|(id, title)| crate::ActionMenuEntry {
                id: id.into(),
                title: title.into(),
                icon: Default::default(),
                disabled: false,
                destructive: false,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_card_menu(ModelRc::new(VecModel::from(
        snap.move_targets
            .into_iter()
            .map(|(id, name)| crate::ActionMenuEntry {
                id: format!("move:{id}").into(),
                title: name.into(),
                icon: Default::default(),
                disabled: false,
                destructive: false,
            })
            .collect::<Vec<_>>(),
    )));
    let columns = snap
        .columns
        .into_iter()
        .map(|c| TaskColumnRow {
            id: c.id as i32,
            name: c.name.into(),
            icon: c.icon.into(),
            colour: colour(&c.colour),
            count: c.count.into(),
            over_wip: c.over_wip,
            collapsed: c.collapsed,
            wip_limit: c.wip as i32,
            fixed: c.fixed,
            cards: ModelRc::new(VecModel::from(
                c.cards
                    .into_iter()
                    .map(|k| TaskCardRow {
                        id: k.id as i32,
                        struck_title: if k.done { strike_through(&k.title) } else { String::new() }.into(),
                        title: k.title.into(),
                        due: k.due.into(),
                        due_warn: k.due_warn,
                        high_priority: k.high,
                        labels: k.labels.into(),
                        checklist_total: k.checklist.1 as i32,
                        checklist_done: k.checklist.0 as i32,
                        link_count: k.links as i32,
                        done: k.done,
                        from_mail: k.from_mail,
                        context: k.context.into(),
                        assignee: k.assignee.into(),
                        status: k.status.into(),
                        no_reply_days: k.no_reply_days as i32,
                        replied: k.replied,
                        quick_capture: k.quick_capture,
                        link_kind: k.link.as_ref().map(|l| l.0.clone()).unwrap_or_default().into(),
                        link_text: k.link.map(|l| l.1).unwrap_or_default().into(),
                        link_more: k.link_more,
                    })
                    .collect::<Vec<_>>(),
            )),
        })
        .collect::<Vec<_>>();
    ui.set_columns(ModelRc::new(VecModel::from(columns)));
    match snap.detail {
        Some(d) => {
            ui.set_detail(TaskDetailRow {
                id: d.id as i32,
                title: d.title.into(),
                description: d.description.into(),
                due: d.due.into(),
                start: d.start.into(),
                high_priority: d.high,
                move_back: d.move_back,
                done: d.done,
                project: d.project.into(),
                column: d.column.into(),
                context: d.context.into(),
                assignee: d.assignee.into(),
                labels: d.labels.into(),
            });
            let entries = |rows: Vec<(String, String)>| {
                ModelRc::new(VecModel::from(
                    rows.into_iter()
                        .map(|(id, title)| crate::ActionMenuEntry {
                            id: id.into(),
                            title: title.into(),
                            icon: Default::default(),
                            disabled: false,
                            destructive: false,
                        })
                        .collect::<Vec<_>>(),
                ))
            };
            ui.set_context_menu(entries(d.context_menu));
            ui.set_label_menu(entries(d.label_menu));
            ui.set_checklist(ModelRc::new(VecModel::from(
                d.checklist
                    .into_iter()
                    .map(|(id, text, done)| ChecklistRow {
                        id: id as i32,
                        text: text.into(),
                        done,
                    })
                    .collect::<Vec<_>>(),
            )));
            ui.set_links(ModelRc::new(VecModel::from(
                d.links
                    .into_iter()
                    .map(|(id, kind, text, found, open, note)| LinkRow {
                        note: note.into(),
                        id: id as i32,
                        thread_id: open
                            .strip_prefix("mail:")
                            .and_then(|thread| thread.parse().ok())
                            .unwrap_or(-1),
                        open: open.into(),
                        kind: kind.into(),
                        text: text.into(),
                        found,
                    })
                    .collect::<Vec<_>>(),
            )));
            ui.set_activity(ModelRc::new(VecModel::from(
                d.activity
                    .into_iter()
                    .map(|(text, at)| ActivityRow {
                        text: text.into(),
                        at: at.into(),
                    })
                    .collect::<Vec<_>>(),
            )));
            ui.set_detail_open(true);
        }
        None => ui.set_detail_open(false),
    }
}

struct Ctx {
    weak: slint::Weak<AppWindow>,
    state: Rc<RefCell<InboxState>>,
    session: RefCell<Session>,
    generation: Arc<AtomicU64>,
    handle: tokio::runtime::Handle,
}

pub(crate) fn register(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
) {
    let ctx = Rc::new(Ctx {
        weak: app.as_weak(),
        state: Rc::clone(state),
        session: RefCell::new(Session {
            selection: "focus:inbox".into(),
            ..Default::default()
        }),
        generation: Arc::new(AtomicU64::new(0)),
        handle: runtime.handle().clone(),
    });
    let command = Rc::clone(&ctx);
    let runtime_for_mail = Rc::clone(runtime);
    app.global::<TasksUi>().on_command(move |action, a, b| {
        if action == "board-key" {
            if let Some(app) = command.weak.upgrade() {
                board_key(&app, &command, a.as_str());
            }
            return;
        }
        if action == "display" {
            if let Some(app) = command.weak.upgrade() {
                let ui = app.global::<TasksUi>();
                match a.as_str() {
                    "completed" => ui.set_show_completed(!ui.get_show_completed()),
                    "compact" => ui.set_compact_cards(!ui.get_compact_cards()),
                    _ => return,
                }
                if let Some(core) = command.state.borrow().core.clone() {
                    let (hide, compact) = (!ui.get_show_completed(), ui.get_compact_cards());
                    command.handle.spawn(async move {
                        if let Err(error) = core.set_task_display(hide, compact).await {
                            tracing::warn!("saving the Display menu choice failed: {error}");
                        }
                    });
                }
            }
            dispatch(&command, "refresh".into(), Vec::new());
            return;
        }
        if action == "picker" {
            if let Some(app) = command.weak.upgrade() {
                show_date_picker(&app, b.as_str());
            }
            return;
        }
        if action == "open-mail" {
            if let Some(app) = command.weak.upgrade()
                && let Ok(thread_id) = a.parse::<i32>()
            {
                open_linked_mail(&app, &command.state, &runtime_for_mail, thread_id, b.as_str());
            }
            return;
        }
        if action == "open" && a.starts_with("event:") {
            open_linked_event(&command, a.trim_start_matches("event:"));
            return;
        }
        if action == "due-badge" {
            if let Some(app) = command.weak.upgrade() {
                refresh_due_badge(&app, &command.state, &command.handle);
                refresh_task_setting_choices(&app, &command.state, &command.handle);
            }
            return;
        }
        dispatch(&command, action.to_string(), vec![a.to_string(), b.to_string()]);
    });
    app.global::<TasksUi>().on_to_transfer(slint::DataTransfer::from);
    app.global::<TasksUi>()
        .on_text_of(|data| data.plain_text().unwrap_or_default());
    let linked_weak = app.as_weak();
    app.global::<LinkedTask>().on_open(move || {
        let Some(app) = linked_weak.upgrade() else {
            return;
        };
        let task_id = app.global::<LinkedTask>().get_task_id();
        if task_id < 0 {
            return;
        }
        app.set_active_view("tasks".into());
        app.global::<TasksUi>().invoke_command(
            "open-on-board".into(),
            format!("task:{task_id}").into(),
            "".into(),
        );
    });
    let reply_weak = app.as_weak();
    app.global::<TaskReplyUi>().on_open(move || {
        let Some(app) = reply_weak.upgrade() else {
            return;
        };
        let task_id = app.global::<TaskReplyUi>().get_task_id();
        if task_id < 0 {
            // The due-today card has no single task: show the Today list.
            if app.global::<TaskReplyUi>().get_due_today() {
                app.set_active_view("tasks".into());
                app.set_settings_open(false);
                app.global::<TasksUi>()
                    .invoke_command("select".into(), "focus:today".into(), "".into());
            }
            return;
        }
        app.set_active_view("tasks".into());
        app.set_settings_open(false);
        app.global::<TasksUi>().invoke_command(
            "open-on-board".into(),
            format!("task:{task_id}").into(),
            "".into(),
        );
    });
    let undo_ctx = Rc::clone(&ctx);
    app.global::<LinkedTask>().on_undo(move || undo_create(&undo_ctx));
    let open = Rc::clone(&ctx);
    app.on_open_task_from_mail(move |id| open_from_mail(&open, id));
    let bulk = Rc::clone(&ctx);
    app.on_create_tasks_from_selection(move || create_from_selection(&bulk));
    let pick_app = app.as_weak();
    app.global::<TaskFromMail>().on_pick(move |kind, id| {
        let Some(app) = pick_app.upgrade() else {
            return;
        };
        let dialog = app.global::<TaskFromMail>();
        let name_of = |menu: ModelRc<crate::ActionMenuEntry>| {
            (0..menu.row_count())
                .filter_map(|i| menu.row_data(i))
                .find(|e| e.id == id)
                .map(|e| e.title.to_string())
                .unwrap_or_default()
        };
        if kind == "project" {
            dialog.set_project_name(name_of(dialog.get_project_menu()).into());
            dialog.set_project_id(id);
        } else {
            dialog.set_context_name(name_of(dialog.get_context_menu()).into());
            dialog.set_context_id(id);
        }
    });
    let create = Rc::clone(&ctx);
    app.global::<TaskFromMail>()
        .on_create(move || create_from_mail(&create));
    let save = Rc::clone(&ctx);
    app.global::<TasksUi>()
        .on_save_detail(move |id, title, description, due| {
            dispatch(
                &save,
                "save-detail".into(),
                vec![
                    id.to_string(),
                    title.to_string(),
                    description.to_string(),
                    due.to_string(),
                ],
            );
        });
}

/// Show a linked event in Calendar: the view switches to the day it starts
/// on. An event that is no longer in the calendar only gets a status line.
fn open_linked_event(ctx: &Rc<Ctx>, uid: &str) {
    let Some(core) = ctx.state.borrow().core.as_ref().map(|c| c.file_core()) else {
        return;
    };
    let uid = uid.to_owned();
    let weak = ctx.weak.clone();
    ctx.handle.spawn(async move {
        let start = core.task_event_start(uid).await.ok().flatten();
        let _ = weak.upgrade_in_event_loop(move |app| {
            let Some(start) = start else {
                app.set_sync_status(crate::UiMessage::plain(
                    "That event is no longer in the calendar.",
                ));
                return;
            };
            let Some(day) = Local.timestamp_millis_opt(start).single() else {
                return;
            };
            app.set_settings_open(false);
            app.set_active_view("calendar".into());
            app.invoke_calendar_select_date(day.format("%Y-%m-%d").to_string().into());
        });
    });
}

/// Open a linked conversation in Mail. The list is searched for its subject
/// in every mailbox's unified inbox view, so a thread that is not in the
/// inbox may not be listed; the reader then stays empty rather than showing
/// an unrelated message.
fn open_linked_mail(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &tokio::runtime::Runtime,
    thread_id: i32,
    subject: &str,
) {
    app.set_settings_open(false);
    app.set_active_view("mail".into());
    {
        let mut state = state.borrow_mut();
        let listed = state.messages.iter().any(|row| row.id == thread_id)
            && state.query.is_empty();
        if listed {
            drop(state);
            app.invoke_select_email(thread_id);
            return;
        }
        state.set_scope(crate::mail_scope::Scope::UNIFIED_INBOX);
        state.query = subject.to_owned();
        state.search_filter = "All mail".to_owned();
        state.pending_selection = Some(thread_id);
        state.mail_groups.clear();
        state.page = 1;
        state.next_cursor = None;
        state.selected_id = None;
        state.checked_ids.clear();
        state.preview_closed = false;
    }
    if let Err(error) = crate::mail_view_model::refresh_from_source(app, state, runtime, false, &[])
    {
        app.set_sync_status(crate::UiMessage::detail("Mail refresh failed: {}", error));
    }
}

/// Count the open tasks listed under Today and show the number in the window
/// title. Runs for any view, because the title is always visible.
pub(crate) fn refresh_due_badge(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &tokio::runtime::Handle,
) {
    let Some(core) = state.borrow().core.as_ref().map(|c| c.file_core()) else {
        return;
    };
    let weak = app.as_weak();
    let today_end = today_end_ms();
    runtime.spawn(async move {
        let counts = core.task_counts(today_end).await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            if let Ok(counts) = counts {
                app.set_tasks_due_today(i32::try_from(counts.today).unwrap_or(i32::MAX));
            }
        });
    });
}

/// Where an arrow key moves the board cursor. `columns` lists the card ids of
/// each column left to right; a collapsed column counts as empty. Returns the
/// new cursor, or `None` when the board has no card. With no cursor (or one
/// that is gone after a reload) any key lands on the first card.
fn board_step(columns: &[Vec<i64>], cursor: i64, key: &str) -> Option<i64> {
    let first = columns.iter().find_map(|c| c.first().copied());
    let Some((col, row)) = columns.iter().enumerate().find_map(|(ci, cards)| {
        cards.iter().position(|id| *id == cursor).map(|ri| (ci, ri))
    }) else {
        return first;
    };
    let cards = &columns[col];
    match key {
        "up" => Some(cards[row.saturating_sub(1)]),
        "down" => Some(cards[(row + 1).min(cards.len() - 1)]),
        "home" => cards.first().copied(),
        "end" => cards.last().copied(),
        "left" | "right" => {
            let next = if key == "left" {
                columns[..col].iter().rposition(|c| !c.is_empty())
            } else {
                columns[col + 1..]
                    .iter()
                    .position(|c| !c.is_empty())
                    .map(|i| col + 1 + i)
            };
            Some(match next {
                Some(ci) => columns[ci][row.min(columns[ci].len() - 1)],
                None => cursor,
            })
        }
        _ => Some(cursor),
    }
}

/// A key pressed on the focused board: move the cursor, open the card under
/// it, or complete / reopen it.
fn board_key(app: &AppWindow, ctx: &Rc<Ctx>, key: &str) {
    let ui = app.global::<TasksUi>();
    let rows: Vec<_> = ui.get_columns().iter().collect();
    let columns: Vec<Vec<i64>> = rows
        .iter()
        .map(|c| {
            if c.collapsed {
                Vec::new()
            } else {
                c.cards.iter().map(|card| i64::from(card.id)).collect()
            }
        })
        .collect();
    let cursor = i64::from(ui.get_cursor());
    match key {
        "open" | "toggle" => {
            let card = rows
                .iter()
                .flat_map(|c| c.cards.iter())
                .find(|card| i64::from(card.id) == cursor);
            let Some(card) = card else { return };
            let action = match key {
                "open" => "open",
                _ if card.done => "reopen",
                _ => "complete",
            };
            dispatch(ctx, action.to_owned(), vec![card.id.to_string(), String::new()]);
        }
        _ => {
            if let Some(next) = board_step(&columns, cursor, key) {
                ui.set_cursor(i32::try_from(next).unwrap_or(-1));
            }
        }
    }
}

const DEFAULT_STATUSES: [&str; 4] = ["inbox", "next", "waiting", "scheduled"];

/// Show the stored "New task from email" defaults in the Tasks settings.
/// The choice lists arrive later from `refresh_task_setting_choices`.
pub(crate) fn apply_task_defaults(app: &AppWindow, settings: &rustmail_core::models::Settings) {
    let ui = app.global::<TaskSettingsUi>();
    ui.set_project(settings.task_default_project.clone().into());
    ui.set_context(settings.task_default_context.clone().into());
    ui.set_status_index(
        DEFAULT_STATUSES
            .iter()
            .position(|s| *s == settings.task_default_status)
            .unwrap_or(0) as i32,
    );
    ui.set_columns_index(i32::from(settings.task_project_columns == "simple"));
    let tasks = app.global::<TasksUi>();
    tasks.set_show_completed(!settings.task_hide_completed);
    tasks.set_compact_cards(settings.task_compact_cards);
    set_choice_indexes(&ui);
}

/// Point the two select rows at the stored names; a name that no longer
/// exists falls back to entry 0 (first project / no context).
fn set_choice_indexes(ui: &TaskSettingsUi<'_>) {
    let index_of = |choices: ModelRc<slint::SharedString>, name: slint::SharedString| {
        (1..choices.row_count())
            .find(|i| choices.row_data(*i).as_ref() == Some(&name))
            .unwrap_or(0) as i32
    };
    ui.set_project_index(index_of(ui.get_project_choices(), ui.get_project()));
    ui.set_context_index(index_of(ui.get_context_choices(), ui.get_context()));
}

/// Fill the project and context choices of the Tasks settings from the task
/// database. Entry 0 stands for "the first project" and "no context".
pub(crate) fn refresh_task_setting_choices(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &tokio::runtime::Handle,
) {
    let Some(core) = state.borrow().core.as_ref().map(|c| c.file_core()) else {
        return;
    };
    let weak = app.as_weak();
    runtime.spawn(async move {
        let loaded = async {
            core.tasks_open().await?;
            Ok::<_, rustmail_core::error::CoreError>((
                core.task_projects().await?,
                core.task_contexts().await?,
            ))
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let Ok((projects, contexts)) = loaded else { return };
            let list = |first: &str, names: Vec<String>| {
                ModelRc::new(VecModel::from(
                    std::iter::once(first.to_owned())
                        .chain(names)
                        .map(slint::SharedString::from)
                        .collect::<Vec<_>>(),
                ))
            };
            let ui = app.global::<TaskSettingsUi>();
            ui.set_project_choices(list(
                "First project",
                projects.into_iter().map(|p| p.name).collect(),
            ));
            ui.set_context_choices(list(
                "No context",
                contexts.into_iter().map(|c| c.name).collect(),
            ));
            set_choice_indexes(&ui);
        });
    });
}

/// Refresh the task badges on the mail list rows: one entry per conversation
/// that has a task.
pub(crate) fn refresh_task_chips(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &tokio::runtime::Runtime,
) {
    let Some(core) = state.borrow().core.as_ref().map(|c| c.file_core()) else {
        return;
    };
    let weak = app.as_weak();
    runtime.spawn(async move {
        let found = core.task_threads(None).await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let Ok(found) = found else { return };
            app.global::<TaskChips>().set_rows(ModelRc::new(VecModel::from(
                found
                    .into_iter()
                    .filter_map(|t| {
                        Some(TaskChipEntry {
                            id: i32::try_from(t.thread_id).ok()?,
                            status: t.status.into(),
                            waiting_on_reply: t.waiting_on_reply,
                        })
                    })
                    .collect::<Vec<_>>(),
            )));
        });
    });
}

/// Show the task linked to the selected conversation under the mail header.
/// The thread is recorded synchronously so a slow lookup for a conversation
/// that is no longer selected is dropped.
pub(crate) fn show_linked_task(app: &AppWindow, state: &Rc<RefCell<InboxState>>, runtime: &tokio::runtime::Runtime) {
    let (thread_id, core) = {
        let state = state.borrow();
        let thread = state
            .selected_id
            .filter(|_| !state.preview_closed)
            .and_then(|id| state.messages.iter().find(|m| m.id == id))
            .map(|m| m.thread_id.unwrap_or(i64::from(m.id)));
        (thread, state.core.as_ref().map(|c| c.file_core()))
    };
    let linked = app.global::<LinkedTask>();
    let Some((thread_id, core)) = thread_id.zip(core) else {
        linked.set_thread_id(-1);
        linked.set_task_id(-1);
        return;
    };
    let thread_id = thread_id as i32;
    if linked.get_thread_id() != thread_id {
        linked.set_thread_id(thread_id);
        linked.set_task_id(-1);
        // Moving between conversations is also when the list badges are
        // brought up to date (and the first time they are filled).
        refresh_task_chips(app, state, runtime);
    }
    let weak = app.as_weak();
    runtime.spawn(async move {
        let found = core.task_threads(Some(vec![i64::from(thread_id)])).await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let linked = app.global::<LinkedTask>();
            if linked.get_thread_id() != thread_id {
                return;
            }
            if linked.get_undo_thread_id() != thread_id {
                linked.set_undo_task_id(-1);
            }
            match found.ok().and_then(|f| f.into_iter().next()) {
                Some(t) => {
                    linked.set_task_id(t.task_id as i32);
                    linked.set_title(t.title.into());
                    linked.set_status(t.status.into());
                    linked.set_due(t.due_at.map(iso_date).unwrap_or_default().into());
                    linked.set_waiting_on_reply(t.waiting_on_reply);
                }
                None => linked.set_task_id(-1),
            }
        });
    });
}

/// Open the "New task from email" dialog for the conversation of list row
/// `row_id`, or the selected conversation when it is negative.
fn open_from_mail(ctx: &Rc<Ctx>, row_id: i32) {
    let found = {
        let state = ctx.state.borrow();
        let id = if row_id >= 0 { Some(row_id) } else { state.selected_id };
        let core = state.core.as_ref().map(|c| c.file_core());
        id.and_then(|id| state.messages.iter().find(|m| m.id == id))
            .map(|m| {
                (
                    m.thread_id.unwrap_or(i64::from(m.id)),
                    m.subject.clone(),
                    m.sender.clone(),
                )
            })
            .zip(core)
    };
    let Some(((thread_id, subject, sender), core)) = found else {
        return;
    };
    let weak = ctx.weak.clone();
    ctx.handle.spawn(async move {
        let loaded = async {
            core.tasks_open().await?;
            let projects = core.task_projects().await?;
            let contexts = core.task_contexts().await?;
            Ok::<_, rustmail_core::error::CoreError>((projects, contexts))
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let dialog = app.global::<TaskFromMail>();
            let (projects, contexts) = match loaded {
                Ok(found) => found,
                Err(error) => {
                    app.global::<TasksUi>().set_status(error.to_string().into());
                    return;
                }
            };
            let entry = |id: String, title: String| crate::ActionMenuEntry {
                id: id.into(),
                title: title.into(),
                icon: Default::default(),
                disabled: false,
                destructive: false,
            };
            let defaults = app.global::<TaskSettingsUi>();
            let wanted_project = defaults.get_project();
            let first = projects
                .iter()
                .find(|p| !wanted_project.is_empty() && p.name == wanted_project.as_str())
                .or_else(|| projects.first());
            dialog.set_project_id(first.map_or(String::new(), |p| p.id.to_string()).into());
            dialog.set_project_name(first.map_or(String::new(), |p| p.name.clone()).into());
            dialog.set_project_menu(ModelRc::new(VecModel::from(
                projects
                    .iter()
                    .map(|p| entry(p.id.to_string(), p.name.clone()))
                    .collect::<Vec<_>>(),
            )));
            let mut context_entries = vec![entry("none".into(), "No context".into())];
            context_entries.extend(
                contexts
                    .iter()
                    .map(|c| entry(c.id.to_string(), format!("@{}", c.name))),
            );
            dialog.set_context_menu(ModelRc::new(VecModel::from(context_entries)));
            let wanted_context = defaults.get_context();
            let default_context = contexts
                .iter()
                .find(|c| !wanted_context.is_empty() && c.name == wanted_context.as_str());
            dialog.set_context_id(
                default_context.map_or("none".to_owned(), |c| c.id.to_string()).into(),
            );
            dialog.set_context_name(
                default_context.map_or(String::new(), |c| format!("@{}", c.name)).into(),
            );
            dialog.set_thread_id(thread_id as i32);
            dialog.set_mail_label(format!("{sender} · {subject}").into());
            dialog.set_title(subject.into());
            dialog.set_status(
                DEFAULT_STATUSES
                    .get(usize::try_from(defaults.get_status_index()).unwrap_or(0))
                    .copied()
                    .unwrap_or("inbox")
                    .into(),
            );
            dialog.set_due("".into());
            dialog.set_archive(app.global::<TaskSettingsUi>().get_archive());
            dialog.set_error("".into());
            dialog.set_busy(false);
            dialog.set_open(true);
        });
    });
}

/// Undo "create task from email": delete the task just made and, when the
/// dialog also archived the conversation, move it back.
fn undo_create(ctx: &Rc<Ctx>) {
    let Some(app) = ctx.weak.upgrade() else {
        return;
    };
    let Some(core) = ctx.state.borrow().core.as_ref().map(|c| c.file_core()) else {
        return;
    };
    let linked = app.global::<LinkedTask>();
    let (task_id, thread_id, archived) = (
        i64::from(linked.get_undo_task_id()),
        i64::from(linked.get_undo_thread_id()),
        linked.get_undo_archived(),
    );
    if task_id < 0 {
        return;
    }
    linked.set_undo_task_id(-1);
    let weak = ctx.weak.clone();
    ctx.handle.spawn(async move {
        let result = async {
            core.task_delete(task_id).await?;
            if archived {
                core.perform_action(PerformActionArgs {
                    kind: ActionKind::Unarchive,
                    thread_ids: vec![thread_id],
                    params: None,
                })
                .await?;
            }
            Ok::<_, rustmail_core::error::CoreError>(())
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let linked = app.global::<LinkedTask>();
            linked.set_task_id(-1);
            if let Err(error) = result {
                app.global::<TasksUi>().set_status(error.to_string().into());
            }
        });
    });
}

/// Create the task from the dialog's fields, then optionally archive the mail.
/// The column kind a status word of the Tasks settings and the dialog stands for.
fn column_kind(status: &str) -> ColumnKind {
    match status {
        "next" => ColumnKind::Next,
        "waiting" => ColumnKind::Waiting,
        "scheduled" => ColumnKind::Scheduled,
        _ => ColumnKind::Inbox,
    }
}

/// "Create tasks" in the multi-select toolbar: one task for each ticked
/// conversation, with the defaults of the Tasks settings (list, project,
/// context, archive). A conversation that already has a task is skipped.
fn create_from_selection(ctx: &Rc<Ctx>) {
    let Some(app) = ctx.weak.upgrade() else {
        return;
    };
    let (threads, core) = {
        let state = ctx.state.borrow();
        let mut threads: Vec<i64> = state
            .messages
            .iter()
            .filter(|m| state.checked_ids.contains(&m.id))
            .map(|m| m.thread_id.unwrap_or(i64::from(m.id)))
            .collect();
        threads.dedup();
        (threads, state.core.as_ref().map(|c| c.file_core()))
    };
    let Some(core) = core else {
        return;
    };
    if threads.is_empty() {
        app.set_sync_status(crate::UiMessage::plain("Select conversations first."));
        return;
    }
    let defaults = app.global::<TaskSettingsUi>();
    let wanted_project = defaults.get_project().to_string();
    let wanted_context = defaults.get_context().to_string();
    let status = DEFAULT_STATUSES[usize::try_from(defaults.get_status_index().clamp(0, 3)).unwrap_or(0)];
    let archive = defaults.get_archive();
    let weak = ctx.weak.clone();
    ctx.handle.spawn(async move {
        let result = async {
            core.tasks_open().await?;
            let project_id = core
                .task_projects()
                .await?
                .iter()
                .find(|p| !wanted_project.is_empty() && p.name == wanted_project)
                .map(|p| p.id);
            let context_id = core
                .task_contexts()
                .await?
                .iter()
                .find(|c| !wanted_context.is_empty() && c.name == wanted_context)
                .map(|c| c.id);
            let (mut created, mut made) = (0_usize, Vec::new());
            for thread_id in &threads {
                let draft = TaskDraft {
                    title: None,
                    project_id,
                    status: column_kind(status),
                    due_at: None,
                    context_id,
                    label_ids: Vec::new(),
                };
                if core.task_create_from_thread_once(*thread_id, draft).await?.is_some() {
                    created += 1;
                    made.push(*thread_id);
                }
            }
            if archive && !made.is_empty() {
                core.perform_action(PerformActionArgs {
                    kind: ActionKind::Archive,
                    thread_ids: made,
                    params: None,
                })
                .await?;
            }
            Ok::<_, rustmail_core::error::CoreError>((created, threads.len() - created))
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            app.set_sync_status(match result {
                Ok((created, skipped)) => crate::UiMessage::arguments(
                    "Tasks created: {}. Already had a task: {}",
                    created.to_string(),
                    skipped.to_string(),
                ),
                Err(error) => {
                    crate::UiMessage::detail("Could not create the tasks: {}", error)
                }
            });
        });
    });
}

fn create_from_mail(ctx: &Rc<Ctx>) {
    let Some(app) = ctx.weak.upgrade() else {
        return;
    };
    let Some(core) = ctx.state.borrow().core.as_ref().map(|c| c.file_core()) else {
        return;
    };
    let dialog = app.global::<TaskFromMail>();
    if dialog.get_busy() || dialog.get_title().trim().is_empty() {
        return;
    }
    let due = match parse_due(dialog.get_due().as_str()) {
        Ok(due) => due,
        Err(message) => {
            dialog.set_error(message.into());
            return;
        }
    };
    let thread_id = i64::from(dialog.get_thread_id());
    let status = column_kind(dialog.get_status().as_str());
    let draft = TaskDraft {
        title: Some(dialog.get_title().to_string()),
        project_id: dialog.get_project_id().parse().ok(),
        status,
        due_at: due,
        context_id: dialog.get_context_id().parse().ok(),
        label_ids: Vec::new(),
    };
    let archive = dialog.get_archive();
    dialog.set_error("".into());
    dialog.set_busy(true);
    let weak = ctx.weak.clone();
    ctx.handle.spawn(async move {
        let result = async {
            let task = core.task_create_from_thread(thread_id, draft).await?;
            if archive {
                core.perform_action(PerformActionArgs {
                    kind: ActionKind::Archive,
                    thread_ids: vec![thread_id],
                    params: None,
                })
                .await?;
            }
            Ok::<_, rustmail_core::error::CoreError>(task.id)
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let dialog = app.global::<TaskFromMail>();
            dialog.set_busy(false);
            match result {
                Ok(task_id) => {
                    dialog.set_open(false);
                    // Offer Undo on the banner of this conversation.
                    let linked = app.global::<LinkedTask>();
                    linked.set_undo_task_id(task_id as i32);
                    linked.set_undo_thread_id(thread_id as i32);
                    linked.set_undo_archived(archive);
                }
                Err(error) => dialog.set_error(error.to_string().into()),
            }
        });
    });
}

fn dispatch(ctx: &Rc<Ctx>, action: String, args: Vec<String>) {
    let Some(core) = ctx.state.borrow().core.as_ref().map(|c| c.file_core()) else {
        return;
    };
    let arg = |i: usize| args.get(i).map_or("", String::as_str).to_owned();
    {
        let mut s = ctx.session.borrow_mut();
        match action.as_str() {
            "select" => {
                s.selection = arg(0);
                s.query.clear();
                s.label = None;
                s.context = None;
                s.assignee = None;
                s.linked = None;
            }
            "filter-add" => match arg(0).split_once(':') {
                Some(("label", id)) => s.label = id.parse().ok(),
                Some(("context", id)) => s.context = id.parse().ok(),
                Some(("assignee", key)) => s.assignee = Some(key.to_owned()),
                Some(("linked", what)) => s.linked = LinkedTo::parse(what),
                _ => {}
            },
            // "Open on board": the task's own project, with nothing hiding it.
            "open-on-board" => {
                let target = arg(0);
                let task_id = target.strip_prefix("task:").unwrap_or(&target).parse::<i64>().ok();
                s.detail_id = task_id;
                if let Some(task) = task_id.and_then(|id| ctx.handle.block_on(core.task_get(id)).ok()) {
                    s.selection = format!("project:{}", task.project_id);
                }
                s.query.clear();
                s.label = None;
                s.context = None;
                s.assignee = None;
                s.linked = None;
                s.list = false;
            }
            "view" => {
                s.list = arg(0) == "list";
                s.timeline = arg(0) == "timeline";
            }
            "nav-filter" => s.nav = arg(0),
            "group" => match arg(0).strip_prefix("sort:") {
                Some(token) => s.sort = s.sort.chosen(token),
                None => s.group = Group::parse(&arg(0)),
            },
            "filter-remove" => match arg(0).as_str() {
                "label" => s.label = None,
                "context" => s.context = None,
                "assignee" => s.assignee = None,
                "linked" => s.linked = None,
                _ => {}
            },
            "search" => s.query = arg(0),
            "open" => {
                let target = arg(0);
                s.detail_id = target.strip_prefix("task:").unwrap_or(&target).parse().ok();
            }
            "close-detail" => s.detail_id = None,
            "delete" if s.detail_id == arg(0).parse().ok() => s.detail_id = None,
            _ => {}
        }
    }
    let hide_done_now = ctx
        .weak
        .upgrade()
        .is_some_and(|app| !app.global::<TasksUi>().get_show_completed());
    let (selection, query, detail_id, chips, nav) = {
        let s = ctx.session.borrow();
        (
            s.selection.clone(),
            s.query.clone(),
            s.detail_id,
            (
                s.label,
                s.context,
                s.list,
                s.group,
                s.sort,
                Extra {
                    assignee: s.assignee.clone(),
                    linked: s.linked,
                    hide_done: hide_done_now,
                    timeline: s.timeline,
                },
            ),
            s.nav.clone(),
        )
    };
    let ticket = ctx.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let generation = ctx.generation.clone();
    let weak = ctx.weak.clone();
    if let Some(app) = weak.upgrade() {
        app.global::<TasksUi>().set_loading(true);
    }
    ctx.handle.spawn(async move {
        let result = mutate(&core, &action, &args, &selection).await;
        // A newer command supersedes this reload, never its mutation.
        if generation.load(Ordering::SeqCst) != ticket {
            return;
        }
        let selection = match &result {
            Ok(Some(next)) => next.clone(),
            _ => selection,
        };
        let snap = load(&core, &selection, &query, detail_id, chips)
            .await
            .map(|mut snap| {
                filter_nav(&mut snap, &nav);
                snap
            });
        let _ = weak.upgrade_in_event_loop(move |app| {
            if generation.load(Ordering::SeqCst) != ticket {
                return;
            }
            let ui = app.global::<TasksUi>();
            ui.set_loading(false);
            ui.set_status(
                result
                    .err()
                    .or(snap.as_ref().err().map(|e| e.to_string()))
                    .unwrap_or_default()
                    .into(),
            );
            if let Ok(snap) = snap {
                apply(&ui, snap);
            }
        });
    });
}

/// The 6 x 7 grid (weeks start on Monday) for a month, as
/// `(day number, ISO date, in the month, today, selected)`.
fn picker_grid(
    first: chrono::NaiveDate,
    today: chrono::NaiveDate,
    selected: Option<chrono::NaiveDate>,
) -> Vec<(String, String, bool, bool, bool)> {
    use chrono::Datelike;
    let start = first - chrono::Duration::days(first.weekday().num_days_from_monday() as i64);
    (0..42)
        .map(|i| {
            let d = start + chrono::Duration::days(i);
            (
                d.day().to_string(),
                d.format("%Y-%m-%d").to_string(),
                d.month() == first.month(),
                d == today,
                Some(d) == selected,
            )
        })
        .collect()
}

/// Fill the drawer's date picker. `spec` is "" (the month of the task's due
/// date, else this month), "prev:YYYY-MM" or "next:YYYY-MM" (the month
/// shown now, moved by one) or "close".
fn show_date_picker(app: &AppWindow, spec: &str) {
    use chrono::Datelike;
    let ui = app.global::<TasksUi>();
    if spec == "close" {
        ui.set_picker_open(false);
        return;
    }
    let today = Local::now().date_naive();
    let selected = chrono::NaiveDate::parse_from_str(ui.get_detail().due.as_str(), "%Y-%m-%d").ok();
    let month = |ym: &str| chrono::NaiveDate::parse_from_str(&format!("{ym}-01"), "%Y-%m-%d").ok();
    let shown = match spec.split_once(':') {
        Some(("prev", ym)) => month(ym).and_then(|m| m.checked_sub_months(chrono::Months::new(1))),
        Some(("next", ym)) => month(ym).and_then(|m| m.checked_add_months(chrono::Months::new(1))),
        _ => None,
    }
    .or(selected)
    .unwrap_or(today)
    .with_day(1)
    .unwrap_or(today);
    ui.set_picker_month(shown.format("%Y-%m").to_string().into());
    ui.set_picker_title(shown.format("%B %Y").to_string().into());
    ui.set_picker_days(ModelRc::new(VecModel::from(
        picker_grid(shown, today, selected)
            .into_iter()
            .map(|(label, date, in_month, is_today, is_selected)| PickerDayRow {
                label: label.into(),
                date: date.into(),
                in_month,
                today: is_today,
                selected: is_selected,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_picker_open(true);
}

/// Parse an ISO date into the end of that local day (the same instant
/// "Today" uses), so a task due today is not overdue until tomorrow.
fn parse_due(text: &str) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| "Due date must look like 2026-12-31".to_owned())?;
    let end = date.and_hms_opt(23, 59, 59).unwrap();
    Local
        .from_local_datetime(&end)
        .latest()
        .map(|t| Some(t.timestamp_millis()))
        .ok_or_else(|| "Due date does not exist in this time zone".to_owned())
}

/// Parse an ISO date into the start of that local day, the first moment of
/// work on a task (`parse_due` gives the end of a day).
fn parse_start(text: &str) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| "Start date must look like 2026-12-31".to_owned())?;
    let begin = date.and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&begin)
        .earliest()
        .map(|t| Some(t.timestamp_millis()))
        .ok_or_else(|| "Start date does not exist in this time zone".to_owned())
}

/// Run the mutation behind a command. `Some(selection)` asks the view to
/// switch to a newly created list.
async fn mutate(
    core: &Core,
    action: &str,
    args: &[String],
    selection: &str,
) -> Result<Option<String>, String> {
    let a = args.first().map_or("", String::as_str);
    let b = args.get(1).map_or("", String::as_str);
    let id = a.parse::<i64>().ok();
    let err = |e: rustmail_core::error::CoreError| e.to_string();
    let selected_project = selection
        .strip_prefix("project:")
        .and_then(|p| p.parse::<i64>().ok());
    match action {
        "add-task" => {
            let title = b.trim();
            if title.is_empty() {
                return Ok(None);
            }
            let projects = core.task_projects().await.map_err(err)?;
            let column = id.filter(|c| *c >= 0);
            let project_id = match selected_project {
                Some(p) => p,
                None => projects.first().map(|p| p.id).ok_or("no project")?,
            };
            let context_id = selection
                .strip_prefix("context:")
                .and_then(|c| c.parse::<i64>().ok());
            core.task_create(NewTask {
                project_id,
                column_id: column.filter(|_| selected_project.is_some()),
                title: title.to_owned(),
                context_id,
                quick_capture: true,
                ..Default::default()
            })
            .await
            .map_err(err)?;
        }
        "complete" => {
            core.task_complete(id.ok_or("bad task")?).await.map_err(err)?;
        }
        "reopen" => {
            core.task_reopen(id.ok_or("bad task")?).await.map_err(err)?;
        }
        "delete" => core.task_delete(id.ok_or("bad task")?).await.map_err(err)?,
        "card-menu" => {
            if let Some(column) = b.strip_prefix("move:").and_then(|c| c.parse::<i64>().ok()) {
                core.task_move(id.ok_or("bad task")?, column, None)
                    .await
                    .map_err(err)?;
            } else if let Some((column, target)) = b
                .strip_prefix("after:")
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(c, t)| Some((c.parse::<i64>().ok()?, t.parse::<i64>().ok()?)))
            {
                // A card dropped on another card: it lands right below it.
                let id = id.ok_or("bad task")?;
                if id != target {
                    core.task_move(id, column, Some(target)).await.map_err(err)?;
                }
            }
        }
        "detail-context" => {
            let context = match b.strip_prefix("ctx:") {
                Some("none") => None,
                Some(c) => Some(c.parse::<i64>().map_err(|_| "bad context")?),
                None => return Ok(None),
            };
            let patch = TaskPatch {
                context_id: Some(context),
                ..Default::default()
            };
            core.task_update(id.ok_or("bad task")?, patch).await.map_err(err)?;
        }
        "detail-assignee" => {
            let text = b.trim();
            let assignee = if text.is_empty() {
                None
            } else if let Some((name, rest)) = text.split_once('<')
                && let Some(email) = rest.strip_suffix('>')
            {
                Some((email.trim().to_owned(), name.trim().to_owned()))
            } else if text.contains('@') && !text.contains(' ') {
                Some((text.to_owned(), String::new()))
            } else {
                let hit = core
                    .suggest_contacts(text.to_owned(), 1)
                    .await
                    .map_err(err)?
                    .into_iter()
                    .next()
                    .ok_or("No contact matches; type an email address")?;
                Some((hit.email, hit.name.unwrap_or_default()))
            };
            core.task_update(
                id.ok_or("bad task")?,
                TaskPatch {
                    assignee: Some(assignee),
                    ..Default::default()
                },
            )
            .await
            .map_err(err)?;
        }
        "link-task" => {
            // `b` is "<depends|blocks>:<title words>"; the first other open
            // task whose title matches becomes the link target.
            let task_id = id.ok_or("bad task")?;
            if let Some(("mail", query)) = b.split_once(':') {
                if query.trim().is_empty() {
                    return Ok(None);
                }
                core.task_link_mail_by_subject(task_id, query.to_owned())
                    .await
                    .map_err(|_| "No mail with that subject")?;
                return Ok(None);
            }
            if let Some(("event", query)) = b.split_once(':') {
                // A calendar event: the one whose title contains the words,
                // nearest to now.
                if query.trim().is_empty() {
                    return Ok(None);
                }
                let uid = core
                    .task_find_event(query.to_owned())
                    .await
                    .map_err(err)?
                    .ok_or("No calendar event matches")?;
                core.task_link_add(
                    task_id,
                    rustmail_core::tasks::NewLink {
                        kind: rustmail_core::tasks::LinkKind::Event,
                        account_email: None,
                        thread_id: None,
                        message_id: None,
                        event_uid: Some(uid),
                        other_task_uid: None,
                    },
                )
                .await
                .map_err(err)?;
                return Ok(None);
            }
            let (kind, query) = match b.split_once(':') {
                Some(("blocks", q)) => (rustmail_core::tasks::LinkKind::TaskBlocks, q),
                Some((_, q)) => (rustmail_core::tasks::LinkKind::TaskDepends, q),
                None => return Ok(None),
            };
            if query.trim().is_empty() {
                return Ok(None);
            }
            let found = core
                .task_list(TaskFilter {
                    text: Some(query.trim().to_owned()),
                    ..Default::default()
                })
                .await
                .map_err(err)?
                .into_iter()
                .find(|t| t.id != task_id)
                .ok_or("No other open task matches")?;
            core.task_link_add(
                task_id,
                rustmail_core::tasks::NewLink {
                    kind,
                    account_email: None,
                    thread_id: None,
                    message_id: None,
                    event_uid: None,
                    other_task_uid: Some(found.uid),
                },
            )
            .await
            .map_err(err)?;
        }
        "link-remove" => {
            core.task_link_remove(id.ok_or("bad link")?).await.map_err(err)?;
        }
        "detail-label" => {
            let label = b
                .strip_prefix("lbl:")
                .and_then(|l| l.parse::<i64>().ok())
                .ok_or("bad label")?;
            let task_id = id.ok_or("bad task")?;
            let mut ids = core.task_get(task_id).await.map_err(err)?.label_ids;
            match ids.iter().position(|l| *l == label) {
                Some(at) => {
                    ids.remove(at);
                }
                None => ids.push(label),
            }
            core.task_set_labels(task_id, ids).await.map_err(err)?;
        }
        "detail-label-new" if !b.trim().is_empty() => {
            let task_id = id.ok_or("bad task")?;
            let label = core
                .task_label_create(b.trim().to_owned(), String::new())
                .await
                .map_err(err)?;
            let mut ids = core.task_get(task_id).await.map_err(err)?.label_ids;
            ids.push(label.id);
            core.task_set_labels(task_id, ids).await.map_err(err)?;
        }
        "collapse-empty" => {
            if let Some(project) = selected_project {
                core.task_collapse_empty(project).await.map_err(err)?;
            }
        }
        "collapse" => {
            core.task_column_collapse(id.ok_or("bad column")?, b == "1")
                .await
                .map_err(err)?;
        }
        "column-rename" if !b.trim().is_empty() => {
            let patch = ColumnPatch {
                name: Some(b.trim().to_owned()),
                ..Default::default()
            };
            core.task_column_update(id.ok_or("bad column")?, patch)
                .await
                .map_err(err)?;
        }
        "column-wip" => {
            let limit = match b.trim() {
                "" => 0,
                text => text
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (0..=999).contains(n))
                    .ok_or("The limit must be a number from 0 to 999")?,
            };
            let patch = ColumnPatch {
                wip_limit: Some((limit > 0).then_some(limit)),
                ..Default::default()
            };
            core.task_column_update(id.ok_or("bad column")?, patch)
                .await
                .map_err(err)?;
        }
        "column-colour" => {
            let patch = ColumnPatch {
                colour: Some(b.trim_start_matches("colour:").to_owned()),
                ..Default::default()
            };
            core.task_column_update(id.ok_or("bad column")?, patch)
                .await
                .map_err(err)?;
        }
        "column-icon" => {
            let patch = ColumnPatch {
                icon: Some(b.trim_start_matches("icon:").to_owned()),
                ..Default::default()
            };
            core.task_column_update(id.ok_or("bad column")?, patch)
                .await
                .map_err(err)?;
        }
        "column-move" => {
            let id = id.ok_or("bad column")?;
            let project = selected_project.ok_or("select a project first")?;
            let ids: Vec<i64> = core
                .task_columns(project)
                .await
                .map_err(err)?
                .iter()
                .map(|c| c.id)
                .collect();
            let at = ids.iter().position(|c| *c == id).ok_or("bad column")?;
            // `after` is the column the moved one should follow; None = first.
            let after = match b {
                "left" if at > 0 => Some(at.checked_sub(2).map(|i| ids[i])),
                "right" if at + 1 < ids.len() => Some(Some(ids[at + 1])),
                _ => None,
            };
            if let Some(after) = after {
                core.task_column_reorder(id, after).await.map_err(err)?;
            }
        }
        "column-delete" => {
            let id = id.ok_or("bad column")?;
            let project = selected_project.ok_or("select a project first")?;
            let columns = core.task_columns(project).await.map_err(err)?;
            let column = columns.iter().find(|c| c.id == id).ok_or("bad column")?;
            if column.kind != ColumnKind::Custom {
                return Err("Built-in columns cannot be deleted".into());
            }
            // Its tasks go to the first other column (the project's inbox).
            let target = columns
                .iter()
                .find(|c| c.id != id)
                .ok_or("a project needs another column")?;
            core.task_column_delete(id, target.id).await.map_err(err)?;
        }
        "add-column" if !a.trim().is_empty() => {
            let project = selected_project.ok_or("select a project first")?;
            core.task_column_create(project, a.to_owned())
                .await
                .map_err(err)?;
        }
        "toggle-move-back" => {
            let id = id.ok_or("bad task")?;
            let task = core.task_get(id).await.map_err(err)?;
            core.task_update(
                id,
                TaskPatch {
                    move_back_on_reply: Some(!task.move_back_on_reply),
                    ..Default::default()
                },
            )
            .await
            .map_err(err)?;
        }
        "toggle-priority" => {
            let id = id.ok_or("bad task")?;
            let task = core.task_get(id).await.map_err(err)?;
            core.task_update(
                id,
                TaskPatch {
                    priority: Some(if task.priority >= 3 { 0 } else { 3 }),
                    ..Default::default()
                },
            )
            .await
            .map_err(err)?;
        }
        "detail-due" => {
            let days = match b {
                "today" => Some(0),
                "tomorrow" => Some(1),
                "week" => Some(7),
                _ => None,
            };
            // A quick choice, a date from the picker, or "none" to clear.
            let due = match days {
                Some(d) => {
                    let day = Local::now().date_naive() + chrono::Duration::days(d);
                    parse_due(&day.format("%Y-%m-%d").to_string())?
                }
                None if b == "none" => None,
                None => parse_due(b)?,
            };
            core.task_update(
                id.ok_or("bad task")?,
                TaskPatch {
                    due_at: Some(due),
                    ..Default::default()
                },
            )
            .await
            .map_err(err)?;
        }
        "detail-start" => {
            let task_id = id.ok_or("bad task")?;
            let start = parse_start(b)?;
            if let Some(start) = start {
                let task = core.task_get(task_id).await.map_err(err)?;
                if task.due_at.is_some_and(|due| start > due) {
                    return Err("The start date must not be after the due date".into());
                }
            }
            core.task_update(
                task_id,
                TaskPatch {
                    start_at: Some(start),
                    ..Default::default()
                },
            )
            .await
            .map_err(err)?;
        }
        "save-detail" => {
            let due = parse_due(args.get(3).map_or("", String::as_str))?;
            core.task_update(
                id.ok_or("bad task")?,
                TaskPatch {
                    title: Some(b.to_owned()),
                    description: Some(args.get(2).cloned().unwrap_or_default()),
                    due_at: Some(due),
                    ..Default::default()
                },
            )
            .await
            .map_err(err)?;
        }
        "check-add" if !b.trim().is_empty() => {
            core.task_checklist_add(id.ok_or("bad task")?, b.to_owned())
                .await
                .map_err(err)?;
        }
        "check-toggle" => {
            core.task_checklist_update(id.ok_or("bad item")?, None, Some(b == "1"))
                .await
                .map_err(err)?;
        }
        "check-delete" => {
            core.task_checklist_delete(id.ok_or("bad item")?)
                .await
                .map_err(err)?;
        }
        "create-project" if !a.trim().is_empty() => {
            let project = core
                .task_project_create_default(a.to_owned())
                .await
                .map_err(err)?;
            return Ok(Some(format!("project:{}", project.id)));
        }
        "create-context" if !a.trim().is_empty() => {
            let context = core.task_context_create(a.to_owned()).await.map_err(err)?;
            return Ok(Some(format!("context:{}", context.id)));
        }
        _ => {}
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn board_keys_move_the_cursor_between_cards_and_skip_empty_columns() {
        let board = vec![vec![1, 2, 3], vec![], vec![4, 5]];
        let step = |cursor, key| super::board_step(&board, cursor, key);
        // No cursor, or a card that is gone: the first card.
        assert_eq!(step(-1, "down"), Some(1));
        assert_eq!(step(99, "right"), Some(1));
        // Up and down stop at the ends of the column.
        assert_eq!(step(2, "up"), Some(1));
        assert_eq!(step(1, "up"), Some(1));
        assert_eq!(step(2, "down"), Some(3));
        assert_eq!(step(3, "down"), Some(3));
        // Left and right skip the empty column and keep the row, clamped.
        assert_eq!(step(2, "right"), Some(5));
        assert_eq!(step(3, "right"), Some(5));
        assert_eq!(step(5, "left"), Some(2));
        assert_eq!(step(1, "left"), Some(1));
        assert_eq!(step(5, "right"), Some(5));
        // Home and End.
        assert_eq!(step(2, "end"), Some(3));
        assert_eq!(step(3, "home"), Some(1));
        // An empty board has no cursor.
        assert_eq!(super::board_step(&[vec![], vec![]], 1, "down"), None);
    }

    use super::*;

    #[tokio::test]
    async fn list_mode_flattens_a_project_and_names_each_status() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert!(snap.can_switch_view && !snap.list_mode);
        let inbox = snap.columns[0].clone_id_name();
        run(&core, &sel, "add-task", &inbox.0.to_string(), "One").await.unwrap();

        let snap = load(&core, &sel, "", None, (None, None, true, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert!(snap.list_mode);
        assert_eq!(snap.columns.len(), 1);
        assert_eq!(snap.columns[0].id, -1);
        assert_eq!(snap.columns[0].cards[0].status, inbox.1);

        // Focus lists have no board to switch to.
        let snap = load(&core, "focus:inbox", "", None, (None, None, true, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert!(!snap.can_switch_view && !snap.list_mode);
    }

    #[tokio::test]
    async fn the_flat_list_sorts_by_due_date_priority_and_title() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        for title in ["banana", "Cherry", "apple"] {
            run(&core, &sel, "add-task", &inbox, title).await.unwrap();
        }
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let id_of = |t: &str| snap.columns[0].cards.iter().find(|c| c.title == t).unwrap().id;
        run(&core, &sel, "detail-due", &id_of("Cherry").to_string(), "2026-12-01").await.unwrap();
        run(&core, &sel, "detail-due", &id_of("banana").to_string(), "2026-11-01").await.unwrap();
        run(&core, &sel, "toggle-priority", &id_of("Cherry").to_string(), "").await.unwrap();

        let order = |sort: Sort| {
            let core = &core;
            let sel = &sel;
            async move {
                load(core, sel, "", None, (None, None, true, Group::None, sort, Extra::default()))
                    .await
                    .unwrap()
                    .columns[0]
                    .cards
                    .iter()
                    .map(|c| c.title.clone())
                    .collect::<Vec<_>>()
            }
        };
        // Soonest first, tasks without a date last.
        assert_eq!(order(Sort::Due).await, ["banana", "Cherry", "apple"]);
        // High priority first; ties keep the manual order.
        assert_eq!(order(Sort::Priority).await, ["Cherry", "banana", "apple"]);
        // Case-insensitive title order.
        assert_eq!(order(Sort::Title).await, ["apple", "banana", "Cherry"]);
        assert_eq!(order(Sort::Manual).await, ["banana", "Cherry", "apple"]);
        // Choosing the active order again reverses it, and again restores it.
        assert!(Sort::Title.chosen("title") == Sort::TitleReversed);
        assert!(Sort::TitleReversed.chosen("title") == Sort::Title);
        assert!(Sort::Title.chosen("due") == Sort::Due);
        assert!(Sort::Manual.chosen("manual") == Sort::Manual);
        let reversed = |sort: Sort| {
            let core = &core;
            let sel = &sel;
            async move {
                load(core, sel, "", None, (None, None, true, Group::None, sort, Extra::default()))
                    .await
                    .unwrap()
                    .columns[0]
                    .cards
                    .iter()
                    .map(|c| c.title.clone())
                    .collect::<Vec<_>>()
            }
        };
        // Latest first with undated last; low priority first; Z to A.
        assert_eq!(reversed(Sort::DueReversed).await, ["Cherry", "banana", "apple"]);
        assert_eq!(reversed(Sort::PriorityReversed).await, ["banana", "apple", "Cherry"]);
        assert_eq!(reversed(Sort::TitleReversed).await, ["Cherry", "banana", "apple"]);

        // The menu offers the sorts and ticks the active one.
        let by_title = load(&core, &sel, "", None, (None, None, true, Group::None, Sort::Title, Extra::default()))
            .await
            .unwrap();
        assert!(
            by_title
                .group_menu
                .iter()
                .any(|(id, text)| id == "sort:title" && text.starts_with("✓"))
        );
    }

    #[tokio::test]
    async fn a_card_shows_its_first_linked_event() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        run(&core, &sel, "add-task", &inbox, "Plan the offsite").await.unwrap();
        run(&core, &sel, "add-task", &inbox, "No links here").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let id_of = |t: &str| snap.columns[0].cards.iter().find(|c| c.title == t).unwrap().id;
        core.task_link_add(
            id_of("Plan the offsite"),
            rustmail_core::tasks::NewLink {
                kind: rustmail_core::tasks::LinkKind::Event,
                account_email: None,
                thread_id: None,
                message_id: None,
                event_uid: Some("offsite-2026".into()),
                other_task_uid: None,
            },
        )
        .await
        .unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let cards = &snap.columns[0].cards;
        let linked = cards.iter().find(|c| c.title == "Plan the offsite").unwrap();
        assert_eq!(linked.link, Some(("event".to_owned(), "offsite-2026".to_owned())));

        // An event that exists in the calendar shows its title, on the card
        // and in the drawer.
        let event = core
            .create_event(rustmail_core::models::CreateEventArgs {
                account_id: 0,
                calendar_id: None,
                summary: "Offsite planning".into(),
                description: None,
                location: None,
                join_url: None,
                starts_at: 1_800_000_000_000,
                ends_at: 1_800_003_600_000,
                all_day: false,
                attendees: Vec::new(),
            })
            .await
            .unwrap();
        let event_id = event.id;
        let uid: String = core
            .calendar_db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT ical_uid FROM calendar_events WHERE id = ?1",
                    [event_id],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        core.task_link_add(
            id_of("No links here"),
            rustmail_core::tasks::NewLink {
                kind: rustmail_core::tasks::LinkKind::Event,
                account_email: None,
                thread_id: None,
                message_id: None,
                event_uid: Some(uid),
                other_task_uid: None,
            },
        )
        .await
        .unwrap();
        let snap = load(&core, &sel, "", Some(id_of("No links here")), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let card = snap.columns[0].cards.iter().find(|c| c.title == "No links here").unwrap();
        assert_eq!(card.link, Some(("event".to_owned(), "Offsite planning".to_owned())));
        assert_eq!(snap.detail.as_ref().unwrap().links[0].2, "Offsite planning");
        assert_eq!(card.link_more, 0);
        assert!(cards.iter().find(|c| c.title == "No links here").unwrap().link.is_none());

        // A second link shows as a count next to the first.
        core.task_link_add(
            id_of("No links here"),
            rustmail_core::tasks::NewLink {
                kind: rustmail_core::tasks::LinkKind::Event,
                account_email: None,
                thread_id: None,
                message_id: None,
                event_uid: Some("second-event".into()),
                other_task_uid: None,
            },
        )
        .await
        .unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let card = snap.columns[0].cards.iter().find(|c| c.title == "No links here").unwrap();
        assert_eq!(card.link_more, 1);
    }

    #[tokio::test]
    async fn grouping_splits_the_list_into_sections() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        for title in ["Soon", "Whenever", "Tagged"] {
            run(&core, &sel, "add-task", &inbox, title).await.unwrap();
        }
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let id_of = |t: &str| snap.columns[0].cards.iter().find(|c| c.title == t).unwrap().id;
        run(&core, &sel, "detail-due", &id_of("Soon").to_string(), "today").await.unwrap();
        run(&core, &sel, "detail-label-new", &id_of("Tagged").to_string(), "urgent")
            .await
            .unwrap();

        let names = |snap: &Snapshot| {
            snap.columns
                .iter()
                .map(|c| (c.name.clone(), c.cards.len()))
                .collect::<Vec<_>>()
        };
        let by_due = load(&core, &sel, "", None, (None, None, true, Group::Due, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(
            names(&by_due),
            [("Today".to_owned(), 1), ("No due date".to_owned(), 2)]
        );
        let by_label = load(&core, &sel, "", None, (None, None, true, Group::Label, Sort::Manual, Extra::default()))
            .await
            .unwrap();
        assert_eq!(
            names(&by_label),
            [("No label".to_owned(), 2), ("urgent".to_owned(), 1)]
        );
        let by_status = load(&core, &sel, "", None, (None, None, true, Group::Status, Sort::Manual, Extra::default()))
            .await
            .unwrap();
        assert_eq!(by_status.columns.len(), 1);
        assert!(by_status.group_menu.iter().any(|(id, t)| id == "status" && t.starts_with("✓")));
        // A group that does not fit the view falls back to no grouping.
        let focus = load(&core, "focus:inbox", "", None, (None, None, false, Group::Status, Sort::Manual, Extra::default()))
            .await
            .unwrap();
        assert!(focus.group_menu.iter().all(|(id, _)| id != "status"));
        // The board has no Group menu.
        let board = load(&core, &sel, "", None, (None, None, false, Group::Due, Sort::Manual, Extra::default())).await.unwrap();
        assert!(board.group_menu.is_empty());
    }

    #[tokio::test]
    async fn task_links_are_added_by_title_and_removed() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        run(&core, &sel, "add-task", &inbox, "Draft").await.unwrap();
        run(&core, &sel, "add-task", &inbox, "Publish").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let id_of = |t: &str| snap.columns[0].cards.iter().find(|c| c.title == t).unwrap().id;
        let (draft, publish) = (id_of("Draft"), id_of("Publish"));

        run(&core, &sel, "link-task", &publish.to_string(), "depends:draft").await.unwrap();
        let snap = load(&core, &sel, "", Some(publish), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let links = &snap.detail.as_ref().unwrap().links;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "task_depends");
        assert_eq!(links[0].4, format!("task:{draft}"));
        // The linked task is still open.
        assert_eq!(links[0].5, "open");

        // Only the task itself matches, so there is nothing to link.
        let own = run(&core, &sel, "link-task", &publish.to_string(), "blocks:publish").await;
        assert!(own.is_err());

        // A calendar event is found by its title and shown by it.
        assert!(run(&core, &sel, "link-task", &publish.to_string(), "event:offsite").await.is_err());
        let at = chrono::Utc::now().timestamp_millis() + 86_400_000;
        core.create_event(rustmail_core::models::CreateEventArgs {
            account_id: 0,
            calendar_id: None,
            summary: "Team Offsite".into(),
            description: None,
            location: None,
            join_url: None,
            starts_at: at,
            ends_at: at + 3_600_000,
            all_day: false,
            attendees: Vec::new(),
        })
        .await
        .unwrap();
        run(&core, &sel, "link-task", &publish.to_string(), "event:offsite").await.unwrap();
        let snap = load(&core, &sel, "", Some(publish), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let links = &snap.detail.as_ref().unwrap().links;
        assert_eq!(links.len(), 2);
        assert_eq!(links[1].1, "event");
        assert_eq!(links[1].2, "Team Offsite");
        assert!(links[1].4.starts_with("event:"));
        // An event shows the day it starts.
        assert_eq!(links[1].5, iso_date(at));
        run(&core, &sel, "link-remove", &links[1].0.to_string(), "").await.unwrap();

        run(&core, &sel, "link-remove", &links[0].0.to_string(), "").await.unwrap();
        let snap = load(&core, &sel, "", Some(publish), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert!(snap.detail.unwrap().links.is_empty());
    }

    #[test]
    fn a_done_title_gets_a_line_through_each_letter_but_not_the_spaces() {
        assert_eq!(strike_through("Hi you"), "H\u{336}i\u{336} y\u{336}o\u{336}u\u{336}");
        assert_eq!(strike_through(""), "");
    }

    #[test]
    fn colours_parse_and_fall_back() {
        assert_eq!(colour("#2563eb"), slint::Color::from_rgb_u8(0x25, 0x63, 0xeb));
        assert_eq!(colour("bogus"), colour(DEFAULT_COLOUR));
    }

    #[tokio::test]
    async fn a_start_date_is_set_cleared_and_kept_before_the_due_date() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].clone_id_name().0.to_string();
        run(&core, &sel, "add-task", &inbox, "Plan").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let id = snap.columns[0].cards[0].id;
        let tid = id.to_string();
        let start = |snap: &Snapshot| snap.detail.as_ref().unwrap().start.clone();
        let view = || load(&core, &sel, "", Some(id), (None, None, false, Group::None, Sort::Manual, Extra::default()));

        run(&core, &sel, "detail-start", &tid, "2026-12-01").await.unwrap();
        assert_eq!(start(&view().await.unwrap()), "2026-12-01");
        // A start after the due date is refused and changes nothing.
        run(&core, &sel, "detail-due", &tid, "2026-12-10").await.unwrap();
        assert!(run(&core, &sel, "detail-start", &tid, "2026-12-11").await.is_err());
        assert_eq!(start(&view().await.unwrap()), "2026-12-01");
        // The due day itself is fine; an empty value clears; text is refused.
        run(&core, &sel, "detail-start", &tid, "2026-12-10").await.unwrap();
        assert!(run(&core, &sel, "detail-start", &tid, "soon").await.is_err());
        run(&core, &sel, "detail-start", &tid, "").await.unwrap();
        assert_eq!(start(&view().await.unwrap()), "");
    }

    #[test]
    fn due_dates_parse_to_end_of_local_day() {
        assert_eq!(parse_due("  "), Ok(None));
        assert!(parse_due("2026-13-40").is_err());
        assert!(parse_due("tomorrow").is_err());
        let due = parse_due("2026-12-31").unwrap().unwrap();
        assert_eq!(iso_date(due), "2026-12-31");
        let next = parse_due("2027-01-01").unwrap().unwrap();
        assert_eq!(next - due, 24 * 60 * 60 * 1000);
    }

    #[test]
    fn avatar_initials() {
        assert_eq!(initials(Some("Ana María Díaz"), None), "AD");
        assert_eq!(initials(Some("ana"), Some("a@x.org")), "A");
        assert_eq!(initials(None, Some("bob@x.org")), "B");
        assert_eq!(initials(None, None), "");
    }

    #[test]
    fn sidebar_search_keeps_matches_and_the_selection() {
        let row = |key: &str, label: &str| NavSnap {
            key: key.into(),
            label: label.into(),
            count: 0,
            colour: String::new(),
        };
        let mut snap = Snapshot {
            selection: "project:2".into(),
            projects: vec![row("project:1", "Garden"), row("project:2", "Taxes")],
            contexts: vec![row("context:1", "phone"), row("context:2", "Gardening")],
            ..Default::default()
        };
        filter_nav(&mut snap, " GARDEN ");
        let keys = |rows: &[NavSnap]| rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&snap.projects), ["project:1", "project:2"]);
        assert_eq!(keys(&snap.contexts), ["context:2"]);
        filter_nav(&mut snap, "");
        assert_eq!(snap.contexts.len(), 1);
    }

    #[test]
    fn focus_keys_map() {
        assert_eq!(focus_for("waiting"), Some(TaskFocus::WaitingFor));
        assert_eq!(focus_for("nope"), None);
    }

    async fn board() -> (tempfile::TempDir, Core, String) {
        let tmp = tempfile::tempdir().unwrap();
        let core = Core::start(rustmail_core::config::Paths::for_tests(tmp.path()))
            .await
            .unwrap();
        core.tasks_open().await.unwrap();
        let project = core.task_projects().await.unwrap()[0].id;
        (tmp, core, format!("project:{project}"))
    }

    async fn run(core: &Core, sel: &str, action: &str, a: &str, b: &str) -> Result<(), String> {
        mutate(core, action, &[a.to_owned(), b.to_owned()], sel)
            .await
            .map(|_| ())
    }

    impl ColumnSnap {
        fn clone_id_name(&self) -> (i64, String) {
            (self.id, self.name.clone())
        }
    }

    fn first_column<'a>(snap: &'a Snapshot, name: &str) -> &'a ColumnSnap {
        snap.columns.iter().find(|c| c.name == name).unwrap()
    }

    #[test]
    fn the_date_picker_grid_starts_on_monday_and_marks_today_and_the_selection() {
        let d = |y, m, day| chrono::NaiveDate::from_ymd_opt(y, m, day).unwrap();
        // 1 Nov 2026 is a Sunday: six cells of October come first.
        let grid = picker_grid(d(2026, 11, 1), d(2026, 11, 3), Some(d(2026, 11, 20)));
        assert_eq!(grid.len(), 42);
        assert_eq!(grid[0].1, "2026-10-26");
        assert!(!grid[0].2 && grid[6].1 == "2026-11-01" && grid[6].2);
        assert_eq!(grid.iter().filter(|c| c.3).map(|c| c.1.as_str()).collect::<Vec<_>>(), ["2026-11-03"]);
        assert_eq!(grid.iter().filter(|c| c.4).map(|c| c.1.as_str()).collect::<Vec<_>>(), ["2026-11-20"]);
    }

    #[tokio::test]
    async fn tasks_are_created_checked_off_completed_reopened_and_deleted() {
        let (_tmp, core, sel) = board().await;
        let view = |detail: Option<i64>| {
            let (core, sel) = (&core, &sel);
            async move {
                load(core, sel, "", detail, (None, None, false, Group::None, Sort::Manual, Extra::default()))
                    .await
                    .unwrap()
            }
        };
        let card = |snap: &Snapshot, title: &str| {
            snap.columns.iter().find_map(|c| {
                c.cards
                    .iter()
                    .find(|k| k.title == title)
                    .map(|k| (c.name.clone(), k.id, k.done, k.checklist))
            })
        };
        let snap = view(None).await;
        let inbox = snap.columns[0].clone_id_name();

        // A blank title creates nothing; a real one lands in the chosen column.
        run(&core, &sel, "add-task", &inbox.0.to_string(), "   ").await.unwrap();
        assert!(snap.columns.iter().all(|c| c.cards.is_empty()));
        run(&core, &sel, "add-task", &inbox.0.to_string(), "Write report").await.unwrap();
        let (column, id, done, _) = card(&view(None).await, "Write report").unwrap();
        assert_eq!((column.as_str(), done), (inbox.1.as_str(), false));

        // Checklist: blank text is ignored, items count and toggle.
        run(&core, &sel, "check-add", &id.to_string(), "  ").await.unwrap();
        run(&core, &sel, "check-add", &id.to_string(), "Outline").await.unwrap();
        run(&core, &sel, "check-add", &id.to_string(), "Draft").await.unwrap();
        let snap = view(Some(id)).await;
        assert_eq!(card(&snap, "Write report").unwrap().3, (0, 2));
        let items = snap.detail.as_ref().unwrap().checklist.clone();
        assert_eq!(items.len(), 2);
        run(&core, &sel, "check-toggle", &items[0].0.to_string(), "1").await.unwrap();
        assert_eq!(card(&view(None).await, "Write report").unwrap().3, (1, 2));
        run(&core, &sel, "check-toggle", &items[0].0.to_string(), "0").await.unwrap();
        run(&core, &sel, "check-delete", &items[1].0.to_string(), "").await.unwrap();
        assert_eq!(card(&view(None).await, "Write report").unwrap().3, (0, 1));

        // Completing moves the card to Done; reopening takes it out again.
        run(&core, &sel, "complete", &id.to_string(), "").await.unwrap();
        let (column, _, done, _) = card(&view(None).await, "Write report").unwrap();
        assert_eq!((column.as_str(), done), ("Done", true));
        run(&core, &sel, "reopen", &id.to_string(), "").await.unwrap();
        let (column, _, done, _) = card(&view(None).await, "Write report").unwrap();
        assert!(!done && column != "Done");

        run(&core, &sel, "delete", &id.to_string(), "").await.unwrap();
        assert!(card(&view(None).await, "Write report").is_none());
        // Commands aimed at a task that is gone fail instead of panicking.
        assert!(run(&core, &sel, "complete", &id.to_string(), "").await.is_err());
        assert!(run(&core, &sel, "complete", "nonsense", "").await.is_err());
    }

    #[tokio::test]
    async fn column_commands_validate_and_apply() {
        let (_tmp, core, sel) = board().await;
        run(&core, &sel, "add-column", "Review", "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let review = first_column(&snap, "Review");
        assert!(!review.fixed);
        let id = review.id.to_string();

        run(&core, &sel, "column-rename", &id, "QA").await.unwrap();
        run(&core, &sel, "column-wip", &id, "3").await.unwrap();
        run(&core, &sel, "column-colour", &id, "colour:#ef4444").await.unwrap();
        run(&core, &sel, "column-icon", &id, "icon:star").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let qa = first_column(&snap, "QA");
        assert_eq!((qa.wip, qa.colour.as_str()), (3, "#ef4444"));
        assert_eq!(qa.icon, "star");

        assert!(run(&core, &sel, "column-wip", &id, "1000").await.is_err());
        assert!(run(&core, &sel, "column-wip", &id, "many").await.is_err());
        run(&core, &sel, "column-wip", &id, "").await.unwrap();

        let before: Vec<_> = snap.columns.iter().map(|c| c.id).collect();
        run(&core, &sel, "column-move", &id, "left").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let after: Vec<_> = snap.columns.iter().map(|c| c.id).collect();
        let at = |v: &[i64]| v.iter().position(|c| c.to_string() == id).unwrap();
        assert_eq!(at(&after) + 1, at(&before));

        let built_in = snap.columns.iter().find(|c| c.fixed).unwrap().id.to_string();
        let refused = run(&core, &sel, "column-delete", &built_in, "").await;
        assert_eq!(refused, Err("Built-in columns cannot be deleted".into()));
        run(&core, &sel, "column-delete", &id, "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert!(snap.columns.iter().all(|c| c.name != "QA"));
    }

    #[tokio::test]
    async fn card_move_due_and_assignee_commands() {
        let (_tmp, core, sel) = board().await;
        run(&core, &sel, "add-column", "Review", "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        let review = first_column(&snap, "Review").id.to_string();
        run(&core, &sel, "add-task", &inbox, "Write report").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let task = snap.columns[0].cards[0].id;
        let tid = task.to_string();

        // What a drop sends: the card menu's move command.
        run(&core, &sel, "card-menu", &tid, &format!("move:{review}")).await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(first_column(&snap, "Review").cards[0].id, task);

        // A card dropped on another card lands right below it, in any column.
        run(&core, &sel, "add-task", &review, "Second").await.unwrap();
        run(&core, &sel, "add-task", &review, "Third").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let titles = |snap: &Snapshot| -> Vec<String> {
            first_column(snap, "Review").cards.iter().map(|c| c.title.to_string()).collect()
        };
        let order = titles(&snap);
        assert_eq!(order.len(), 3);
        let first = first_column(&snap, "Review").cards[0].id.to_string();
        let last = first_column(&snap, "Review").cards[2].id;
        run(&core, &sel, "card-menu", &first, &format!("after:{review}:{last}")).await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let moved = titles(&snap);
        assert_eq!(moved, vec![order[1].clone(), order[2].clone(), order[0].clone()]);
        // Dropping a card on itself changes nothing.
        run(&core, &sel, "card-menu", &first, &format!("after:{review}:{first}")).await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(titles(&snap), moved);
        // The strip above the first card sends a plain move: the last card
        // goes to the top of its own column.
        let bottom = first_column(&snap, "Review").cards[2].id.to_string();
        run(&core, &sel, "card-menu", &bottom, &format!("move:{review}")).await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(
            titles(&snap),
            vec![moved[2].clone(), moved[0].clone(), moved[1].clone()]
        );

        run(&core, &sel, "detail-due", &tid, "tomorrow").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().due, iso_date(today_end_ms() + 1000));
        run(&core, &sel, "detail-due", &tid, "none").await.unwrap();

        run(&core, &sel, "detail-assignee", &tid, "Ana Diaz <ana@example.org>")
            .await
            .unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "Ana Diaz <ana@example.org>");
        run(&core, &sel, "detail-assignee", &tid, "bob@example.org").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "bob@example.org");
        assert!(run(&core, &sel, "detail-assignee", &tid, "nobody known").await.is_err());
        run(&core, &sel, "detail-assignee", &tid, "").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "");
    }

    #[tokio::test]
    async fn the_linked_to_chip_keeps_tasks_with_that_kind_of_link() {
        let (_tmp, core, sel) = board().await;
        let none = || Extra::default();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, none())).await.unwrap();
        let inbox = snap.columns[0].clone_id_name().0.to_string();
        for title in ["Plain", "With event", "With task"] {
            run(&core, &sel, "add-task", &inbox, title).await.unwrap();
        }
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, none())).await.unwrap();
        let id_of = |title: &str| {
            snap.columns.iter().flat_map(|c| &c.cards).find(|k| k.title == title).unwrap().id
        };
        core.task_link_add(
            id_of("With event"),
            rustmail_core::tasks::NewLink {
                kind: rustmail_core::tasks::LinkKind::Event,
                account_email: None,
                thread_id: None,
                message_id: None,
                event_uid: Some("uid-1".into()),
                other_task_uid: None,
            },
        )
        .await
        .unwrap();
        run(&core, &sel, "link-task", &id_of("With task").to_string(), "depends:plain").await.unwrap();

        // Every kind is offered while no chip is set.
        let all = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, none())).await.unwrap();
        let offered: Vec<_> = all.filter_menu.iter().filter(|(id, _)| id.starts_with("linked:")).map(|(id, _)| id.as_str()).collect();
        assert_eq!(offered, ["linked:mail", "linked:event", "linked:task"]);

        let titles = |snap: &Snapshot| {
            let mut t: Vec<_> = snap.columns.iter().flat_map(|c| &c.cards).map(|k| k.title.clone()).collect();
            t.sort();
            t
        };
        let by = |what| Extra { linked: Some(what), ..Default::default() };
        let events = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, by(LinkedTo::Event))).await.unwrap();
        assert_eq!(titles(&events), ["With event"]);
        assert_eq!(events.chips, [("linked".to_owned(), "Linked to: Event".to_owned())]);
        assert!(events.filter_menu.iter().all(|(id, _)| !id.starts_with("linked:")));
        let tasks = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, by(LinkedTo::Task))).await.unwrap();
        assert_eq!(titles(&tasks), ["With task"]);
        let mail = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, by(LinkedTo::Mail))).await.unwrap();
        assert!(titles(&mail).is_empty());
        assert_eq!(tasks.task_count, 1);
    }

    #[tokio::test]
    async fn the_assignee_chip_narrows_the_list_to_one_person() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].clone_id_name().0.to_string();
        for title in ["Ask Ana", "Ask Ben", "Nobody's"] {
            run(&core, &sel, "add-task", &inbox, title).await.unwrap();
        }
        let id_of = |snap: &Snapshot, title: &str| {
            snap.columns.iter().flat_map(|c| &c.cards).find(|k| k.title == title).unwrap().id
        };
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        run(&core, &sel, "detail-assignee", &id_of(&snap, "Ask Ana").to_string(), "Ana <ana@x.dev>").await.unwrap();
        run(&core, &sel, "detail-assignee", &id_of(&snap, "Ask Ben").to_string(), "ben@x.dev").await.unwrap();

        // Everyone assigned is offered, once each, sorted by name.
        let all = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let offered: Vec<_> = all.filter_menu.iter().filter(|(id, _)| id.starts_with("assignee:")).collect();
        assert_eq!(offered.len(), 2);
        assert_eq!(offered[0].0, "assignee:ana@x.dev");
        assert!(offered[0].1.ends_with("Ana"));

        let chosen = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra { assignee: Some("ben@x.dev".into()), ..Default::default() })).await.unwrap();
        assert_eq!(chosen.chips.len(), 1);
        assert!(chosen.chips[0].1.starts_with("Assignee:"));
        let titles: Vec<_> = chosen.columns.iter().flat_map(|c| &c.cards).map(|k| k.title.clone()).collect();
        assert_eq!(titles, ["Ask Ben"]);
        assert_eq!(chosen.task_count, 1);
        // With the chip set the menu no longer offers assignees.
        assert!(chosen.filter_menu.iter().all(|(id, _)| !id.starts_with("assignee:")));
        // A person who is not on the list leaves it unfiltered.
        let stale = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra { assignee: Some("gone@x.dev".into()), ..Default::default() })).await.unwrap();
        assert!(stale.chips.is_empty());
        assert_eq!(stale.task_count, 3);
    }

    #[tokio::test]
    async fn label_and_context_chips_filter_the_board() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        run(&core, &sel, "add-task", &inbox, "Tagged").await.unwrap();
        run(&core, &sel, "add-task", &inbox, "Plain").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let tagged = snap.columns[0].cards.iter().find(|c| c.title == "Tagged").unwrap().id;
        run(&core, &sel, "detail-label-new", &tagged.to_string(), "urgent")
            .await
            .unwrap();
        let label = core.task_labels().await.unwrap()[0].id;

        let snap = load(&core, &sel, "", None, (Some(label), None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        let titles: Vec<_> = snap.columns.iter().flat_map(|c| &c.cards).map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Tagged"]);
        assert_eq!(snap.chips, [("label".to_owned(), "Label: urgent".to_owned())]);
        // The label is active, so it is no longer offered in the menu.
        assert!(snap.filter_menu.iter().all(|(id, _)| !id.starts_with("label:")));
        // An unknown label id is ignored instead of emptying the board.
        let snap = load(&core, &sel, "", None, (Some(9999), None, false, Group::None, Sort::Manual, Extra::default())).await.unwrap();
        assert!(snap.chips.is_empty());
    }
}
