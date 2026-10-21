//! The Filters settings page: list, add, reorder, switch and run filter rules.
use super::*;
use crate::user_actions_controller::{core, spawn};
use rustmail_core::models::{
    FilterAction, FilterCondition, FilterField, FilterOp, FilterRule, Settings,
};
use slint::SharedString;

fn parse<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).ok()
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
            FilterField::HasAttachment | FilterField::ListMail => {
                format!("{}: {}", field_label(c.field), op_label(c.op))
            }
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

/// The rule written in the editor; unused condition and action rows are left out.
fn edited_rule(app: &AppWindow) -> Result<FilterRule, String> {
    let ui = app.global::<FilterRules>();
    let mut conditions = Vec::new();
    for (field, op, value) in [
        (ui.get_c1_field(), ui.get_c1_op(), ui.get_c1_value()),
        (ui.get_c2_field(), ui.get_c2_op(), ui.get_c2_value()),
        (ui.get_c3_field(), ui.get_c3_op(), ui.get_c3_value()),
    ] {
        if field.is_empty() {
            continue;
        }
        conditions.push(FilterCondition {
            field: parse(field.as_str()).ok_or("Choose what a condition looks at")?,
            op: parse(op.as_str()).ok_or("Choose how a condition compares")?,
            value: value.to_string(),
        });
    }
    let mut actions = Vec::new();
    for (kind, value) in [
        (ui.get_a1_kind(), ui.get_a1_value()),
        (ui.get_a2_kind(), ui.get_a2_value()),
    ] {
        if !kind.is_empty() {
            actions.push(FilterAction {
                kind: kind.to_string(),
                value: value.to_string(),
            });
        }
    }
    Ok(FilterRule {
        id: String::new(),
        name: ui.get_edit_name().to_string(),
        enabled: true,
        match_all: ui.get_edit_match_all(),
        conditions,
        actions,
        stop: ui.get_edit_stop(),
    })
}

fn clear_editor(app: &AppWindow) {
    let ui = app.global::<FilterRules>();
    let empty = SharedString::default();
    ui.set_edit_name(empty.clone());
    ui.set_edit_match_all(true);
    ui.set_edit_stop(false);
    ui.set_c1_field(empty.clone());
    ui.set_c1_op(empty.clone());
    ui.set_c1_value(empty.clone());
    ui.set_c2_field(empty.clone());
    ui.set_c2_op(empty.clone());
    ui.set_c2_value(empty.clone());
    ui.set_c3_field(empty.clone());
    ui.set_c3_op(empty.clone());
    ui.set_c3_value(empty.clone());
    ui.set_a1_kind(empty.clone());
    ui.set_a1_value(empty.clone());
    ui.set_a2_kind(empty.clone());
    ui.set_a2_value(empty);
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
                list.push(rule);
                save(&app, &st, &rt, &rl, list, "Filter added.", true);
            }
            Err(error) => app.global::<FilterRules>().set_status(error.into()),
        }
    });

    let (weak, st, rt, rl) = (app.as_weak(), state.clone(), runtime.clone(), rules.clone());
    ui.on_delete_rule(move |id| {
        let Some(app) = weak.upgrade() else { return };
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
