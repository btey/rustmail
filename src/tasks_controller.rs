//! Tasks UI controller. Callbacks only capture input and schedule work on the
//! runtime; the database is never touched on the UI thread. Every command
//! reloads one `Snapshot` (plain `Send` data) that is applied to the models.
use crate::{
    ActivityRow, AppWindow, ChecklistRow, InboxState, LinkRow, TaskCardRow, TaskColumnRow,
    TaskChipRow, TaskDetailRow, TaskNavRow, TasksUi,
};
use chrono::{Local, TimeZone};
use rustmail_core::Core;
use rustmail_core::tasks::{
    ColumnKind, ColumnPatch, NewTask, Task, TaskColumn, TaskFilter, TaskFocus, TaskLabel, TaskPatch,
};
use slint::{ComponentHandle, ModelRc, VecModel};
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

#[derive(Default)]
struct Session {
    selection: String,
    query: String,
    detail_id: Option<i64>,
    /// Filter chips: label and context narrow the current list.
    label: Option<i64>,
    context: Option<i64>,
}

struct DetailSnap {
    id: i64,
    title: String,
    description: String,
    due: String,
    high: bool,
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
    /// `(kind token, text, target found)`.
    links: Vec<(String, String, bool)>,
    activity: Vec<(String, String)>,
}

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
        assignee: task.assignee_name.clone().unwrap_or_default(),
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
            let (text, found) = match &l.target {
                LinkTarget::Mail { subject, .. } => (subject.clone(), true),
                LinkTarget::Task { title, .. } => (title.clone(), true),
                LinkTarget::Event => (l.link.event_uid.clone().unwrap_or_default(), true),
                LinkTarget::MailNotFound | LinkTarget::TaskNotFound => (String::new(), false),
            };
            (l.link.kind.as_str().to_owned(), text, found)
        })
        .collect();
    let detail = DetailSnap {
        id,
        title: task.title.clone(),
        description: task.description.clone(),
        due: task.due_at.map(iso_date).unwrap_or_default(),
        high: task.priority >= 3,
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
    chips: (Option<i64>, Option<i64>),
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
    let (label_chip, context_chip) = chips;
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

    let board = kind == "project" && filter.text.is_none();
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
        // Focus, context and search views are one flat list.
        snap.columns = vec![ColumnSnap {
            id: -1,
            name: String::new(),
            colour: DEFAULT_COLOUR.into(),
            count: tasks.len().to_string(),
            over_wip: false,
            collapsed: false,
            wip: 0,
            fixed: true,
            cards: tasks.iter().map(|t| card(t, &label_map, &context_map, today_end)).collect(),
        }];
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
                    .map(|(kind, text, found)| LinkRow {
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
            "filter-remove" => match arg(0).as_str() {
                "label" => s.label = None,
                "context" => s.context = None,
                _ => {}
            },
            "search" => s.query = arg(0),
            "open" => s.detail_id = arg(0).parse().ok(),
            "close-detail" => s.detail_id = None,
            "delete" if s.detail_id == arg(0).parse().ok() => s.detail_id = None,
            _ => {}
        }
    }
    let (selection, query, detail_id, chips) = {
        let s = ctx.session.borrow();
        (s.selection.clone(), s.query.clone(), s.detail_id, (s.label, s.context))
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
        let snap = load(&core, &selection, &query, detail_id, chips).await;
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

    fn first_column<'a>(snap: &'a Snapshot, name: &str) -> &'a ColumnSnap {
        snap.columns.iter().find(|c| c.name == name).unwrap()
    }

    #[tokio::test]
    async fn column_commands_validate_and_apply() {
        let (_tmp, core, sel) = board().await;
        run(&core, &sel, "add-column", "Review", "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        let review = first_column(&snap, "Review");
        assert!(!review.fixed);
        let id = review.id.to_string();

        run(&core, &sel, "column-rename", &id, "QA").await.unwrap();
        run(&core, &sel, "column-wip", &id, "3").await.unwrap();
        run(&core, &sel, "column-colour", &id, "colour:#ef4444").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        let qa = first_column(&snap, "QA");
        assert_eq!((qa.wip, qa.colour.as_str()), (3, "#ef4444"));

        assert!(run(&core, &sel, "column-wip", &id, "1000").await.is_err());
        assert!(run(&core, &sel, "column-wip", &id, "many").await.is_err());
        run(&core, &sel, "column-wip", &id, "").await.unwrap();

        let before: Vec<_> = snap.columns.iter().map(|c| c.id).collect();
        run(&core, &sel, "column-move", &id, "left").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        let after: Vec<_> = snap.columns.iter().map(|c| c.id).collect();
        let at = |v: &[i64]| v.iter().position(|c| c.to_string() == id).unwrap();
        assert_eq!(at(&after) + 1, at(&before));

        let built_in = snap.columns.iter().find(|c| c.fixed).unwrap().id.to_string();
        let refused = run(&core, &sel, "column-delete", &built_in, "").await;
        assert_eq!(refused, Err("Built-in columns cannot be deleted".into()));
        run(&core, &sel, "column-delete", &id, "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        assert!(snap.columns.iter().all(|c| c.name != "QA"));
    }

    #[tokio::test]
    async fn card_move_due_and_assignee_commands() {
        let (_tmp, core, sel) = board().await;
        run(&core, &sel, "add-column", "Review", "").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        let review = first_column(&snap, "Review").id.to_string();
        run(&core, &sel, "add-task", &inbox, "Write report").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        let task = snap.columns[0].cards[0].id;
        let tid = task.to_string();

        // What a drop sends: the card menu's move command.
        run(&core, &sel, "card-menu", &tid, &format!("move:{review}")).await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        assert_eq!(first_column(&snap, "Review").cards[0].id, task);

        run(&core, &sel, "detail-due", &tid, "tomorrow").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().due, iso_date(today_end_ms() + 1000));
        run(&core, &sel, "detail-due", &tid, "none").await.unwrap();

        run(&core, &sel, "detail-assignee", &tid, "Ana Diaz <ana@example.org>")
            .await
            .unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "Ana Diaz <ana@example.org>");
        run(&core, &sel, "detail-assignee", &tid, "bob@example.org").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "bob@example.org");
        assert!(run(&core, &sel, "detail-assignee", &tid, "nobody known").await.is_err());
        run(&core, &sel, "detail-assignee", &tid, "").await.unwrap();
        let snap = load(&core, &sel, "", Some(task), (None, None)).await.unwrap();
        assert_eq!(snap.detail.as_ref().unwrap().assignee, "");
    }

    #[tokio::test]
    async fn label_and_context_chips_filter_the_board() {
        let (_tmp, core, sel) = board().await;
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        let inbox = snap.columns[0].id.to_string();
        run(&core, &sel, "add-task", &inbox, "Tagged").await.unwrap();
        run(&core, &sel, "add-task", &inbox, "Plain").await.unwrap();
        let snap = load(&core, &sel, "", None, (None, None)).await.unwrap();
        let tagged = snap.columns[0].cards.iter().find(|c| c.title == "Tagged").unwrap().id;
        run(&core, &sel, "detail-label-new", &tagged.to_string(), "urgent")
            .await
            .unwrap();
        let label = core.task_labels().await.unwrap()[0].id;

        let snap = load(&core, &sel, "", None, (Some(label), None)).await.unwrap();
        let titles: Vec<_> = snap.columns.iter().flat_map(|c| &c.cards).map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Tagged"]);
        assert_eq!(snap.chips, [("label".to_owned(), "Label: urgent".to_owned())]);
        // The label is active, so it is no longer offered in the menu.
        assert!(snap.filter_menu.iter().all(|(id, _)| !id.starts_with("label:")));
        // An unknown label id is ignored instead of emptying the board.
        let snap = load(&core, &sel, "", None, (Some(9999), None)).await.unwrap();
        assert!(snap.chips.is_empty());
    }
}
