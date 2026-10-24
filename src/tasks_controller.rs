//! Tasks UI controller. Callbacks only capture input and schedule work on the
//! runtime; the database is never touched on the UI thread. Every command
//! reloads one `Snapshot` (plain `Send` data) that is applied to the models.
use crate::{
    ActivityRow, AppWindow, ChecklistRow, InboxState, LinkRow, TaskCardRow, TaskColumnRow,
    LinkedTask, TaskChipRow, TaskDetailRow, TaskFromMail, TaskNavRow, TasksUi,
};
use chrono::{Local, TimeZone};
use rustmail_core::Core;
use rustmail_core::models::{ActionKind, PerformActionArgs};
use rustmail_core::tasks::{
    ColumnKind, ColumnPatch, NewTask, Task, TaskColumn, TaskDraft, TaskFilter, TaskFocus, TaskLabel,
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

const DAY_MS: i64 = 24 * 60 * 60 * 1000;


#[derive(Default)]
struct Session {
    selection: String,
    query: String,
    detail_id: Option<i64>,
    /// Filter chips: label and context narrow the current list.
    label: Option<i64>,
    context: Option<i64>,
    /// Project shown as one flat list instead of columns.
    list: bool,
    /// Sidebar search over project and context names.
    nav: String,
    group: Group,
}

struct DetailSnap {
    id: i64,
    title: String,
    description: String,
    due: String,
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
    links: Vec<(i64, String, String, bool, String)>,
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
}

struct ColumnSnap {
    id: i64,
    name: String,
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
    can_switch_view: bool,
    /// `(menu id, title)` for the Group menu; empty when the view is a board.
    group_menu: Vec<(String, String)>,
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
    let links = links
        .into_iter()
        .map(|l| {
            use rustmail_core::tasks::LinkTarget;
            let (text, found, open) = match &l.target {
                LinkTarget::Mail { subject, .. } => (subject.clone(), true, String::new()),
                LinkTarget::Task { task_id, title } => {
                    (title.clone(), true, format!("task:{task_id}"))
                }
                LinkTarget::Event => {
                    (l.link.event_uid.clone().unwrap_or_default(), true, String::new())
                }
                LinkTarget::MailNotFound | LinkTarget::TaskNotFound => {
                    (String::new(), false, String::new())
                }
            };
            (l.link.id, l.link.kind.as_str().to_owned(), text, found, open)
        })
        .collect();
    let detail = DetailSnap {
        id,
        title: task.title.clone(),
        description: task.description.clone(),
        due: task.due_at.map(iso_date).unwrap_or_default(),
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
    chips: (Option<i64>, Option<i64>, bool, Group),
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
    let (label_chip, context_chip, list_mode, group) = chips;
    filter.label_id = label_chip.filter(|l| label_map.contains_key(l));
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

    let board = kind == "project" && filter.text.is_none() && !list_mode;
    snap.list_mode = kind == "project" && list_mode;
    snap.can_switch_view = kind == "project";
    filter.include_done = board;
    let tasks = core.task_list(filter).await?;
    snap.task_count = tasks.iter().filter(|t| t.completed_at.is_none()).count() as i64;
    if board {
        let columns = core.task_columns(project_id.unwrap_or_default()).await?;
        snap.move_targets = columns.iter().map(|c| (c.id, c.name.clone())).collect();
        snap.columns = columns
            .iter()
            .map(|c| ColumnSnap {
                id: c.id,
                name: c.name.clone(),
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
        snap.group_menu = options
            .iter()
            .map(|g| {
                let tick = if *g == group { "✓ " } else { "" };
                (g.token().to_owned(), format!("{tick}{}", titles(*g)))
            })
            .collect();
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
    ui.set_list_mode(snap.list_mode);
    ui.set_can_switch_view(snap.can_switch_view);
    ui.set_group_menu(ModelRc::new(VecModel::from(
        snap.group_menu
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
                    .map(|(id, kind, text, found, open)| LinkRow {
                        id: id as i32,
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
    app.global::<TasksUi>().on_command(move |action, a, b| {
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
        app.global::<TasksUi>()
            .invoke_command("open".into(), format!("task:{task_id}").into(), "".into());
    });
    let open = Rc::clone(&ctx);
    app.on_open_task_from_mail(move |id| open_from_mail(&open, id));
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
    }
    let weak = app.as_weak();
    runtime.spawn(async move {
        let found = core.task_threads(Some(vec![i64::from(thread_id)])).await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let linked = app.global::<LinkedTask>();
            if linked.get_thread_id() != thread_id {
                return;
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
            let first = projects.first();
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
            dialog.set_context_id("none".into());
            dialog.set_context_name("".into());
            dialog.set_thread_id(thread_id as i32);
            dialog.set_mail_label(format!("{sender} · {subject}").into());
            dialog.set_title(subject.into());
            dialog.set_status("inbox".into());
            dialog.set_due("".into());
            dialog.set_archive(false);
            dialog.set_error("".into());
            dialog.set_busy(false);
            dialog.set_open(true);
        });
    });
}

/// Create the task from the dialog's fields, then optionally archive the mail.
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
    let status = match dialog.get_status().as_str() {
        "next" => ColumnKind::Next,
        "waiting" => ColumnKind::Waiting,
        "scheduled" => ColumnKind::Scheduled,
        _ => ColumnKind::Inbox,
    };
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
            core.task_create_from_thread(thread_id, draft).await?;
            if archive {
                core.perform_action(PerformActionArgs {
                    kind: ActionKind::Archive,
                    thread_ids: vec![thread_id],
                    params: None,
                })
                .await?;
            }
            Ok::<_, rustmail_core::error::CoreError>(())
        }
        .await;
        let _ = weak.upgrade_in_event_loop(move |app| {
            let dialog = app.global::<TaskFromMail>();
            dialog.set_busy(false);
            match result {
                Ok(()) => dialog.set_open(false),
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
            }
            "filter-add" => match arg(0).split_once(':') {
                Some(("label", id)) => s.label = id.parse().ok(),
                Some(("context", id)) => s.context = id.parse().ok(),
                _ => {}
            },
            "view" => s.list = arg(0) == "list",
            "nav-filter" => s.nav = arg(0),
            "group" => s.group = Group::parse(&arg(0)),
            "filter-remove" => match arg(0).as_str() {
                "label" => s.label = None,
                "context" => s.context = None,
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
    let (selection, query, detail_id, chips, nav) = {
        let s = ctx.session.borrow();
        (
            s.selection.clone(),
            s.query.clone(),
            s.detail_id,
            (s.label, s.context, s.list, s.group),
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
            let due = days.map(|d| {
                let day = Local::now().date_naive() + chrono::Duration::days(d);
                parse_due(&day.format("%Y-%m-%d").to_string())
            });
            core.task_update(
                id.ok_or("bad task")?,
                TaskPatch {
                    due_at: Some(due.transpose()?.flatten()),
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
                .task_project_create(a.to_owned(), String::new())
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
    use super::*;

    #[tokio::test]
    async fn list_mode_flattens_a_project_and_names_each_status() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        assert!(snap.can_switch_view && !snap.list_mode);
        let inbox = snap.columns[0].clone_id_name();
        run(&core, &sel, "add-task", &inbox.0.to_string(), "One").await.unwrap();

        let snap = load(&core, &sel, "", None, (None, None, true, Group::None)).await.unwrap();
        assert!(snap.list_mode);
        assert_eq!(snap.columns.len(), 1);
        assert_eq!(snap.columns[0].id, -1);
        assert_eq!(snap.columns[0].cards[0].status, inbox.1);

        // Focus lists have no board to switch to.
        let snap = load(&core, "focus:inbox", "", None, (None, None, true, Group::None)).await.unwrap();
        assert!(!snap.can_switch_view && !snap.list_mode);
    }

    #[tokio::test]
    async fn grouping_splits_the_list_into_sections() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        for title in ["Soon", "Whenever", "Tagged"] {
            run(&core, &sel, "add-task", &inbox, title).await.unwrap();
        }
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
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
        let by_due = load(&core, &sel, "", None, (None, None, true, Group::Due)).await.unwrap();
        assert_eq!(
            names(&by_due),
            [("Today".to_owned(), 1), ("No due date".to_owned(), 2)]
        );
        let by_label = load(&core, &sel, "", None, (None, None, true, Group::Label))
            .await
            .unwrap();
        assert_eq!(
            names(&by_label),
            [("No label".to_owned(), 2), ("urgent".to_owned(), 1)]
        );
        let by_status = load(&core, &sel, "", None, (None, None, true, Group::Status))
            .await
            .unwrap();
        assert_eq!(by_status.columns.len(), 1);
        assert!(by_status.group_menu.iter().any(|(id, t)| id == "status" && t.starts_with("✓")));
        // A group that does not fit the view falls back to no grouping.
        let focus = load(&core, "focus:inbox", "", None, (None, None, false, Group::Status))
            .await
            .unwrap();
        assert!(focus.group_menu.iter().all(|(id, _)| id != "status"));
        // The board has no Group menu.
        let board = load(&core, &sel, "", None, (None, None, false, Group::Due)).await.unwrap();
        assert!(board.group_menu.is_empty());
    }

    #[tokio::test]
    async fn task_links_are_added_by_title_and_removed() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        run(&core, &sel, "add-task", &inbox, "Draft").await.unwrap();
        run(&core, &sel, "add-task", &inbox, "Publish").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let id_of = |t: &str| snap.columns[0].cards.iter().find(|c| c.title == t).unwrap().id;
        let (draft, publish) = (id_of("Draft"), id_of("Publish"));

        run(&core, &sel, "link-task", &publish.to_string(), "depends:draft").await.unwrap();
        let snap = load(&core, &sel, "", Some(publish), (None, None, false, Group::None)).await.unwrap();
        let links = &snap.detail.as_ref().unwrap().links;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "task_depends");
        assert_eq!(links[0].4, format!("task:{draft}"));

        // Only the task itself matches, so there is nothing to link.
        let own = run(&core, &sel, "link-task", &publish.to_string(), "blocks:publish").await;
        assert!(own.is_err());

        run(&core, &sel, "link-remove", &links[0].0.to_string(), "").await.unwrap();
        let snap = load(&core, &sel, "", Some(publish), (None, None, false, Group::None)).await.unwrap();
        assert!(snap.detail.unwrap().links.is_empty());
    }

    #[test]
    fn colours_parse_and_fall_back() {
        assert_eq!(colour("#2563eb"), slint::Color::from_rgb_u8(0x25, 0x63, 0xeb));
        assert_eq!(colour("bogus"), colour(DEFAULT_COLOUR));
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

    #[tokio::test]
    async fn column_commands_validate_and_apply() {
        let (_tmp, core, sel) = board().await;
        run(&core, &sel, "add-column", "Review", "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let review = first_column(&snap, "Review");
        assert!(!review.fixed);
        let id = review.id.to_string();

        run(&core, &sel, "column-rename", &id, "QA").await.unwrap();
        run(&core, &sel, "column-wip", &id, "3").await.unwrap();
        run(&core, &sel, "column-colour", &id, "colour:#ef4444").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let qa = first_column(&snap, "QA");
        assert_eq!((qa.wip, qa.colour.as_str()), (3, "#ef4444"));

        assert!(run(&core, &sel, "column-wip", &id, "1000").await.is_err());
        assert!(run(&core, &sel, "column-wip", &id, "many").await.is_err());
        run(&core, &sel, "column-wip", &id, "").await.unwrap();

        let before: Vec<_> = snap.columns.iter().map(|c| c.id).collect();
        run(&core, &sel, "column-move", &id, "left").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let after: Vec<_> = snap.columns.iter().map(|c| c.id).collect();
        let at = |v: &[i64]| v.iter().position(|c| c.to_string() == id).unwrap();
        assert_eq!(at(&after) + 1, at(&before));

        let built_in = snap.columns.iter().find(|c| c.fixed).unwrap().id.to_string();
        let refused = run(&core, &sel, "column-delete", &built_in, "").await;
        assert_eq!(refused, Err("Built-in columns cannot be deleted".into()));
        run(&core, &sel, "column-delete", &id, "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        assert!(snap.columns.iter().all(|c| c.name != "QA"));
    }

    #[tokio::test]
    async fn card_move_due_and_assignee_commands() {
        let (_tmp, core, sel) = board().await;
        run(&core, &sel, "add-column", "Review", "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        let review = first_column(&snap, "Review").id.to_string();
        run(&core, &sel, "add-task", &inbox, "Write report").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let task = snap.columns[0].cards[0].id;
        let tid = task.to_string();

        // What a drop sends: the card menu's move command.
        run(&core, &sel, "card-menu", &tid, &format!("move:{review}")).await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        assert_eq!(first_column(&snap, "Review").cards[0].id, task);

        run(&core, &sel, "detail-due", &tid, "tomorrow").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().due, iso_date(today_end_ms() + 1000));
        run(&core, &sel, "detail-due", &tid, "none").await.unwrap();

        run(&core, &sel, "detail-assignee", &tid, "Ana Diaz <ana@example.org>")
            .await
            .unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "Ana Diaz <ana@example.org>");
        run(&core, &sel, "detail-assignee", &tid, "bob@example.org").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "bob@example.org");
        assert!(run(&core, &sel, "detail-assignee", &tid, "nobody known").await.is_err());
        run(&core, &sel, "detail-assignee", &tid, "").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None, false, Group::None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "");
    }

    #[tokio::test]
    async fn label_and_context_chips_filter_the_board() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        run(&core, &sel, "add-task", &inbox, "Tagged").await.unwrap();
        run(&core, &sel, "add-task", &inbox, "Plain").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None, false, Group::None)).await.unwrap();
        let tagged = snap.columns[0].cards.iter().find(|c| c.title == "Tagged").unwrap().id;
        run(&core, &sel, "detail-label-new", &tagged.to_string(), "urgent")
            .await
            .unwrap();
        let label = core.task_labels().await.unwrap()[0].id;

        let snap = load(&core, &sel, "", None, (Some(label), None, false, Group::None)).await.unwrap();
        let titles: Vec<_> = snap.columns.iter().flat_map(|c| &c.cards).map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Tagged"]);
        assert_eq!(snap.chips, [("label".to_owned(), "Label: urgent".to_owned())]);
        // The label is active, so it is no longer offered in the menu.
        assert!(snap.filter_menu.iter().all(|(id, _)| !id.starts_with("label:")));
        // An unknown label id is ignored instead of emptying the board.
        let snap = load(&core, &sel, "", None, (Some(9999), None, false, Group::None)).await.unwrap();
        assert!(snap.chips.is_empty());
    }
}
