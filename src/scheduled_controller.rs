//! The "Scheduled messages" dialog: what is waiting for its send time, and a
//! way to cancel each message. Core does the work; this only moves data.

use super::*;
use rustmail_core::Core;

async fn refresh(core: Arc<Core>, weak: slint::Weak<AppWindow>, cancelled: Option<i64>) {
    let mut error = String::new();
    if let Some(action_id) = cancelled
        && let Err(e) = core.cancel_send(action_id).await
    {
        error = e.to_string();
    }
    let listed = core.list_scheduled_sends().await;
    let _ = weak.upgrade_in_event_loop(move |app| {
        let ui = app.global::<ScheduledUi>();
        match listed {
            Ok(rows) => {
                ui.set_error(error.into());
                ui.set_rows(ModelRc::new(VecModel::from(
                    rows.into_iter()
                        .map(|row| ScheduledRow {
                            action_id: i32::try_from(row.action_id).unwrap_or(-1),
                            subject: row.subject.into(),
                            to: row.to.into(),
                            when: Local
                                .timestamp_millis_opt(row.send_at)
                                .single()
                                .map(|at| at.format("%a %-d %b, %H:%M").to_string())
                                .unwrap_or_default()
                                .into(),
                        })
                        .collect::<Vec<_>>(),
                )));
            }
            Err(e) => ui.set_error(e.to_string().into()),
        }
    });
}

pub(super) fn register(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
) {
    let ui = app.global::<ScheduledUi>();
    {
        let (weak, state, runtime) = (app.as_weak(), Rc::clone(state), Rc::clone(runtime));
        ui.on_request(move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let ui = app.global::<ScheduledUi>();
            ui.set_error("".into());
            ui.set_open(true);
            if let Some(core) = state.borrow().core.as_ref().map(|c| c.file_core()) {
                runtime.spawn(refresh(core, weak.clone(), None));
            }
        });
    }
    let (weak, state, runtime) = (app.as_weak(), Rc::clone(state), Rc::clone(runtime));
    ui.on_cancel(move |action_id| {
        if let Some(core) = state.borrow().core.as_ref().map(|c| c.file_core()) {
            runtime.spawn(refresh(core, weak.clone(), Some(i64::from(action_id))));
        }
    });
}
