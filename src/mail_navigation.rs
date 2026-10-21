//! Keyboard targets for the visible mailbox projection, independent of widgets.

use crate::{MailListEntry, MailNavigationTarget};
use slint::{Model, ModelRc, SharedString};

pub(super) fn target(
    entries: ModelRc<MailListEntry>,
    current_id: i32,
    movement: SharedString,
    step: i32,
) -> MailNavigationTarget {
    let mut count = 0_usize;
    let mut current = None;
    let mut selected = None;
    for index in 0..entries.row_count() {
        let Some(entry) = entries.row_data(index) else {
            continue;
        };
        if !entry.is_header && entry.show_row {
            if entry.email.selected {
                selected = Some(count);
            }
            if entry.email.id == current_id && current.is_none() {
                current = Some(count);
            }
            count += 1;
        }
    }
    if count == 0 {
        return MailNavigationTarget {
            id: -1,
            entry_index: -1,
            ..Default::default()
        };
    }
    let current = current.or(selected);
    let last = count - 1;
    let step = step.max(1) as usize;
    let index = match movement.as_str() {
        "next" => current.map_or(0, |index| index.saturating_add(step).min(last)),
        "previous" => current.map_or(last, |index| index.saturating_sub(step)),
        "first" => 0,
        "last" => last,
        _ => current.unwrap_or(0),
    };
    let mut headers = 0;
    let mut messages = 0;
    for entry_index in 0..entries.row_count() {
        let Some(entry) = entries.row_data(entry_index) else {
            continue;
        };
        if entry.is_header {
            headers += 1;
        } else if entry.show_row {
            if messages == index {
                return MailNavigationTarget {
                    id: entry.email.id,
                    entry_index: entry_index as i32,
                    preceding_headers: headers,
                    preceding_messages: messages as i32,
                    checked: entry.email.checked,
                };
            }
            messages += 1;
        }
    }
    // A model can be backed by a dynamic source; tolerate a row disappearing
    // between the two reads instead of selecting an unrelated message.
    MailNavigationTarget {
        id: -1,
        entry_index: -1,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_skips_headers_and_collapsed_rows() {
        let rows = vec![
            MailListEntry {
                is_header: true,
                ..Default::default()
            },
            MailListEntry {
                show_row: false,
                email: crate::EmailRow {
                    id: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            MailListEntry {
                show_row: true,
                email: crate::EmailRow {
                    id: 2,
                    selected: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            MailListEntry {
                is_header: true,
                ..Default::default()
            },
            MailListEntry {
                show_row: true,
                email: crate::EmailRow {
                    id: 3,
                    checked: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        let model = ModelRc::new(slint::VecModel::from(rows));
        let next = target(model.clone(), -1, "next".into(), 1);
        assert_eq!(next.id, 3);
        assert_eq!(next.entry_index, 4);
        assert_eq!(next.preceding_headers, 2);
        assert_eq!(next.preceding_messages, 1);
        assert!(next.checked);
        assert_eq!(target(model.clone(), 3, "previous".into(), i32::MAX).id, 2);
        assert_eq!(target(model.clone(), 3, "first".into(), 0).id, 2);
        assert_eq!(target(model, 2, "last".into(), 1).id, 3);
        assert_eq!(target(ModelRc::default(), -1, "next".into(), 1).id, -1);
    }
}
