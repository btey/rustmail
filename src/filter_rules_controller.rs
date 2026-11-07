//! The Filters settings page: list, add, reorder, switch and run filter rules.
use super::*;
use crate::user_actions_controller::{core, spawn};
use rustmail_core::models::{
    FilterAction, FilterCondition, FilterField, FilterOp, FilterRule, Settings,
};
use slint::{Model, ModelRc, SharedString, VecModel};

/// Condition fields in the order of the editor's list.
const FIELDS: [FilterField; 12] = [
    FilterField::From,
    FilterField::To,
    FilterField::Cc,
    FilterField::Subject,
    FilterField::Header,
    FilterField::Body,
    FilterField::Age,
    FilterField::Size,
    FilterField::Read,
    FilterField::Starred,
    FilterField::HasAttachment,
    FilterField::ListMail,
];
const TEXT_OPS: [FilterOp; 6] = [
    FilterOp::Contains,
    FilterOp::NotContains,
    FilterOp::Equals,
    FilterOp::StartsWith,
    FilterOp::EndsWith,
    FilterOp::Regex,
];
const NUMBER_OPS: [FilterOp; 2] = [FilterOp::Greater, FilterOp::Less];
const FLAG_OPS: [FilterOp; 2] = [FilterOp::Yes, FilterOp::No];
/// Action kinds in the order of the editor's list: the engine's own list, so
/// an action the engine learns is never missing here.
const ACTION_KINDS: [&str; 12] = rustmail_core::filters::ACTION_KINDS;
/// The first action kind that takes a value.
const FIRST_VALUE_KIND: i32 = 6;
const MAX_CONDITIONS: usize = 8;
const MAX_ACTIONS: usize = 6;

fn ops_for(field: FilterField) -> &'static [FilterOp] {
    match field {
        FilterField::Age | FilterField::Size => &NUMBER_OPS,
        FilterField::Read
        | FilterField::Starred
        | FilterField::HasAttachment
        | FilterField::ListMail => &FLAG_OPS,
        _ => &TEXT_OPS,
    }
}

fn field_label(field: FilterField) -> &'static str {
    match field {
        FilterField::From => "From",
        FilterField::To => "To",
        FilterField::Cc => "Cc",
        FilterField::Subject => "Subject",
        FilterField::Size => "Size",
        FilterField::HasAttachment => "Attachment",
        FilterField::ListMail => "Mailing list",
        FilterField::Header => "Header",
        FilterField::Body => "Body",
        FilterField::Age => "Age",
        FilterField::Read => "Read",
        FilterField::Starred => "Starred",
    }
}

fn op_label(op: FilterOp) -> &'static str {
    match op {
        FilterOp::Contains => "contains",
        FilterOp::NotContains => "does not contain",
        FilterOp::Equals => "is",
        FilterOp::StartsWith => "starts with",
        FilterOp::EndsWith => "ends with",
        FilterOp::Regex => "matches",
        FilterOp::Greater => "over",
        FilterOp::Less => "under",
        FilterOp::Yes => "yes",
        FilterOp::No => "no",
    }
}

fn action_label(action: &FilterAction) -> String {
    match action.kind.as_str() {
        "mark_read" => "mark read".into(),
        "mark_unread" => "mark unread".into(),
        "star" => "star".into(),
        "archive" => "archive".into(),
        "spam" => "mark as spam".into(),
        "trash" => "trash".into(),
        "move" => format!("move to {}", action.value),
        "add_label" => format!("label {}", action.value),
        "copy" => format!("copy to {}", action.value),
        "forward" => format!("forward to {}", action.value),
        "run_command" => format!("run {}", action.value),
        "create_task" => format!("create task ({})", action.value),
        other => other.into(),
    }
}

/// One-line description of a rule for the list.
fn describe(rule: &FilterRule) -> String {
    let joiner = if rule.match_all { " and " } else { " or " };
    let conditions = rule
        .conditions
        .iter()
        .map(|c| match c.field {
            FilterField::Size => format!("{} {} {c_value} KB", field_label(c.field), op_label(c.op), c_value = c.value),
            FilterField::Age => format!(
                "{} {} {} days",
                field_label(c.field),
                if c.op == FilterOp::Greater { "over" } else { "under" },
                c.value
            ),
            FilterField::HasAttachment
            | FilterField::ListMail
            | FilterField::Read
            | FilterField::Starred => {
                format!("{}: {}", field_label(c.field), op_label(c.op))
            }
            FilterField::Header => format!(
                "Header {} {} “{}”",
                c.name,
                op_label(c.op),
                c.value
            ),
            _ => format!("{} {} “{}”", field_label(c.field), op_label(c.op), c.value),
        })
        .collect::<Vec<_>>()
        .join(joiner);
    let actions = rule
        .actions
        .iter()
        .map(action_label)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "If {conditions} → {actions}{}",
        if rule.stop { " (then stop)" } else { "" }
    )
}

fn project(app: &AppWindow, rules: &Rc<RefCell<Vec<FilterRule>>>, settings: &Settings) {
    *rules.borrow_mut() = settings.filter_rules.clone();
    app.global::<FilterRules>().set_rows(ModelRc::new(VecModel::from(
        settings
            .filter_rules
            .iter()
            .map(|r| FilterRuleRow {
                id: r.id.clone().into(),
                name: r.name.clone().into(),
                summary: describe(r).into(),
                enabled: r.enabled,
            })
            .collect::<Vec<_>>(),
    )));
}

/// The rule written in the editor.
fn edited_rule(app: &AppWindow) -> Result<FilterRule, String> {
    let ui = app.global::<FilterRules>();
    let mut conditions = Vec::new();
    for row in ui.get_edit_conditions().iter() {
        let field = *FIELDS
            .get(row.field as usize)
            .ok_or("Choose what a condition looks at")?;
        let op = *ops_for(field)
            .get(row.op as usize)
            .ok_or("Choose how a condition compares")?;
        conditions.push(FilterCondition {
            field,
            op,
            value: row.value.to_string(),
            name: row.name.to_string(),
        });
    }
    let mut actions = Vec::new();
    for row in ui.get_edit_actions().iter() {
        let kind = ACTION_KINDS
            .get(row.kind as usize)
            .ok_or("Choose an action")?;
        actions.push(FilterAction {
            kind: (*kind).to_owned(),
            value: if row.kind >= FIRST_VALUE_KIND {
                row.value.to_string()
            } else {
                String::new()
            },
        });
    }
    Ok(FilterRule {
        id: ui.get_edit_id().to_string(),
        name: ui.get_edit_name().to_string(),
        enabled: true,
        match_all: ui.get_edit_match_all(),
        conditions,
        actions,
        stop: ui.get_edit_stop(),
    })
}

fn default_condition() -> FilterConditionEdit {
    FilterConditionEdit::default()
}

fn default_action() -> FilterActionEdit {
    FilterActionEdit::default()
}

/// A saved rule's "Create task" value as picker positions: status, project and
/// context (0 = the default / none, otherwise the entry's place in the lists).
fn task_picks(app: &AppWindow, value: &str) -> (i32, i32, i32) {
    let settings = app.global::<crate::TaskSettingsUi>();
    let position = |list: slint::ModelRc<SharedString>, name: &str| -> i32 {
        if name.is_empty() {
            return 0;
        }
        list.iter()
            .position(|entry| entry.as_str().eq_ignore_ascii_case(name))
            .map_or(0, |at| at as i32)
    };
    let mut parts = value.split(';');
    let status = match parts.next().map(str::trim) {
        Some("next") => 1,
        Some("waiting") => 2,
        Some("scheduled") => 3,
        _ => 0,
    };
    let project = position(settings.get_project_choices(), parts.next().unwrap_or("").trim());
    let context = position(settings.get_context_choices(), parts.next().unwrap_or("").trim());
    (status, project, context)
}

/// Load a saved rule into the editor to change it.
fn load_into_editor(app: &AppWindow, rule: &FilterRule) {
    let ui = app.global::<FilterRules>();
    ui.set_edit_id(rule.id.clone().into());
    ui.set_edit_name(rule.name.clone().into());
    ui.set_edit_match_all(rule.match_all);
    ui.set_edit_stop(rule.stop);
    let conditions = rule
        .conditions
        .iter()
        .map(|c| FilterConditionEdit {
            field: FIELDS.iter().position(|f| *f == c.field).unwrap_or(0) as i32,
            op: ops_for(c.field).iter().position(|o| *o == c.op).unwrap_or(0) as i32,
            name: c.name.clone().into(),
            value: c.value.clone().into(),
        })
        .collect::<Vec<_>>();
    ui.set_edit_conditions(ModelRc::new(VecModel::from(if conditions.is_empty() {
        vec![default_condition()]
    } else {
        conditions
    })));
    let actions = rule
        .actions
        .iter()
        .map(|a| {
            let kind = ACTION_KINDS.iter().position(|k| *k == a.kind).unwrap_or(0) as i32;
            let (status, project, context) =
                if kind == 11 { task_picks(app, &a.value) } else { (0, 0, 0) };
            FilterActionEdit { kind, value: a.value.clone().into(), status, project, context }
        })
        .collect::<Vec<_>>();
    ui.set_edit_actions(ModelRc::new(VecModel::from(if actions.is_empty() {
        vec![default_action()]
    } else {
        actions
    })));
}

fn clear_editor(app: &AppWindow) {
    let ui = app.global::<FilterRules>();
    ui.set_edit_id(SharedString::default());
    ui.set_edit_name(SharedString::default());
    ui.set_edit_match_all(true);
    ui.set_edit_stop(false);
    ui.set_edit_conditions(ModelRc::new(VecModel::from(vec![default_condition()])));
    ui.set_edit_actions(ModelRc::new(VecModel::from(vec![default_action()])));
}

fn save(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
    rules: &Rc<RefCell<Vec<FilterRule>>>,
    new_list: Vec<FilterRule>,
    done: &'static str,
    clear: bool,
) {
    let Some(core) = core(state) else { return };
    let ui = app.global::<FilterRules>();
    ui.set_busy(true);
    let rules = rules.clone();
    spawn(
        app,
        runtime,
        async move {
            core.save_filter_rules(new_list)
                .await
                .map_err(|e| e.to_string())?;
            core.get_settings().await.map_err(|e| e.to_string())
        },
        move |app, result| {
            let ui = app.global::<FilterRules>();
            ui.set_busy(false);
            match result {
                Ok(settings) => {
                    project(&app, &rules, &settings);
                    if clear {
                        clear_editor(&app);
                    }
                    ui.set_status(done.into());
                }
                Err(error) => ui.set_status(error.into()),
            }
        },
    );
}

pub(super) fn register(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
) {
    let ui = app.global::<FilterRules>();
    let rules = Rc::new(RefCell::new(Vec::<FilterRule>::new()));

    let (weak, st, rt, rl) = (app.as_weak(), state.clone(), runtime.clone(), rules.clone());
    ui.on_context_changed(move || {
        let (Some(app), Some(core)) = (weak.upgrade(), core(&st)) else {
            return;
        };
        let rl = rl.clone();
        spawn(
            &app,
            &rt,
            async move { core.get_settings().await.map_err(|e| e.to_string()) },
            move |app, result| {
                if let Ok(settings) = result {
                    project(&app, &rl, &settings);
                }
            },
        );
    });

    let (weak, st, rt, rl) = (app.as_weak(), state.clone(), runtime.clone(), rules.clone());
    ui.on_add_rule(move || {
        let Some(app) = weak.upgrade() else { return };
        match edited_rule(&app) {
            Ok(rule) => {
                let mut list = rl.borrow().clone();
                // A rule being changed keeps its place and whether it is on.
                match list.iter().position(|r| !rule.id.is_empty() && r.id == rule.id) {
                    Some(at) => {
                        let enabled = list[at].enabled;
                        list[at] = FilterRule { enabled, ..rule };
                        save(&app, &st, &rt, &rl, list, "Filter updated.", true);
                    }
                    None => {
                        list.push(FilterRule { id: String::new(), ..rule });
                        save(&app, &st, &rt, &rl, list, "Filter added.", true);
                    }
                }
            }
            Err(error) => app.global::<FilterRules>().set_status(error.into()),
        }
    });

    let (weak, rl) = (app.as_weak(), rules.clone());
    ui.on_edit_rule(move |id| {
        let Some(app) = weak.upgrade() else { return };
        if let Some(rule) = rl.borrow().iter().find(|r| r.id == id.as_str()) {
            load_into_editor(&app, rule);
            app.global::<FilterRules>().set_status(SharedString::default());
        }
    });
    let weak = app.as_weak();
    ui.on_cancel_edit(move || {
        if let Some(app) = weak.upgrade() {
            clear_editor(&app);
        }
    });

    let (weak, st, rt, rl) = (app.as_weak(), state.clone(), runtime.clone(), rules.clone());
    ui.on_delete_rule(move |id| {
        let Some(app) = weak.upgrade() else { return };
        if app.global::<FilterRules>().get_edit_id() == id {
            clear_editor(&app);
        }
        let mut list = rl.borrow().clone();
        list.retain(|r| r.id != id.as_str());
        save(&app, &st, &rt, &rl, list, "Filter deleted.", false);
    });

    let (weak, st, rt, rl) = (app.as_weak(), state.clone(), runtime.clone(), rules.clone());
    ui.on_toggle_rule(move |id| {
        let Some(app) = weak.upgrade() else { return };
        let mut list = rl.borrow().clone();
        if let Some(rule) = list.iter_mut().find(|r| r.id == id.as_str()) {
            rule.enabled = !rule.enabled;
        }
        save(&app, &st, &rt, &rl, list, "Filter updated.", false);
    });

    let (weak, st, rt, rl) = (app.as_weak(), state.clone(), runtime.clone(), rules.clone());
    ui.on_move_rule(move |id, direction| {
        let Some(app) = weak.upgrade() else { return };
        let mut list = rl.borrow().clone();
        if let Some(from) = list.iter().position(|r| r.id == id.as_str()) {
            let to = from as i64 + i64::from(direction);
            if (0..list.len() as i64).contains(&to) {
                list.swap(from, to as usize);
                save(&app, &st, &rt, &rl, list, "Order saved.", false);
            }
        }
    });

    let weak = app.as_weak();
    ui.on_add_condition(move || {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<FilterRules>();
        let mut list: Vec<_> = ui.get_edit_conditions().iter().collect();
        if list.len() < MAX_CONDITIONS {
            list.push(default_condition());
            ui.set_edit_conditions(ModelRc::new(VecModel::from(list)));
        }
    });
    let weak = app.as_weak();
    ui.on_remove_condition(move |index| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<FilterRules>();
        let mut list: Vec<_> = ui.get_edit_conditions().iter().collect();
        if list.len() > 1 && (index as usize) < list.len() {
            list.remove(index as usize);
            ui.set_edit_conditions(ModelRc::new(VecModel::from(list)));
        }
    });
    let weak = app.as_weak();
    ui.on_update_condition(move |index, row| {
        let Some(app) = weak.upgrade() else { return };
        let model = app.global::<FilterRules>().get_edit_conditions();
        if (index as usize) < model.row_count() {
            model.set_row_data(index as usize, row);
        }
    });
    let weak = app.as_weak();
    ui.on_add_action(move || {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<FilterRules>();
        let mut list: Vec<_> = ui.get_edit_actions().iter().collect();
        if list.len() < MAX_ACTIONS {
            list.push(default_action());
            ui.set_edit_actions(ModelRc::new(VecModel::from(list)));
        }
    });
    let weak = app.as_weak();
    ui.on_remove_action(move |index| {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<FilterRules>();
        let mut list: Vec<_> = ui.get_edit_actions().iter().collect();
        if list.len() > 1 && (index as usize) < list.len() {
            list.remove(index as usize);
            ui.set_edit_actions(ModelRc::new(VecModel::from(list)));
        }
    });
    let weak = app.as_weak();
    ui.on_update_action(move |index, row| {
        let Some(app) = weak.upgrade() else { return };
        let model = app.global::<FilterRules>().get_edit_actions();
        if (index as usize) < model.row_count() {
            model.set_row_data(index as usize, row);
        }
    });

    let (weak, st, rt) = (app.as_weak(), state.clone(), runtime.clone());
    ui.on_run_now(move || {
        let (Some(app), Some(core)) = (weak.upgrade(), core(&st)) else {
            return;
        };
        let ui = app.global::<FilterRules>();
        ui.set_busy(true);
        ui.set_run_status("Running the filters…".into());
        spawn(
            &app,
            &rt,
            async move {
                core.run_filters_on_inboxes()
                    .await
                    .map_err(|e| e.to_string())
            },
            |app, result| {
                let ui = app.global::<FilterRules>();
                ui.set_busy(false);
                ui.set_run_status(match result {
                    Ok(summary) => format!(
                        "Checked {} messages; {} matched a filter.",
                        summary.examined, summary.matched
                    )
                    .into(),
                    Err(error) => error.into(),
                });
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_lists_every_engine_action_and_the_value_kinds_start_at_move() {
        assert_eq!(ACTION_KINDS[FIRST_VALUE_KIND as usize], "move");
        assert_eq!(ACTION_KINDS.last(), Some(&"create_task"));
        // Every kind from "move" on takes a value, every earlier kind none.
        for (index, kind) in ACTION_KINDS.iter().enumerate() {
            let takes_value = matches!(
                *kind,
                "move" | "add_label" | "copy" | "forward" | "run_command" | "create_task"
            );
            assert_eq!(index as i32 >= FIRST_VALUE_KIND, takes_value, "{kind}");
        }
    }

    #[test]
    fn create_task_is_described_in_the_list() {
        let action = FilterAction {
            kind: "create_task".into(),
            value: "next;Taxes".into(),
        };
        assert_eq!(action_label(&action), "create task (next;Taxes)");
    }
}
