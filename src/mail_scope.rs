//! Typed mail navigation scopes.
//!
//! Slint models, saved tabs and the warm-start cache carry a scope as an
//! opaque string key. Rust parses that key once at the boundary and otherwise
//! works with [`Scope`], so a selection never depends on display names and
//! every consumer handles every kind of mailbox exhaustively.

use rustmail_core::models::Account;
use std::{fmt, str::FromStr};

/// Mailboxes every account presents, in sidebar order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum StandardMailbox {
    Inbox,
    Starred,
    Sent,
    Archive,
    Spam,
    Trash,
    Drafts,
}

impl StandardMailbox {
    pub(crate) const ALL: [Self; 7] = [
        Self::Inbox,
        Self::Starred,
        Self::Sent,
        Self::Archive,
        Self::Spam,
        Self::Trash,
        Self::Drafts,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Starred => "Starred",
            Self::Sent => "Sent",
            Self::Archive => "Archive",
            Self::Spam => "Spam",
            Self::Trash => "Trash",
            Self::Drafts => "Drafts",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mailbox| mailbox.label() == label)
    }
}

/// Where to ask Microsoft 365 when the search box is used in a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerSearchTarget {
    /// The whole mailbox of an account.
    Account(i64),
    /// One folder, by its local id.
    Folder(i64),
    /// An account's folder with this role (`models::roles`).
    Role(i64, &'static str),
}

impl StandardMailbox {
    /// The folder role the mailbox stands for; Starred is no folder.
    pub(crate) const fn role(self) -> Option<&'static str> {
        use rustmail_core::models::roles;
        match self {
            Self::Inbox => Some(roles::INBOX),
            Self::Sent => Some(roles::SENT),
            Self::Archive => Some(roles::ARCHIVE),
            Self::Spam => Some(roles::SPAM),
            Self::Trash => Some(roles::TRASH),
            Self::Drafts => Some(roles::DRAFTS),
            Self::Starred => None,
        }
    }
}

/// What a search typed in `scope` should ask the server, given the profile's
/// Microsoft 365 accounts. A single account's mailbox asks only that account
/// and only its folder; a folder asks that folder (the core ignores folders
/// of other account types); views that span accounts ask every account.
pub(crate) fn server_search_targets(
    scope: Scope,
    exchange_accounts: &[i64],
) -> Vec<ServerSearchTarget> {
    match scope {
        Scope::Account { account_id, mailbox } if exchange_accounts.contains(&account_id) => {
            vec![match mailbox.role() {
                Some(role) => ServerSearchTarget::Role(account_id, role),
                None => ServerSearchTarget::Account(account_id),
            }]
        }
        Scope::Account { .. } => Vec::new(),
        Scope::Folder(id) => vec![ServerSearchTarget::Folder(id)],
        Scope::Unified(mailbox) => exchange_accounts
            .iter()
            .map(|&account_id| match mailbox.role() {
                Some(role) => ServerSearchTarget::Role(account_id, role),
                None => ServerSearchTarget::Account(account_id),
            })
            .collect(),
        Scope::Important | Scope::Other | Scope::Label { .. } | Scope::Tasks(_) => {
            exchange_accounts
                .iter()
                .map(|&account_id| ServerSearchTarget::Account(account_id))
                .collect()
        }
    }
}

/// The folders besides the Inbox that the short poll of Microsoft 365 mail
/// must read while `scope` is open. Accounts' whole-mailbox views (labels,
/// tasks, split inboxes, Starred) show mail the Inbox poll already reads.
#[cfg(test)]
pub(crate) fn watched_folders(
    scope: Scope,
    exchange_accounts: &[i64],
) -> Vec<rustmail_core::exchange::WatchedFolder> {
    watched_folders_of_scopes(&[scope], exchange_accounts)
}

/// The same for several open views at once: the shown one and the mail views
/// kept by inactive tabs. Duplicates are dropped, so two tabs on one folder
/// cost one read.
pub(crate) fn watched_folders_of_scopes(
    scopes: &[Scope],
    exchange_accounts: &[i64],
) -> Vec<rustmail_core::exchange::WatchedFolder> {
    use rustmail_core::exchange::WatchedFolder;
    let mut folders: Vec<WatchedFolder> = Vec::new();
    for &scope in scopes {
        for target in server_search_targets(scope, exchange_accounts) {
            let folder = match target {
                ServerSearchTarget::Account(_) => continue,
                ServerSearchTarget::Folder(id) => WatchedFolder::Folder(id),
                ServerSearchTarget::Role(account, role) => {
                    WatchedFolder::Role(account, role.to_owned())
                }
            };
            if !folders.contains(&folder) {
                folders.push(folder);
            }
        }
    }
    folders
}

static INACTIVE_TAB_SCOPES: std::sync::Mutex<Vec<Scope>> = std::sync::Mutex::new(Vec::new());

/// Remember the mail views of inactive tabs, so their folders stay fresh too.
pub(crate) fn set_inactive_tab_scopes(scopes: Vec<Scope>) {
    if let Ok(mut current) = INACTIVE_TAB_SCOPES.lock() {
        *current = scopes;
    }
}

pub(crate) fn inactive_tab_scopes() -> Vec<Scope> {
    INACTIVE_TAB_SCOPES
        .lock()
        .map(|scopes| scopes.clone())
        .unwrap_or_default()
}

/// Which sidebar section a label is listed in. The id alone selects the
/// label; the kind keeps the key identical to the row that was clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LabelKind {
    /// Automatic categories shared by every account.
    Category,
    /// Manual labels shared by every account.
    Global,
    /// Labels owned by one account.
    Account,
}

impl LabelKind {
    const fn prefix(self) -> &'static str {
        match self {
            Self::Category => "Category",
            Self::Global => "GlobalLabel",
            Self::Account => "AccountLabel",
        }
    }

    /// The section a label with these properties belongs to.
    pub(crate) const fn of(is_auto: bool, owner_account_id: Option<i64>) -> Self {
        if is_auto {
            Self::Category
        } else if owner_account_id.is_some() {
            Self::Account
        } else {
            Self::Global
        }
    }
}

/// Virtual folders over the task links (the Mail sidebar's "With tasks" and
/// "Waiting on reply"). They span every account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TaskFolder {
    /// Conversations linked to an open task.
    WithTasks,
    /// Conversations linked to an open Waiting-for task that nobody has
    /// answered since the link was made.
    WaitingOnReply,
}

impl TaskFolder {
    pub(crate) const ALL: [Self; 2] = [Self::WithTasks, Self::WaitingOnReply];

    const fn token(self) -> &'static str {
        match self {
            Self::WithTasks => "with",
            Self::WaitingOnReply => "waiting",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::WithTasks => "With tasks",
            Self::WaitingOnReply => "Waiting on reply",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|folder| folder.token() == token)
    }
}

/// A selectable mail view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    /// One standard mailbox across every account.
    Unified(StandardMailbox),
    /// Gmail-style split inbox, primary half.
    Important,
    /// Gmail-style split inbox, remaining half.
    Other,
    Label { kind: LabelKind, id: i64 },
    /// A virtual folder over the task links.
    Tasks(TaskFolder),
    /// A provider folder outside the standard mailboxes.
    Folder(i64),
    /// One standard mailbox of one account, whether or not the provider
    /// reported a matching role folder.
    Account {
        account_id: i64,
        mailbox: StandardMailbox,
    },
}

impl Default for Scope {
    fn default() -> Self {
        Self::UNIFIED_INBOX
    }
}

impl Scope {
    pub(crate) const UNIFIED_INBOX: Self = Self::Unified(StandardMailbox::Inbox);

    pub(crate) const fn label_id(self) -> Option<i64> {
        match self {
            Self::Label { id, .. } => Some(id),
            _ => None,
        }
    }

    /// The scope the sidebar can show for these accounts. Unified shortcuts
    /// are absent with exactly one account, so they resolve to that account's
    /// own mailbox; the message list is identical. Other scopes and the
    /// zero/multiple-account cases keep their meaning.
    #[must_use]
    pub(crate) fn visible_for(self, accounts: &[Account]) -> Self {
        match (self, accounts) {
            (Self::Unified(mailbox), [account]) => Self::Account {
                account_id: account.id,
                mailbox,
            },
            _ => self,
        }
    }
}

const UNIFIED_PREFIX: &str = "Unified ";
const FOLDER_PREFIX: &str = "Folder:";
const ACCOUNT_PREFIX: &str = "Account:";
const TASKS_PREFIX: &str = "TaskFolder:";

impl fmt::Display for Scope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unified(mailbox) => write!(formatter, "{UNIFIED_PREFIX}{}", mailbox.label()),
            Self::Important => formatter.write_str("Important"),
            Self::Other => formatter.write_str("Other"),
            Self::Label { kind, id } => write!(formatter, "{}:{id}", kind.prefix()),
            Self::Folder(id) => write!(formatter, "{FOLDER_PREFIX}{id}"),
            Self::Tasks(folder) => write!(formatter, "{TASKS_PREFIX}{}", folder.token()),
            Self::Account {
                account_id,
                mailbox,
            } => write!(formatter, "{ACCOUNT_PREFIX}{account_id}:{}", mailbox.label()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParseScopeError(String);

impl fmt::Display for ParseScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown mail scope {:?}", self.0)
    }
}

impl std::error::Error for ParseScopeError {}

impl FromStr for Scope {
    type Err = ParseScopeError;

    fn from_str(key: &str) -> Result<Self, Self::Err> {
        let error = || ParseScopeError(key.to_owned());
        let id = |value: &str| value.parse::<i64>().map_err(|_| error());
        if let Some(label) = key.strip_prefix(UNIFIED_PREFIX) {
            return StandardMailbox::from_label(label)
                .map(Self::Unified)
                .ok_or_else(error);
        }
        if let Some(token) = key.strip_prefix(TASKS_PREFIX) {
            return TaskFolder::from_token(token)
                .map(Self::Tasks)
                .ok_or_else(error);
        }
        if let Some(folder_id) = key.strip_prefix(FOLDER_PREFIX) {
            return id(folder_id).map(Self::Folder);
        }
        if let Some(rest) = key.strip_prefix(ACCOUNT_PREFIX) {
            let (account_id, label) = rest.split_once(':').ok_or_else(error)?;
            return Ok(Self::Account {
                account_id: id(account_id)?,
                mailbox: StandardMailbox::from_label(label).ok_or_else(error)?,
            });
        }
        if let Some((prefix, label_id)) = key.split_once(':')
            && let Some(kind) = [LabelKind::Category, LabelKind::Global, LabelKind::Account]
                .into_iter()
                .find(|kind| kind.prefix() == prefix)
        {
            return Ok(Self::Label {
                kind,
                id: id(label_id)?,
            });
        }
        match key {
            "Important" => Ok(Self::Important),
            "Other" => Ok(Self::Other),
            _ => Err(error()),
        }
    }
}

impl From<Scope> for String {
    fn from(scope: Scope) -> Self {
        scope.to_string()
    }
}

impl From<Scope> for slint::SharedString {
    fn from(scope: Scope) -> Self {
        scope.to_string().into()
    }
}

impl TryFrom<String> for Scope {
    type Error = ParseScopeError;

    fn try_from(key: String) -> Result<Self, Self::Error> {
        key.parse()
    }
}

impl serde::Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = String::deserialize(deserializer)?;
        key.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustmail_core::models::{AuthKind, MailProtocol, Provider};

    #[test]
    fn a_search_asks_only_the_server_places_the_scope_shows() {
        use ServerSearchTarget::*;
        let exchange = [7, 9];
        assert_eq!(
            server_search_targets(Scope::Unified(StandardMailbox::Inbox), &exchange),
            vec![Role(7, "inbox"), Role(9, "inbox")]
        );
        assert_eq!(
            server_search_targets(
                Scope::Account {
                    account_id: 9,
                    mailbox: StandardMailbox::Sent
                },
                &exchange
            ),
            vec![Role(9, "sent")]
        );
        // Another kind of account has nothing to ask.
        assert!(
            server_search_targets(
                Scope::Account {
                    account_id: 3,
                    mailbox: StandardMailbox::Inbox
                },
                &exchange
            )
            .is_empty()
        );
        assert_eq!(
            server_search_targets(Scope::Folder(41), &exchange),
            vec![Folder(41)]
        );
        assert_eq!(
            server_search_targets(Scope::Unified(StandardMailbox::Starred), &exchange),
            vec![Account(7), Account(9)]
        );
        assert!(server_search_targets(Scope::Important, &[]).is_empty());
    }

    #[test]
    fn the_short_poll_reads_the_folder_every_kind_of_scope_shows() {
        use rustmail_core::exchange::WatchedFolder::{Folder, Role};
        let exchange = [7, 9];
        assert_eq!(
            watched_folders(Scope::Unified(StandardMailbox::Sent), &exchange),
            vec![Role(7, "sent".into()), Role(9, "sent".into())]
        );
        assert_eq!(
            watched_folders(
                Scope::Account {
                    account_id: 9,
                    mailbox: StandardMailbox::Archive
                },
                &exchange
            ),
            vec![Role(9, "archive".into())]
        );
        assert_eq!(watched_folders(Scope::Folder(41), &exchange), vec![Folder(41)]);
        // Whole-mailbox views show mail the Inbox poll reads already.
        assert!(watched_folders(Scope::Unified(StandardMailbox::Starred), &exchange).is_empty());
        assert!(watched_folders(Scope::Important, &exchange).is_empty());
        assert!(watched_folders(Scope::Folder(41), &[]).len() == 1);
    }

    fn account(id: i64) -> Account {
        Account {
            id,
            email: format!("{id}@example.test"),
            display_name: Some("Work / Unified Folder:1".into()),
            avatar_url: None,
            provider: Provider::Imap,
            auth_kind: AuthKind::Password,
            mail_protocol: MailProtocol::Imap,
            sync_state: "idle".into(),
            sync_error: None,
            can_create_top_level_mailbox: true,
        }
    }

    fn every_scope() -> Vec<Scope> {
        let mut scopes = vec![Scope::Important, Scope::Other, Scope::Folder(-3)];
        scopes.extend(TaskFolder::ALL.map(Scope::Tasks));
        for mailbox in StandardMailbox::ALL {
            scopes.push(Scope::Unified(mailbox));
            scopes.push(Scope::Account {
                account_id: 42,
                mailbox,
            });
        }
        for kind in [LabelKind::Category, LabelKind::Global, LabelKind::Account] {
            scopes.push(Scope::Label { kind, id: 7 });
        }
        scopes
    }

    #[test]
    fn keys_round_trip_and_stay_compatible_with_the_slint_sidebar() {
        for scope in every_scope() {
            assert_eq!(scope.to_string().parse::<Scope>(), Ok(scope));
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(serde_json::from_str::<Scope>(&json).unwrap(), scope);
        }
        // ui/components/sidebar.slint builds and compares these keys itself.
        assert_eq!(Scope::UNIFIED_INBOX.to_string(), "Unified Inbox");
        assert_eq!(
            Scope::Unified(StandardMailbox::Trash).to_string(),
            "Unified Trash"
        );
        assert_eq!(
            Scope::Label {
                kind: LabelKind::Category,
                id: 3
            }
            .to_string(),
            "Category:3"
        );
        assert_eq!(
            Scope::Label {
                kind: LabelKind::Global,
                id: 3
            }
            .to_string(),
            "GlobalLabel:3"
        );
        assert_eq!(
            Scope::Label {
                kind: LabelKind::Account,
                id: 3
            }
            .to_string(),
            "AccountLabel:3"
        );
    }

    #[test]
    fn malformed_and_display_name_keys_are_rejected() {
        for key in [
            "",
            "Unified",
            "Unified Unknown",
            "Folder:",
            "Folder:x",
            "Account:1",
            "Account:x:Inbox",
            "Account:1:Custom",
            "Label:4",
            "Category:",
            "Person / Inbox",
            "Person",
            "inbox",
        ] {
            assert!(key.parse::<Scope>().is_err(), "{key:?}");
        }
        assert!(serde_json::from_str::<Scope>("\"Person / Inbox\"").is_err());
    }

    #[test]
    fn unified_scopes_resolve_to_the_only_account() {
        for scope in every_scope() {
            let expected = match scope {
                Scope::Unified(mailbox) => Scope::Account {
                    account_id: 9,
                    mailbox,
                },
                other => other,
            };
            assert_eq!(scope.visible_for(&[account(9)]), expected);
            assert_eq!(scope.visible_for(&[]), scope);
            assert_eq!(scope.visible_for(&[account(9), account(10)]), scope);
            assert_eq!(expected.visible_for(&[account(9)]), expected);
        }
    }
}
