//! User-defined message actions and the external composing editor.
use super::*;
use rustmail_core::{
    Core, actions,
    models::{MessageAction, Settings},
};
use slint::SharedString;
use std::{cell::Cell, future::Future};

pub(super) fn core(state: &Rc<RefCell<InboxState>>) -> Option<Arc<Core>> {
    state
        .borrow()
        .core
        .as_ref()
        .map(|s| s.account_preferences_core())
}

/// Run `work` on the runtime and hand its result to `apply` on the UI thread.
pub(super) fn spawn<T: Send + 'static>(
    app: &AppWindow,
    runtime: &Rc<tokio::runtime::Runtime>,
    work: impl Future<Output = Result<T, String>> + Send + 'static,
    apply: impl FnOnce(AppWindow, Result<T, String>) + 'static,
) {
    let weak = app.as_weak();
    let task = runtime.spawn(work);
    let abort = task.abort_handle();
    if slint::spawn_local(async move {
        let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
        if let Some(app) = weak.upgrade() {
            apply(app, result);
        }
    })
    .is_err()
    {
        abort.abort();
    }
}

fn project(app: &AppWindow, settings: &Settings) {
    let ui = app.global::<UserActions>();
    ui.set_rows(ModelRc::new(VecModel::from(
        settings
            .message_actions
            .iter()
            .map(|a| UserActionRow {
                id: a.id.clone().into(),
                name: a.name.clone().into(),
                command: a.command.clone().into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_menu_entries(ModelRc::new(VecModel::from(
        settings
            .message_actions
            .iter()
            .map(|a| ActionMenuEntry {
                id: a.id.clone().into(),
                title: a.name.clone().into(),
                icon: Default::default(),
                disabled: false,
                destructive: false,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_editor_command(settings.external_editor_command.clone().into());
    ui.set_editor_available(!settings.external_editor_command.trim().is_empty());
}

/// At most this many checked messages are acted on in one go.
const MAX_ACTION_MESSAGES: usize = 100;

enum ActionRun {
    One(rustmail_core::actions::ActionOutcome),
    Many { total: usize, failures: Vec<String> },
}

fn current_actions(app: &AppWindow) -> Vec<MessageAction> {
    app.global::<UserActions>()
        .get_rows()
        .iter()
        .map(|row| MessageAction {
            id: row.id.to_string(),
            name: row.name.to_string(),
            command: row.command.to_string(),
        })
        .collect()
}

/// Save a new action list and refresh the settings page and reader menu.
fn save_actions(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
    actions_list: Vec<MessageAction>,
    done: &'static str,
    clear_form: bool,
) {
    let Some(core) = core(state) else {
        return;
    };
    let ui = app.global::<UserActions>();
    ui.set_busy(true);
    spawn(
        app,
        runtime,
        async move {
            core.save_message_actions(actions_list)
                .await
                .map_err(|e| e.to_string())?;
            core.get_settings().await.map_err(|e| e.to_string())
        },
        move |app, result| {
            let ui = app.global::<UserActions>();
            ui.set_busy(false);
            match result {
                Ok(settings) => {
                    project(&app, &settings);
                    if clear_form {
                        ui.set_new_name(SharedString::default());
                        ui.set_new_command(SharedString::default());
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
    document: &Rc<RefCell<RichComposeDocument>>,
    editor: &Rc<RefCell<LazyComposeEditor>>,
) {
    let ui = app.global::<UserActions>();

    let (weak, state_load, runtime_load) = (app.as_weak(), state.clone(), runtime.clone());
    ui.on_context_changed(move || {
        let (Some(app), Some(core)) = (weak.upgrade(), core(&state_load)) else {
            return;
        };
        spawn(
            &app,
            &runtime_load,
            async move { core.get_settings().await.map_err(|e| e.to_string()) },
            |app, result| {
                if let Ok(settings) = result {
                    project(&app, &settings);
                }
            },
        );
    });

    let (weak, state_add, runtime_add) = (app.as_weak(), state.clone(), runtime.clone());
    ui.on_add_action(move || {
        let Some(app) = weak.upgrade() else { return };
        let ui = app.global::<UserActions>();
        let mut list = current_actions(&app);
        list.push(MessageAction {
            id: String::new(),
            name: ui.get_new_name().to_string(),
            command: ui.get_new_command().to_string(),
        });
        save_actions(&app, &state_add, &runtime_add, list, "Action added.", true);
    });

    let (weak, state_delete, runtime_delete) = (app.as_weak(), state.clone(), runtime.clone());
    ui.on_delete_action(move |id| {
        let Some(app) = weak.upgrade() else { return };
        let mut list = current_actions(&app);
        list.retain(|a| a.id != id.as_str());
        save_actions(&app, &state_delete, &runtime_delete, list, "Action deleted.", false);
    });

    let (weak, state_editor, runtime_editor) = (app.as_weak(), state.clone(), runtime.clone());
    ui.on_save_editor(move || {
        let (Some(app), Some(core)) = (weak.upgrade(), core(&state_editor)) else {
            return;
        };
        let ui = app.global::<UserActions>();
        let command = ui.get_editor_command().to_string();
        ui.set_busy(true);
        spawn(
            &app,
            &runtime_editor,
            async move {
                core.set_external_editor_command(command)
                    .await
                    .map_err(|e| e.to_string())
            },
            |app, result| {
                let ui = app.global::<UserActions>();
                ui.set_busy(false);
                match result {
                    Ok(saved) => {
                        ui.set_editor_command(saved.clone().into());
                        ui.set_editor_available(!saved.is_empty());
                        ui.set_status("Editor command saved.".into());
                    }
                    Err(error) => ui.set_status(error.into()),
                }
            },
        );
    });

    let (weak, state_run, runtime_run) = (app.as_weak(), state.clone(), runtime.clone());
    ui.on_run(move |action_id| {
        let Some(app) = weak.upgrade() else { return };
        // Checked messages when there are any (one run per conversation's
        // newest message); otherwise the message shown in the reader: the
        // open conversation's selected message when it has loaded, otherwise
        // the conversation's newest.
        let targets: Vec<(Option<i64>, Option<i64>)> = {
            let state = state_run.borrow();
            let checked: Vec<_> = state
                .messages
                .iter()
                .filter(|m| state.checked_ids.contains(&m.id))
                .filter_map(|m| m.thread_id)
                .map(|thread| (None, Some(thread)))
                .collect();
            if !checked.is_empty() {
                checked
            } else {
                state
                    .selected_id
                    .and_then(|selected| {
                        let shown = state
                            .conversation_messages
                            .get(state.conversation_selected_index)
                            .filter(|_| state.conversation_owner_id == Some(selected));
                        match shown.map(|m| m.message_id).filter(|id| *id > 0) {
                            Some(message_id) => Some((Some(message_id), None)),
                            None => state
                                .messages
                                .iter()
                                .find(|m| m.id == selected)
                                .and_then(|m| m.thread_id)
                                .map(|thread| (None, Some(thread))),
                        }
                    })
                    .into_iter()
                    .collect()
            }
        };
        let (Some(core), false) = (core(&state_run), targets.is_empty()) else {
            app.set_toast(UiMessage::plain("Select a message first."));
            return;
        };
        let name = current_actions(&app)
            .into_iter()
            .find(|a| a.id == action_id.as_str())
            .map(|a| a.name)
            .unwrap_or_default();
        app.set_toast(UiMessage::detail("Running action “{}”…", &name));
        let id = action_id.to_string();
        let targets: Vec<_> = targets.into_iter().take(MAX_ACTION_MESSAGES).collect();
        spawn(
            &app,
            &runtime_run,
            async move {
                let mut outcomes = Vec::new();
                for (message_id, thread_id) in &targets {
                    let message_id = match (message_id, thread_id) {
                        (Some(id), _) => *id,
                        (None, Some(thread)) => core
                            .latest_message_in_thread(*thread)
                            .await
                            .map_err(|e| e.to_string())?,
                        (None, None) => return Err("no message is selected".to_owned()),
                    };
                    outcomes.push(
                        core.run_message_action(message_id, id.clone())
                            .await
                            .map_err(|e| e.to_string()),
                    );
                }
                if outcomes.len() == 1 {
                    return outcomes.remove(0).map(ActionRun::One);
                }
                let failures: Vec<String> = outcomes
                    .iter()
                    .filter_map(|o| match o {
                        Ok(outcome) if outcome.success => None,
                        Ok(outcome) => Some(outcome.output.clone()),
                        Err(error) => Some(error.clone()),
                    })
                    .collect();
                Ok(ActionRun::Many {
                    total: outcomes.len(),
                    failures,
                })
            },
            move |app, result| {
                let message = match result {
                    Ok(ActionRun::Many { total, failures }) => {
                        let summary = if failures.is_empty() {
                            "all finished".to_owned()
                        } else {
                            format!("{} failed: {}", failures.len(), failures[0])
                        };
                        UiMessage::three_arguments(
                            "Action “{}” ran on {} messages: {}",
                            &name,
                            total,
                            summary,
                        )
                    }
                    Ok(ActionRun::One(outcome)) if outcome.success && outcome.output.is_empty() => {
                        UiMessage::detail("Action “{}” finished.", &name)
                    }
                    Ok(ActionRun::One(outcome)) if outcome.success => {
                        UiMessage::arguments("Action “{}” finished: {}", &name, outcome.output)
                    }
                    Ok(ActionRun::One(outcome)) => match outcome.code {
                        Some(code) => UiMessage::three_arguments(
                            "Action “{}” failed (exit {}): {}",
                            &name,
                            code,
                            outcome.output,
                        ),
                        None => {
                            UiMessage::arguments("Action “{}” failed: {}", &name, outcome.output)
                        }
                    },
                    Err(error) => {
                        UiMessage::arguments("Action “{}” could not run: {}", &name, error)
                    }
                };
                app.set_toast(message);
            },
        );
    });

    let editing = Rc::new(Cell::new(false));
    let (weak, state_edit, runtime_edit) = (app.as_weak(), state.clone(), runtime.clone());
    let (document, editor) = (document.clone(), editor.clone());
    ui.on_edit_externally(move || {
        let Some(app) = weak.upgrade() else { return };
        let Some(core) = core(&state_edit) else { return };
        if editing.replace(true) {
            return;
        }
        let text = document.borrow().text().to_owned();
        app.set_compose_notice(UiMessage::plain(
            "Editing in your external editor. Save and close it to bring the text back.",
        ));
        app.set_compose_notice_is_error(false);
        let (document, editor, editing) = (document.clone(), editor.clone(), editing.clone());
        spawn(
            &app,
            &runtime_edit,
            async move {
                let settings = core.get_settings().await.map_err(|e| e.to_string())?;
                actions::edit_externally(
                    &settings.external_editor_command,
                    &text,
                    actions::EDITOR_TIMEOUT,
                )
                .await
                .map_err(|e| e.to_string())
            },
            move |app, result| {
                editing.set(false);
                match result {
                    Ok(new_text) => {
                        let mut document = document.borrow_mut();
                        let selection = document.synchronize(&new_text, 0, 0);
                        apply_rich_compose(&app, &document, selection, &mut editor.borrow_mut());
                        app.set_compose_notice(UiMessage::plain("Text updated from your editor."));
                        app.set_compose_notice_is_error(false);
                    }
                    Err(error) => {
                        app.set_compose_notice(UiMessage::detail("External editor: {}", error));
                        app.set_compose_notice_is_error(true);
                    }
                }
            },
        );
    });
}
