//! Mail filters: user rules applied to incoming mail and, on request, to a
//! folder. Conditions look only at facts already stored for each message, so a
//! rule never needs the body. Actions go through the ordinary queued mailbox
//! action path, so they sync to the server exactly like a click.

use crate::{
    Core,
    db::repo,
    error::{CoreError, Result},
    models::{
        ActionKind, ActionParams, Address, FilterAction, FilterCondition, FilterField, FilterOp,
        FilterRule, PerformActionArgs,
    },
};
use rusqlite::params;
use std::collections::HashSet;

pub const MAX_RULES: usize = 50;
const MAX_CONDITIONS: usize = 8;
const MAX_ACTIONS: usize = 6;
const MAX_VALUE_BYTES: usize = 500;
const MAX_NAME_BYTES: usize = 60;
const PENDING_BATCH: i64 = 100;
const MANUAL_LIMIT: i64 = 5000;
const REGEX_SIZE_LIMIT: usize = 256 * 1024;

pub const ACTION_KINDS: [&str; 8] = [
    "mark_read",
    "mark_unread",
    "star",
    "archive",
    "spam",
    "trash",
    "move",
    "add_label",
];

/// What the conditions can see of one message.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub size_bytes: i64,
    pub has_attachments: bool,
    pub list_mail: bool,
}

fn people(json: &str) -> String {
    serde_json::from_str::<Vec<Address>>(json)
        .unwrap_or_default()
        .iter()
        .map(|a| match &a.name {
            Some(name) if !name.is_empty() => format!("{name} <{}>", a.email),
            _ => a.email.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn text_matches(op: FilterOp, haystack: &str, needle: &str) -> bool {
    if op == FilterOp::Regex {
        return regex::RegexBuilder::new(needle)
            .case_insensitive(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .is_ok_and(|re| re.is_match(haystack));
    }
    let (haystack, needle) = (haystack.to_lowercase(), needle.to_lowercase());
    match op {
        FilterOp::Contains => haystack.contains(&needle),
        FilterOp::NotContains => !haystack.contains(&needle),
        FilterOp::Equals => haystack == needle,
        FilterOp::StartsWith => haystack.starts_with(&needle),
        FilterOp::EndsWith => haystack.ends_with(&needle),
        _ => false,
    }
}

pub fn condition_matches(condition: &FilterCondition, facts: &Facts) -> bool {
    let text = match condition.field {
        FilterField::From => &facts.from,
        FilterField::To => &facts.to,
        FilterField::Cc => &facts.cc,
        FilterField::Subject => &facts.subject,
        FilterField::Size => {
            let Ok(limit_kb) = condition.value.trim().parse::<i64>() else {
                return false;
            };
            let kilobytes = facts.size_bytes / 1024;
            return match condition.op {
                FilterOp::Greater => kilobytes > limit_kb,
                FilterOp::Less => kilobytes < limit_kb,
                _ => false,
            };
        }
        FilterField::HasAttachment | FilterField::ListMail => {
            let present = if condition.field == FilterField::HasAttachment {
                facts.has_attachments
            } else {
                facts.list_mail
            };
            return match condition.op {
                FilterOp::Yes => present,
                FilterOp::No => !present,
                _ => false,
            };
        }
    };
    text_matches(condition.op, text, &condition.value)
}

pub fn rule_matches(rule: &FilterRule, facts: &Facts) -> bool {
    rule.enabled
        && !rule.conditions.is_empty()
        && if rule.match_all {
            rule.conditions.iter().all(|c| condition_matches(c, facts))
        } else {
            rule.conditions.iter().any(|c| condition_matches(c, facts))
        }
}

/// The actions to run for one message: those of every matching rule in order,
/// ending at the first matching rule marked `stop`. Duplicates are dropped.
pub fn actions_for(rules: &[FilterRule], facts: &Facts) -> Vec<FilterAction> {
    let mut actions: Vec<FilterAction> = Vec::new();
    for rule in rules.iter().filter(|r| rule_matches(r, facts)) {
        for action in &rule.actions {
            if !actions.contains(action) {
                actions.push(action.clone());
            }
        }
        if rule.stop {
            break;
        }
    }
    actions
}

fn validate_condition(condition: &FilterCondition) -> Result<()> {
    use FilterOp::*;
    let bad = |msg: &str| Err(CoreError::Other(msg.to_owned()));
    if condition.value.len() > MAX_VALUE_BYTES {
        return bad("A condition value can have at most 500 bytes");
    }
    match condition.field {
        FilterField::From | FilterField::To | FilterField::Cc | FilterField::Subject => {
            if !matches!(
                condition.op,
                Contains | NotContains | Equals | StartsWith | EndsWith | Regex
            ) {
                return bad("Choose a text comparison for this condition");
            }
            if condition.value.is_empty() {
                return bad("Enter the text to look for");
            }
            if condition.op == Regex
                && regex::RegexBuilder::new(&condition.value)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .is_err()
            {
                return bad("The regular expression is not valid");
            }
        }
        FilterField::Size => {
            if !matches!(condition.op, Greater | Less)
                || condition.value.trim().parse::<u32>().is_err()
            {
                return bad("Size conditions need “larger/smaller than” and a number of KB");
            }
        }
        FilterField::HasAttachment | FilterField::ListMail => {
            if !matches!(condition.op, Yes | No) {
                return bad("Choose yes or no for this condition");
            }
        }
    }
    Ok(())
}

fn validate_rule(rule: &mut FilterRule) -> Result<()> {
    rule.name = rule.name.trim().to_owned();
    if rule.name.is_empty() || rule.name.len() > MAX_NAME_BYTES {
        return Err(CoreError::Other("Use a rule name of 1–60 bytes".into()));
    }
    if rule.conditions.is_empty() || rule.conditions.len() > MAX_CONDITIONS {
        return Err(CoreError::Other("A rule needs 1–8 conditions".into()));
    }
    if rule.actions.is_empty() || rule.actions.len() > MAX_ACTIONS {
        return Err(CoreError::Other("A rule needs 1–6 actions".into()));
    }
    for condition in &mut rule.conditions {
        condition.value = condition.value.trim().to_owned();
        validate_condition(condition)?;
    }
    for action in &mut rule.actions {
        action.value = action.value.trim().to_owned();
        if !ACTION_KINDS.contains(&action.kind.as_str()) {
            return Err(CoreError::Other("Choose a supported action".into()));
        }
        let needs_value = matches!(action.kind.as_str(), "move" | "add_label");
        if needs_value != !action.value.is_empty() || action.value.len() > MAX_VALUE_BYTES {
            return Err(CoreError::Other(
                "Move and label actions need a folder or label name; other actions take none"
                    .into(),
            ));
        }
    }
    if rule.id.is_empty() {
        rule.id = format!("flt-{:032x}", rand::random::<u128>());
    }
    Ok(())
}

/// The folder in `account` whose full name or last path segment equals `name`.
fn find_folder(conn: &rusqlite::Connection, account_id: i64, name: &str) -> Result<Option<i64>> {
    let wanted = name.to_lowercase();
    let mut statement = conn.prepare(
        "SELECT id, imap_name, delimiter FROM folders WHERE account_id = ?1 AND selectable = 1",
    )?;
    let folders = statement
        .query_map(params![account_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, imap_name, delimiter) in &folders {
        if imap_name.to_lowercase() == wanted {
            return Ok(Some(*id));
        }
        let last = delimiter
            .as_deref()
            .filter(|d| !d.is_empty())
            .and_then(|d| imap_name.rsplit(d).next())
            .unwrap_or(imap_name);
        if last.to_lowercase() == wanted {
            return Ok(Some(*id));
        }
    }
    Ok(None)
}

fn find_label(conn: &rusqlite::Connection, account_id: i64, name: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM labels
             WHERE is_auto = 0 AND name = ?2 COLLATE NOCASE
               AND (owner_account_id IS NULL OR owner_account_id = ?1)
             ORDER BY owner_account_id IS NULL LIMIT 1",
            params![account_id, name],
            |r| r.get(0),
        )
        .ok())
}

fn facts_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, Option<i64>, i64, Facts)> {
    let name: Option<String> = r.get("from_name")?;
    let addr: Option<String> = r.get("from_addr")?;
    let from = match (name.filter(|n| !n.is_empty()), addr) {
        (Some(name), Some(addr)) => format!("{name} <{addr}>"),
        (None, Some(addr)) => addr,
        (Some(name), None) => name,
        (None, None) => String::new(),
    };
    Ok((
        r.get("id")?,
        r.get("thread_id")?,
        r.get("account_id")?,
        Facts {
            from,
            to: people(&r.get::<_, String>("to_json")?),
            cc: people(&r.get::<_, String>("cc_json")?),
            subject: r.get("subject")?,
            size_bytes: r.get::<_, Option<i64>>("size")?.unwrap_or(0),
            has_attachments: r.get::<_, i64>("has_attachments")? != 0,
            list_mail: r.get::<_, Option<String>>("list_unsubscribe")?.is_some(),
        },
    ))
}

const FACT_COLUMNS: &str = "id, thread_id, account_id, from_name, from_addr, to_json, cc_json,
     subject, size, has_attachments, list_unsubscribe";

/// How many messages a manual run looked at and how many matched a rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilterRunSummary {
    pub examined: usize,
    pub matched: usize,
}

impl Core {
    pub async fn save_filter_rules(&self, mut rules: Vec<FilterRule>) -> Result<Vec<FilterRule>> {
        if rules.len() > MAX_RULES {
            return Err(CoreError::Other(format!("Use at most {MAX_RULES} rules")));
        }
        for rule in &mut rules {
            validate_rule(rule)?;
        }
        let saved = rules.clone();
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                settings.filter_rules = rules;
                repo::settings::set(conn, &settings)
            })
            .await?;
        Ok(saved)
    }

    /// Apply the filters to mail that arrived since the last pass. Messages
    /// are marked handled before their actions run, so a crash can skip a
    /// filter but never repeats one. Returns how many messages matched.
    pub async fn apply_pending_filters(&self) -> Result<usize> {
        let (rules, batch) = self
            .db
            .write(move |conn| {
                // Only mail delivered to an Inbox is filtered on receipt.
                conn.execute(
                    "UPDATE messages SET filter_state = 1
                     WHERE filter_state = 0 AND (is_outgoing = 1 OR is_draft = 1
                        OR folder_id IS NULL
                        OR folder_id NOT IN (SELECT id FROM folders WHERE role = 'inbox'))",
                    [],
                )?;
                let settings = repo::settings::get(conn)?;
                let batch = {
                    let mut statement = conn.prepare(&format!(
                        "SELECT {FACT_COLUMNS} FROM messages
                         WHERE filter_state = 0 ORDER BY id LIMIT ?1"
                    ))?;
                    statement
                        .query_map(params![PENDING_BATCH], facts_from_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                for (id, ..) in &batch {
                    conn.execute(
                        "UPDATE messages SET filter_state = 1 WHERE id = ?1",
                        params![id],
                    )?;
                }
                let rules: Vec<FilterRule> = settings
                    .filter_rules
                    .into_iter()
                    .filter(|r| r.enabled)
                    .collect();
                Ok((rules, batch))
            })
            .await?;
        if rules.is_empty() || batch.is_empty() {
            return Ok(0);
        }
        self.run_rules(&rules, batch).await
    }

    /// Run the filters on every incoming message already in `folder_id`.
    pub async fn run_filters_now(&self, folder_id: i64) -> Result<FilterRunSummary> {
        let (rules, batch) = self
            .db
            .read(move |conn| {
                let rules: Vec<FilterRule> = repo::settings::get(conn)?
                    .filter_rules
                    .into_iter()
                    .filter(|r| r.enabled)
                    .collect();
                let mut statement = conn.prepare(&format!(
                    "SELECT {FACT_COLUMNS} FROM messages
                     WHERE folder_id = ?1 AND is_outgoing = 0 AND is_draft = 0
                     ORDER BY date DESC LIMIT ?2"
                ))?;
                let batch = statement
                    .query_map(params![folder_id, MANUAL_LIMIT], facts_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((rules, batch))
            })
            .await?;
        let examined = batch.len();
        let matched = if rules.is_empty() {
            0
        } else {
            self.run_rules(&rules, batch).await?
        };
        Ok(FilterRunSummary { examined, matched })
    }

    /// Run the filters on the Inbox of every account (the manual "run now").
    pub async fn run_filters_on_inboxes(&self) -> Result<FilterRunSummary> {
        let folders: Vec<i64> = self
            .db
            .read(|conn| {
                let mut statement =
                    conn.prepare("SELECT id FROM folders WHERE role = 'inbox' AND selectable = 1")?;
                Ok(statement
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        let mut total = FilterRunSummary::default();
        for folder in folders {
            let summary = self.run_filters_now(folder).await?;
            total.examined += summary.examined;
            total.matched += summary.matched;
        }
        Ok(total)
    }

    async fn run_rules(
        &self,
        rules: &[FilterRule],
        batch: Vec<(i64, Option<i64>, i64, Facts)>,
    ) -> Result<usize> {
        // One conversation can hold several matching messages; act once each.
        let mut seen: HashSet<(i64, String, String)> = HashSet::new();
        let mut plans: Vec<(i64, i64, Vec<FilterAction>)> = Vec::new();
        let mut matched = 0;
        for (_, thread, account, facts) in &batch {
            let actions = actions_for(rules, facts);
            let Some(thread) = thread else { continue };
            if actions.is_empty() {
                continue;
            }
            matched += 1;
            let fresh: Vec<FilterAction> = actions
                .into_iter()
                .filter(|a| seen.insert((*thread, a.kind.clone(), a.value.clone())))
                .collect();
            if !fresh.is_empty() {
                plans.push((*thread, *account, fresh));
            }
        }
        for (thread_id, account_id, actions) in plans {
            for action in actions {
                let Some((kind, params)) = self.resolve_filter_action(account_id, &action).await?
                else {
                    tracing::warn!(
                        "filter action {} {:?} skipped: its target does not exist for account {account_id}",
                        action.kind,
                        action.value
                    );
                    continue;
                };
                if let Err(error) = self
                    .perform_action(PerformActionArgs {
                        kind,
                        thread_ids: vec![thread_id],
                        params,
                    })
                    .await
                {
                    tracing::warn!(
                        "filter action {} failed for thread {thread_id}: {error}",
                        action.kind
                    );
                }
            }
        }
        Ok(matched)
    }

    async fn resolve_filter_action(
        &self,
        account_id: i64,
        action: &FilterAction,
    ) -> Result<Option<(ActionKind, Option<ActionParams>)>> {
        let simple = match action.kind.as_str() {
            "mark_read" => Some(ActionKind::MarkRead),
            "mark_unread" => Some(ActionKind::MarkUnread),
            "star" => Some(ActionKind::Star),
            "archive" => Some(ActionKind::Archive),
            "spam" => Some(ActionKind::Spam),
            "trash" => Some(ActionKind::Trash),
            _ => None,
        };
        if let Some(kind) = simple {
            return Ok(Some((kind, None)));
        }
        let name = action.value.clone();
        match action.kind.as_str() {
            "move" => {
                let folder = self
                    .db
                    .read(move |conn| find_folder(conn, account_id, &name))
                    .await?;
                Ok(folder.map(|id| {
                    (
                        ActionKind::Move,
                        Some(ActionParams {
                            wake_at: None,
                            target_folder_id: Some(id),
                            label_id: None,
                        }),
                    )
                }))
            }
            "add_label" => {
                let label = self
                    .db
                    .read(move |conn| find_label(conn, account_id, &name))
                    .await?;
                Ok(label.map(|id| {
                    (
                        ActionKind::AddLabel,
                        Some(ActionParams {
                            wake_at: None,
                            target_folder_id: None,
                            label_id: Some(id),
                        }),
                    )
                }))
            }
            _ => Ok(None),
        }
    }

    /// Long-lived loop: filter new inbox mail whenever the mailbox changes.
    pub(crate) async fn filter_loop(&self) {
        use tokio::sync::broadcast::error::RecvError;
        let mut rx = self.bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(crate::events::CoreEvent::MailUpdated { .. }) => loop {
                    match self.apply_pending_filters().await {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(error) => {
                            tracing::warn!("filter pass failed: {error}");
                            break;
                        }
                    }
                },
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(field: FilterField, op: FilterOp, value: &str) -> FilterCondition {
        FilterCondition {
            field,
            op,
            value: value.into(),
        }
    }

    fn facts() -> Facts {
        Facts {
            from: "Eve Example <eve@example.net>".into(),
            to: "Alice <alice@example.org>, bob@example.org".into(),
            cc: String::new(),
            subject: "Invoice #42 for March".into(),
            size_bytes: 300 * 1024,
            has_attachments: true,
            list_mail: false,
        }
    }

    fn rule(name: &str, conditions: Vec<FilterCondition>, kind: &str, stop: bool) -> FilterRule {
        FilterRule {
            id: name.into(),
            name: name.into(),
            enabled: true,
            match_all: true,
            conditions,
            actions: vec![FilterAction {
                kind: kind.into(),
                value: String::new(),
            }],
            stop,
        }
    }

    #[test]
    fn text_conditions_ignore_case_and_cover_names_and_addresses() {
        let f = facts();
        use FilterField::*;
        use FilterOp::*;
        assert!(condition_matches(&cond(From, Contains, "EVE EXAMPLE"), &f));
        assert!(condition_matches(&cond(From, EndsWith, "example.net>"), &f));
        assert!(condition_matches(&cond(To, Contains, "bob@"), &f));
        assert!(condition_matches(&cond(Subject, StartsWith, "invoice"), &f));
        assert!(condition_matches(
            &cond(Subject, Regex, r"#\d+ for (March|April)"),
            &f
        ));
        assert!(condition_matches(
            &cond(Subject, NotContains, "receipt"),
            &f
        ));
        assert!(!condition_matches(&cond(Subject, Equals, "invoice"), &f));
        assert!(!condition_matches(&cond(Cc, Contains, "a"), &f));
        // An invalid expression never matches (and cannot be saved).
        assert!(!condition_matches(&cond(Subject, Regex, "(unclosed"), &f));
    }

    #[test]
    fn size_and_flag_conditions() {
        let f = facts();
        use FilterField::*;
        use FilterOp::*;
        assert!(condition_matches(&cond(Size, Greater, "256"), &f));
        assert!(!condition_matches(&cond(Size, Greater, "300"), &f));
        assert!(condition_matches(&cond(Size, Less, "400"), &f));
        assert!(!condition_matches(&cond(Size, Less, "many"), &f));
        assert!(condition_matches(&cond(HasAttachment, Yes, ""), &f));
        assert!(condition_matches(&cond(ListMail, No, ""), &f));
        assert!(!condition_matches(&cond(ListMail, Yes, ""), &f));
    }

    #[test]
    fn all_any_disabled_and_stop_semantics() {
        let f = facts();
        let from_eve = cond(FilterField::From, FilterOp::Contains, "eve");
        let other = cond(FilterField::Subject, FilterOp::Contains, "receipt");
        let mut r = rule("r", vec![from_eve.clone(), other.clone()], "star", false);
        assert!(!rule_matches(&r, &f));
        r.match_all = false;
        assert!(rule_matches(&r, &f));
        r.enabled = false;
        assert!(!rule_matches(&r, &f));

        let rules = vec![
            rule("first", vec![from_eve.clone()], "mark_read", false),
            rule("second", vec![from_eve.clone()], "star", true),
            rule("third", vec![from_eve.clone()], "trash", false),
            rule("dup", vec![from_eve], "mark_read", false),
        ];
        let kinds: Vec<_> = actions_for(&rules, &f)
            .into_iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(
            kinds,
            ["mark_read", "star"],
            "stop ends the list, duplicates collapse"
        );
    }

    #[test]
    fn rules_are_validated_before_they_are_saved() {
        let good = || {
            rule(
                "Invoices",
                vec![cond(FilterField::Subject, FilterOp::Contains, "invoice")],
                "star",
                false,
            )
        };
        let mut r = good();
        r.id.clear();
        validate_rule(&mut r).unwrap();
        assert!(r.id.starts_with("flt-"));
        let check = |edit: &dyn Fn(&mut FilterRule)| {
            let mut r = good();
            edit(&mut r);
            validate_rule(&mut r).is_err()
        };
        assert!(check(&|r| r.name = "  ".into()));
        assert!(check(&|r| r.conditions.clear()));
        assert!(check(&|r| r.actions.clear()));
        assert!(check(&|r| r.conditions[0].value.clear()));
        assert!(check(&|r| r.conditions[0].op = FilterOp::Yes));
        assert!(check(&|r| {
            r.conditions[0].op = FilterOp::Regex;
            r.conditions[0].value = "(".into();
        }));
        assert!(check(&|r| {
            r.conditions[0].field = FilterField::Size;
            r.conditions[0].op = FilterOp::Greater;
            r.conditions[0].value = "big".into();
        }));
        assert!(check(&|r| r.actions[0].kind = "delete_permanently".into()));
        assert!(
            check(&|r| r.actions[0].kind = "move".into()),
            "move needs a folder"
        );
        assert!(
            check(&|r| r.actions[0].value = "Archive".into()),
            "star takes no value"
        );
        assert!(check(&|r| r.conditions = (0..9)
            .map(|_| cond(FilterField::From, FilterOp::Contains, "x"))
            .collect()));
    }
}
