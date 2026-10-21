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

/// Forwarded copies sent per filter pass: a guard against mail loops.
const MAX_FORWARDS_PER_PASS: usize = 50;
const MAX_COMMAND_BYTES: usize = 2048;
/// Largest header block and body text a condition looks at.
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_PARSED_BYTES: usize = 4 * 1024 * 1024;

pub const ACTION_KINDS: [&str; 11] = [
    "mark_read",
    "mark_unread",
    "star",
    "archive",
    "spam",
    "trash",
    "move",
    "add_label",
    "copy",
    "forward",
    "run_command",
];

/// Actions that work on the matched message, not on its whole conversation.
fn is_message_action(kind: &str) -> bool {
    matches!(kind, "copy" | "forward" | "run_command")
}

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
    pub is_read: bool,
    pub is_starred: bool,
    pub age_days: i64,
    /// Headers of the original message (decoded where possible); `None` when
    /// they were not loaded, in which case header conditions never match.
    pub headers: Option<Vec<(String, String)>>,
    /// Plain-text body; `None` when not loaded (body conditions never match).
    pub body: Option<String>,
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
    let flag = |present: bool| match condition.op {
        FilterOp::Yes => present,
        FilterOp::No => !present,
        _ => false,
    };
    let number = |actual: i64| {
        let Ok(limit) = condition.value.trim().parse::<i64>() else {
            return false;
        };
        match condition.op {
            FilterOp::Greater => actual > limit,
            FilterOp::Less => actual < limit,
            _ => false,
        }
    };
    match condition.field {
        FilterField::From => text_matches(condition.op, &facts.from, &condition.value),
        FilterField::To => text_matches(condition.op, &facts.to, &condition.value),
        FilterField::Cc => text_matches(condition.op, &facts.cc, &condition.value),
        FilterField::Subject => text_matches(condition.op, &facts.subject, &condition.value),
        FilterField::Body => facts
            .body
            .as_deref()
            .is_some_and(|body| text_matches(condition.op, body, &condition.value)),
        FilterField::Header => {
            let Some(headers) = &facts.headers else {
                return false;
            };
            let name = condition.name.trim();
            let mut values = headers
                .iter()
                .filter(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str());
            if condition.op == FilterOp::NotContains {
                // A header that is absent does not contain the text.
                values.all(|value| text_matches(condition.op, value, &condition.value))
            } else {
                values.any(|value| text_matches(condition.op, value, &condition.value))
            }
        }
        FilterField::Size => number(facts.size_bytes / 1024),
        FilterField::Age => number(facts.age_days),
        FilterField::HasAttachment => flag(facts.has_attachments),
        FilterField::ListMail => flag(facts.list_mail),
        FilterField::Read => flag(facts.is_read),
        FilterField::Starred => flag(facts.is_starred),
    }
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

/// Whether `rule` could match once header and body text are known: those
/// conditions count as possibly true.
fn might_match(rule: &FilterRule, facts: &Facts) -> bool {
    let possible = |c: &FilterCondition| {
        matches!(c.field, FilterField::Header | FilterField::Body) || condition_matches(c, facts)
    };
    rule.enabled
        && !rule.conditions.is_empty()
        && if rule.match_all {
            rule.conditions.iter().all(possible)
        } else {
            rule.conditions.iter().any(possible)
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

fn validate_condition(condition: &mut FilterCondition) -> Result<()> {
    use FilterOp::*;
    let bad = |msg: &str| Err(CoreError::Other(msg.to_owned()));
    if condition.value.len() > MAX_VALUE_BYTES {
        return bad("A condition value can have at most 500 bytes");
    }
    condition.name = condition.name.trim().to_owned();
    if condition.field != FilterField::Header {
        condition.name.clear();
    }
    match condition.field {
        FilterField::From
        | FilterField::To
        | FilterField::Cc
        | FilterField::Subject
        | FilterField::Body
        | FilterField::Header => {
            if !matches!(
                condition.op,
                Contains | NotContains | Equals | StartsWith | EndsWith | Regex
            ) {
                return bad("Choose a text comparison for this condition");
            }
            if condition.field == FilterField::Header {
                let name = &condition.name;
                if name.is_empty()
                    || name.len() > 100
                    || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':')
                {
                    return bad("Enter a header name such as X-Spam-Flag (no spaces or colon)");
                }
                // An empty value with "contains" asks only whether the header exists.
                if condition.value.is_empty() && condition.op != Contains {
                    return bad("Enter the text to look for");
                }
            } else if condition.value.is_empty() {
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
        FilterField::Size | FilterField::Age => {
            if !matches!(condition.op, Greater | Less)
                || condition.value.trim().parse::<u32>().is_err()
            {
                return bad(
                    "Size and age conditions need “larger/older” or “smaller/newer” and a number",
                );
            }
        }
        FilterField::HasAttachment
        | FilterField::ListMail
        | FilterField::Read
        | FilterField::Starred => {
            if !matches!(condition.op, Yes | No) {
                return bad("Choose yes or no for this condition");
            }
        }
    }
    Ok(())
}

/// A plain address: one `@`, no spaces, separators or control characters.
fn valid_forward_address(address: &str) -> bool {
    let mut parts = address.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && address.len() <= 254
        && address
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, ',' | ';' | '<' | '>' | '(' | ')' | '"'))
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
        let needs_value = !matches!(
            action.kind.as_str(),
            "mark_read" | "mark_unread" | "star" | "archive" | "spam" | "trash"
        );
        let max = if action.kind == "run_command" {
            MAX_COMMAND_BYTES
        } else {
            MAX_VALUE_BYTES
        };
        if needs_value != !action.value.is_empty() || action.value.len() > max {
            return Err(CoreError::Other(
                "Move, copy, label, forward and command actions need a folder, label, address \
                 or command; other actions take none"
                    .into(),
            ));
        }
        if action.kind == "forward" && !valid_forward_address(&action.value) {
            return Err(CoreError::Other(
                "Forward needs one plain address such as name@example.org".into(),
            ));
        }
        if action.value.contains('\0') {
            return Err(CoreError::Other(
                "An action value cannot contain NUL".into(),
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
            is_read: r.get::<_, i64>("is_read")? != 0,
            is_starred: r.get::<_, i64>("is_starred")? != 0,
            age_days: (crate::models::now_ms() - r.get::<_, i64>("date")?).max(0)
                / (24 * 60 * 60 * 1000),
            headers: None,
            body: None,
        },
    ))
}

const FACT_COLUMNS: &str = "id, thread_id, account_id, from_name, from_addr, to_json, cc_json,
     subject, size, has_attachments, list_unsubscribe, is_read, is_starred, date";

type Pending = (i64, Option<i64>, i64, Facts);
/// A message, its conversation and account, its facts and the actions to run.
type Plan = (i64, Option<i64>, i64, Facts, Vec<FilterAction>);

/// The header block of `raw` as (name, value) pairs, unfolded and decoded.
fn parse_headers(raw: &[u8]) -> Vec<(String, String)> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .into_iter()
        .chain(raw.windows(2).position(|w| w == b"\n\n"))
        .min()
        .unwrap_or(raw.len());
    let head = &raw[..end.min(MAX_HEADER_BYTES)];
    let Some(message) = mail_parser::MessageParser::default().parse_headers(head) else {
        return Vec::new();
    };
    message
        .headers()
        .iter()
        .map(|header| {
            let text = match header.value().as_text() {
                Some(text) => text.to_owned(),
                None => head
                    .get(header.offset_start as usize..header.offset_end as usize)
                    .map(|v| String::from_utf8_lossy(v).into_owned())
                    .unwrap_or_default(),
            };
            let value = text.split_whitespace().collect::<Vec<_>>().join(" ");
            (header.name().to_owned(), value)
        })
        .collect()
}

fn parse_body(raw: &[u8]) -> Option<String> {
    let message =
        mail_parser::MessageParser::default().parse(&raw[..raw.len().min(MAX_PARSED_BYTES)])?;
    let mut text = message.body_text(0)?.into_owned();
    text.truncate(text.floor_char_boundary(MAX_BODY_BYTES));
    Some(text)
}

fn needs(rules: &[FilterRule], field: FilterField) -> bool {
    rules
        .iter()
        .any(|r| r.conditions.iter().any(|c| c.field == field))
}

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

    /// Read the original of every message that a rule with a header or body
    /// condition could still match, so those conditions have text to look at.
    /// Messages that cannot match anyway are never fetched (an IMAP original
    /// costs a server round trip).
    async fn load_message_facts(&self, rules: &[FilterRule], batch: &mut [Pending]) {
        let want_headers = needs(rules, FilterField::Header);
        let want_body = needs(rules, FilterField::Body);
        if !want_headers && !want_body {
            return;
        }
        let mut failed = 0usize;
        for (message_id, _, _, facts) in batch.iter_mut() {
            if !rules.iter().any(|rule| might_match(rule, facts)) {
                continue;
            }
            let id = *message_id;
            if want_body {
                facts.body = self
                    .db
                    .read(move |conn| {
                        Ok(conn
                            .query_row(
                                "SELECT text_body FROM message_bodies WHERE message_id = ?1",
                                params![id],
                                |r| r.get::<_, Option<String>>(0),
                            )
                            .ok()
                            .flatten()
                            .filter(|text| !text.is_empty()))
                    })
                    .await
                    .unwrap_or(None)
                    .map(|mut text| {
                        text.truncate(text.floor_char_boundary(MAX_BODY_BYTES));
                        text
                    });
            }
            if !want_headers && facts.body.is_some() {
                continue;
            }
            match self.original_message(id).await {
                Ok(raw) => {
                    if want_headers {
                        facts.headers = Some(parse_headers(&raw));
                    }
                    if want_body && facts.body.is_none() {
                        facts.body = parse_body(&raw);
                    }
                }
                Err(_) => failed += 1,
            }
        }
        if failed > 0 {
            tracing::warn!(
                "filters: {failed} messages could not be read for header or body conditions"
            );
        }
    }

    async fn run_rules(&self, rules: &[FilterRule], mut batch: Vec<Pending>) -> Result<usize> {
        self.load_message_facts(rules, &mut batch).await;
        // One conversation can hold several matching messages; act once each.
        let mut seen: HashSet<(i64, String, String)> = HashSet::new();
        let mut plans: Vec<Plan> = Vec::new();
        let mut matched = 0;
        for (message, thread, account, facts) in batch {
            let actions = actions_for(rules, &facts);
            if actions.is_empty() {
                continue;
            }
            matched += 1;
            let fresh: Vec<FilterAction> = actions
                .into_iter()
                .filter(|a| {
                    is_message_action(&a.kind)
                        || thread.is_some_and(|t| seen.insert((t, a.kind.clone(), a.value.clone())))
                })
                .collect();
            if !fresh.is_empty() {
                plans.push((message, thread, account, facts, fresh));
            }
        }
        let mut forwards = 0usize;
        for (message_id, thread_id, account_id, facts, actions) in plans {
            for action in actions {
                if is_message_action(&action.kind) {
                    if action.kind == "forward" {
                        forwards += 1;
                        if forwards > MAX_FORWARDS_PER_PASS {
                            tracing::warn!("filter forward skipped: too many forwards in one pass");
                            continue;
                        }
                    }
                    if let Err(error) = self
                        .run_message_filter_action(message_id, account_id, &facts, &action)
                        .await
                    {
                        tracing::warn!(
                            "filter action {} failed for message {message_id}: {error}",
                            action.kind
                        );
                    }
                    continue;
                }
                let Some(thread_id) = thread_id else { continue };
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

    /// Copy, forward and run-command act on the matched message itself.
    async fn run_message_filter_action(
        &self,
        message_id: i64,
        account_id: i64,
        facts: &Facts,
        action: &FilterAction,
    ) -> Result<()> {
        match action.kind.as_str() {
            "run_command" => {
                let outcome = self
                    .run_command_on_message(message_id, &action.value, &facts.subject, &facts.from)
                    .await?;
                if !outcome.success {
                    tracing::warn!(
                        "filter command failed for message {message_id} ({:?}): {}",
                        outcome.code,
                        outcome.output
                    );
                }
                Ok(())
            }
            "forward" => {
                self.forward_original(account_id, message_id, &action.value, &facts.subject)
                    .await
            }
            "copy" => {
                self.copy_original(account_id, message_id, &action.value, facts.is_read)
                    .await
            }
            _ => Ok(()),
        }
    }

    /// Send the original to `to` as an attached message. Sent directly with the
    /// account's own SMTP settings; no Sent copy is kept.
    async fn forward_original(
        &self,
        account_id: i64,
        message_id: i64,
        to: &str,
        subject: &str,
    ) -> Result<()> {
        use crate::accounts::secrets;
        use crate::models::AuthKind;
        let cfg = self
            .db
            .read(move |conn| repo::accounts::get_config(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))?;
        if to.eq_ignore_ascii_case(&cfg.email) {
            return Err(CoreError::Other(
                "a filter will not forward mail to the account it came from".into(),
            ));
        }
        let original = self.original_message(message_id).await?;
        let domain = cfg
            .email
            .split('@')
            .nth(1)
            .unwrap_or("localhost")
            .to_owned();
        let recipients = [Address {
            name: None,
            email: to.to_owned(),
        }];
        let subject = format!("Fwd: {}", subject.replace(['\r', '\n'], " "));
        let out = crate::mime::OutgoingMessage {
            from: Address {
                name: cfg.display_name.clone(),
                email: cfg.email.clone(),
            },
            reply_to: None,
            to: &recipients,
            cc: &[],
            bcc: &[],
            subject: &subject,
            body_text: "Forwarded automatically by a Rustmail filter. The original message is attached.",
            body_html: None,
            in_reply_to: None,
            references: &[],
            message_id: None,
            message_id_domain: &domain,
            attachments: vec![crate::mime::OutgoingAttachment {
                filename: "forwarded-message.eml".into(),
                mime_type: "message/rfc822".into(),
                bytes: original,
            }],
        };
        let (_id, raw) = crate::mime::build_message_owned(out)?;
        let raw = crate::mail_security::Gpg::default()
            .protect(&raw, &cfg.settings.security, &cfg.email, &recipients)
            .await?;
        let auth = match cfg.auth_kind {
            AuthKind::Password => crate::smtp::SmtpAuth::Password(
                secrets::load_smtp_password_async(self.credentials.clone(), cfg.id).await?,
            ),
            AuthKind::Oauth2 => crate::smtp::SmtpAuth::XOAuth2(
                self.tokens.access_token(cfg.id, cfg.provider).await?,
            ),
        };
        let cfg = secrets::hydrate_account(&self.credentials, &cfg).await;
        crate::smtp::send_raw(&cfg, &auth, &cfg.email, &[to.to_owned()], &raw).await
    }

    /// Append a copy of the original to another folder of an IMAP account.
    /// Copies into an Inbox are refused: they would be filtered again.
    async fn copy_original(
        &self,
        account_id: i64,
        message_id: i64,
        folder_name: &str,
        is_read: bool,
    ) -> Result<()> {
        let name = folder_name.to_owned();
        let (config, folder) = self
            .db
            .read(move |conn| {
                let config = repo::accounts::get_config(conn, account_id)?;
                let folder = match find_folder(conn, account_id, &name)? {
                    Some(id) => repo::folders::get(conn, id)?,
                    None => None,
                };
                Ok((config, folder))
            })
            .await?;
        let config = config.ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))?;
        let folder =
            folder.ok_or_else(|| CoreError::NotFound(format!("folder {folder_name:?}")))?;
        if folder.role.as_deref() == Some("inbox") {
            return Err(CoreError::Other(
                "a filter will not copy mail into an Inbox".into(),
            ));
        }
        if config.provider != crate::models::Provider::Imap
            || config.mail_protocol != crate::models::MailProtocol::Imap
            || config.settings.connection.is_local()
        {
            return Err(CoreError::Other("copy works on IMAP accounts only".into()));
        }
        let original = self.original_message(message_id).await?;
        let mut session = self.connect_folder_imap(&config).await?;
        let appended =
            crate::imap::append(&mut session, &folder.imap_name, &original, is_read).await;
        crate::imap::logout(session).await;
        appended
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
            name: String::new(),
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
            is_read: false,
            is_starred: true,
            age_days: 10,
            headers: Some(vec![
                ("X-Spam-Flag".into(), "YES".into()),
                ("Received".into(), "from a.example".into()),
                ("Received".into(), "from b.example".into()),
            ]),
            body: Some("Please pay the attached invoice by Friday.".into()),
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
    fn header_body_age_and_flag_conditions() {
        let f = facts();
        use FilterField::*;
        use FilterOp::*;
        let header = |op, name: &str, value: &str| FilterCondition {
            field: Header,
            op,
            value: value.into(),
            name: name.into(),
        };
        assert!(condition_matches(&header(Equals, "x-spam-flag", "yes"), &f));
        assert!(condition_matches(&header(Contains, "X-Spam-Flag", ""), &f));
        assert!(!condition_matches(&header(Contains, "X-Missing", ""), &f));
        assert!(
            condition_matches(&header(Contains, "received", "b.example"), &f),
            "any of several headers of that name"
        );
        assert!(condition_matches(
            &header(NotContains, "X-Missing", "x"),
            &f
        ));
        assert!(!condition_matches(
            &header(NotContains, "received", "b.example"),
            &f
        ));
        let mut unloaded = facts();
        unloaded.headers = None;
        unloaded.body = None;
        assert!(!condition_matches(
            &header(NotContains, "X-Missing", "x"),
            &unloaded
        ));
        assert!(!condition_matches(
            &cond(Body, Contains, "invoice"),
            &unloaded
        ));
        assert!(condition_matches(&cond(Body, Contains, "INVOICE"), &f));
        assert!(condition_matches(&cond(Age, Greater, "7"), &f));
        assert!(!condition_matches(&cond(Age, Less, "7"), &f));
        assert!(condition_matches(&cond(Read, No, ""), &f));
        assert!(condition_matches(&cond(Starred, Yes, ""), &f));
    }

    #[test]
    fn parses_headers_and_body_from_a_message() {
        let raw = b"Subject: =?utf-8?q?caf=C3=A9?=\r\nX-Long: one\r\n two\r\n\r\nHello body\r\n";
        let headers = parse_headers(raw);
        assert!(headers.contains(&("Subject".into(), "café".into())));
        assert!(headers.contains(&("X-Long".into(), "one two".into())));
        assert_eq!(
            parse_body(raw).as_deref().map(str::trim),
            Some("Hello body")
        );
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
        assert!(check(&|r| {
            r.conditions[0].field = FilterField::Header;
            r.conditions[0].name = "bad name".into();
        }));
        assert!(check(&|r| {
            r.conditions[0].field = FilterField::Age;
            r.conditions[0].op = FilterOp::Contains;
        }));
        assert!(check(&|r| r.actions[0] = FilterAction {
            kind: "forward".into(),
            value: "a@b.c, d@e.f".into()
        }));
        assert!(check(&|r| r.actions[0] = FilterAction {
            kind: "run_command".into(),
            value: String::new()
        }));
        let mut ok = good();
        ok.conditions[0].field = FilterField::Header;
        ok.conditions[0].op = FilterOp::Contains;
        ok.conditions[0].name = "X-Spam-Flag".into();
        ok.conditions[0].value.clear();
        ok.actions = vec![
            FilterAction {
                kind: "forward".into(),
                value: "me@example.org".into(),
            },
            FilterAction {
                kind: "copy".into(),
                value: "Archive".into(),
            },
            FilterAction {
                kind: "run_command".into(),
                value: "cat %f".into(),
            },
        ];
        validate_rule(&mut ok).unwrap();
        assert!(check(&|r| r.conditions = (0..9)
            .map(|_| cond(FilterField::From, FilterOp::Contains, "x"))
            .collect()));
    }
}
