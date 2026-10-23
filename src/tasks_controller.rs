//! Tasks UI controller. Callbacks only capture input and schedule work on the
//! runtime; the database is never touched on the UI thread. Every command
//! reloads one `Snapshot` (plain `Send` data) that is applied to the models.
use crate::{AppWindow, InboxState, TaskCardRow, TaskColumnRow, TaskNavRow, TasksUi};
use chrono::{Local, TimeZone};
use rustmail_core::Core;
use rustmail_core::tasks::{
    NewTask, Task, TaskColumn, TaskFilter, TaskFocus, TaskLabel,
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
    /// `(entry id, column name)` for the card move menu.
    move_targets: Vec<(i64, String)>,
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

async fn load(core: &Core, selection: &str, query: &str) -> rustmail_core::error::Result<Snapshot> {
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
            cards: tasks.iter().map(|t| card(t, &label_map, &context_map, today_end)).collect(),
        }];
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
}

pub(crate) fn register(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
) {
    let session = Rc::new(RefCell::new(Session {
        selection: "focus:inbox".into(),
        ..Default::default()
    }));
    let generation = Arc::new(AtomicU64::new(0));
    let weak = app.as_weak();
    let state = Rc::clone(state);
    let handle = runtime.handle().clone();
    app.global::<TasksUi>().on_command(move |action, a, b| {
        let Some(core) = state.borrow().core.as_ref().map(|c| c.file_core()) else {
            return;
        };
        let (action, a, b) = (action.to_string(), a.to_string(), b.to_string());
        match action.as_str() {
            "select" => {
                let mut s = session.borrow_mut();
                s.selection = a.clone();
                s.query.clear();
            }
            "search" => session.borrow_mut().query = a.clone(),
            _ => {}
        }
        let (selection, query) = {
            let s = session.borrow();
            (s.selection.clone(), s.query.clone())
        };
        let ticket = generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation = generation.clone();
        let weak = weak.clone();
        if let Some(app) = weak.upgrade() {
            app.global::<TasksUi>().set_loading(true);
        }
        handle.spawn(async move {
            let result = mutate(&core, &action, &a, &b, &selection).await;
            // A newer command supersedes this reload, never its mutation.
            if generation.load(Ordering::SeqCst) != ticket {
                return;
            }
            let selection = match &result {
                Ok(Some(next)) => next.clone(),
                _ => selection,
            };
            let snap = load(&core, &selection, &query).await;
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
    });
}

/// Run the mutation behind a command. `Some(selection)` asks the view to
/// switch to a newly created list.
async fn mutate(
    core: &Core,
    action: &str,
    a: &str,
    b: &str,
    selection: &str,
) -> Result<Option<String>, String> {
    let id = a.parse::<i64>().ok();
    let err = |e: rustmail_core::error::CoreError| e.to_string();
    match action {
        "add-task" => {
            let title = b.trim();
            if title.is_empty() {
                return Ok(None);
            }
            let projects = core.task_projects().await.map_err(err)?;
            let selected_project = selection
                .strip_prefix("project:")
                .and_then(|p| p.parse::<i64>().ok());
            let column = id.filter(|c| *c >= 0);
            let project_id = match (selected_project, column) {
                (Some(p), _) => p,
                (None, _) => projects.first().map(|p| p.id).ok_or("no project")?,
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
        "collapse" => {
            core.task_column_collapse(id.ok_or("bad column")?, b == "1")
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
    fn focus_keys_map() {
        assert_eq!(focus_for("waiting"), Some(TaskFocus::WaitingFor));
        assert_eq!(focus_for("nope"), None);
    }
}
