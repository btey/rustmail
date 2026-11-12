//! Microsoft Graph mail client for Exchange Online accounts.
//!
//! Every request carries a bearer token from an [`AccessTokenSource`] (never
//! logged), is bounded in time and size, refuses to follow links that leave
//! the configured Graph host, retries throttling (`429`/`503`/`504` with
//! `Retry-After`) a few times, and refreshes the token once on a `401`.

use super::auth::ExchangeTokens;
use super::settings::ExchangeSettings;
use crate::error::{CoreError, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
/// Largest message downloaded or sent through Graph.
pub const MAX_MIME_BYTES: usize = 35 * 1024 * 1024;
const MAX_NEXT_LINK_BYTES: usize = 16 * 1024;
const MAX_ATTEMPTS: usize = 4;
const MAX_PAGES: usize = 2_000;
const MAX_FOLDERS: usize = 5_000;
const MAX_FOLDER_DEPTH: usize = 24;
const MAX_LISTED_IDS: usize = 200_000;
const BATCH_SIZE: usize = 20;
/// Messages up to this size go to Graph as one MIME request; larger ones use
/// a JSON draft with upload sessions for their attachments.
pub const MAX_SIMPLE_MIME_BYTES: usize = 3 * 1024 * 1024;
/// An attachment up to this size is added in one request; larger ones use an
/// upload session.
pub const MAX_INLINE_ATTACHMENT_BYTES: u64 = 3 * 1024 * 1024;
/// Upload session chunk: a multiple of 320 KiB, as Graph requires.
const UPLOAD_CHUNK: usize = 12 * 320 * 1024;
/// Message properties the sync reads from `delta`.
const MESSAGE_SELECT: &str = "id,internetMessageId,subject,isRead,isDraft,flag,categories,receivedDateTime,hasAttachments,parentFolderId";

#[async_trait]
pub trait AccessTokenSource: Send + Sync {
    async fn access_token(&self) -> Result<String>;
    /// Drop a cached token after the service rejected it.
    async fn invalidate(&self);
}

#[async_trait]
impl AccessTokenSource for ExchangeTokens {
    async fn access_token(&self) -> Result<String> {
        ExchangeTokens::access_token(self).await
    }
    async fn invalidate(&self) {
        ExchangeTokens::invalidate(self).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFolder {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphMessage {
    pub id: String,
    pub internet_message_id: Option<String>,
    pub subject: String,
    pub is_read: bool,
    pub is_draft: bool,
    pub flagged: bool,
    pub categories: Vec<String>,
    pub received_ms: Option<i64>,
    pub has_attachments: bool,
    pub parent_folder_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaItem {
    Upsert(GraphMessage),
    Removed(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeltaPage {
    pub items: Vec<DeltaItem>,
    pub next_link: Option<String>,
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeltaResult {
    Page(DeltaPage),
    /// `410 Gone`: the saved delta link is too old; start again.
    Expired,
}

#[derive(Debug, Clone)]
pub struct BatchRequest {
    pub method: &'static str,
    /// Relative to the Graph version root, e.g. `/me/messages/{id}`.
    pub url: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchResponse {
    pub index: usize,
    pub status: u16,
    pub body: Value,
}

struct Raw {
    status: u16,
    body: Vec<u8>,
}

pub struct GraphMail {
    http: reqwest::Client,
    base: url::Url,
    mailbox: Vec<String>,
    tokens: Arc<dyn AccessTokenSource>,
    retry_cap: Duration,
}

fn http_client(proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("Rustmail/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(proxy) = proxy {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|error| CoreError::Network(format!("proxy setting: {error}")))?,
        );
    }
    builder
        .build()
        .map_err(|error| CoreError::Network(format!("Graph HTTP client: {error}")))
}

fn graph_error_text(body: &[u8]) -> String {
    let value: Value = serde_json::from_slice(body).unwrap_or_default();
    let code = value
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = match (code.is_empty(), message.is_empty()) {
        (true, true) => String::new(),
        (false, true) => code.to_owned(),
        (true, false) => message.to_owned(),
        (false, false) => format!("{code}: {message}"),
    };
    crate::http_body::single_line_excerpt(&text, 300)
}

fn status_error(status: u16, body: &[u8]) -> CoreError {
    let detail = graph_error_text(body);
    match status {
        401 => CoreError::NeedsReauth,
        403 => CoreError::Other(format!(
            "Microsoft 365 denied this request{}{}. Check that the app has the needed permissions and that your organization allows it.",
            if detail.is_empty() { "" } else { " (" },
            if detail.is_empty() {
                String::new()
            } else {
                format!("{detail})")
            }
        )),
        404 => CoreError::NotFound(if detail.is_empty() {
            "Microsoft 365 item".into()
        } else {
            detail
        }),
        _ => CoreError::Network(format!(
            "Microsoft Graph returned {status}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )),
    }
}

impl GraphMail {
    pub fn new(settings: &ExchangeSettings, tokens: Arc<dyn AccessTokenSource>) -> Result<Self> {
        settings.validate_sign_in_fields()?;
        let base = url::Url::parse(&settings.graph_base()?)
            .map_err(|_| CoreError::Other("The Graph URL is not valid.".into()))?;
        let mailbox = match settings.shared_mailbox() {
            Some(address) => vec!["users".to_owned(), address.to_owned()],
            None => vec!["me".to_owned()],
        };
        Ok(Self {
            http: http_client(settings.proxy())?,
            base,
            mailbox,
            tokens,
            retry_cap: Duration::from_secs(60),
        })
    }

    /// Cap the wait between throttling retries (tests use a tiny value).
    pub fn with_retry_cap(mut self, cap: Duration) -> Self {
        self.retry_cap = cap;
        self
    }

    fn endpoint(&self, tail: &[&str]) -> url::Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty();
            for segment in &self.mailbox {
                path.push(segment);
            }
            for segment in tail {
                path.push(segment);
            }
        }
        url
    }

    /// Accept only absolute links on the Graph host we were configured with.
    fn trusted_link(&self, link: &str) -> Result<url::Url> {
        if link.len() > MAX_NEXT_LINK_BYTES {
            return Err(CoreError::Network(
                "Microsoft Graph sent an oversized link".into(),
            ));
        }
        let url = url::Url::parse(link)
            .map_err(|_| CoreError::Network("Microsoft Graph sent an invalid link".into()))?;
        let same_origin = url.scheme() == self.base.scheme()
            && url.host_str() == self.base.host_str()
            && url.port_or_known_default() == self.base.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url
                .path()
                .starts_with(self.base.path().trim_end_matches('/'));
        if !same_origin {
            return Err(CoreError::Network(
                "refusing a link that leaves the Microsoft Graph host".into(),
            ));
        }
        Ok(url)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        url: url::Url,
        body: Option<(&'static str, Vec<u8>)>,
        headers: &[(&str, &str)],
        max_bytes: usize,
        uncertain_on_transport: bool,
    ) -> Result<Raw> {
        let mut refreshed = false;
        for attempt in 0..MAX_ATTEMPTS {
            let token = self.tokens.access_token().await?;
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(&token);
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            if let Some((content_type, bytes)) = &body {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, *content_type)
                    .body(bytes.clone());
            }
            let response = request.send().await.map_err(|error| {
                let error = error.without_url();
                if error.is_connect() {
                    // Nothing was sent: always safe to try again.
                    CoreError::Offline
                } else if uncertain_on_transport {
                    // The request may have been processed (a non-idempotent
                    // send): never repeat it automatically.
                    CoreError::SendUncertain(format!(
                        "the connection to Microsoft 365 failed while sending ({error}); check Sent Items before sending again"
                    ))
                } else if error.is_timeout() {
                    CoreError::Offline
                } else {
                    CoreError::Network(format!("Microsoft Graph request failed: {error}"))
                }
            })?;
            let status = response.status().as_u16();
            if status == 401 && !refreshed {
                refreshed = true;
                self.tokens.invalidate().await;
                continue;
            }
            if matches!(status, 429 | 503 | 504) {
                let wait = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .map_or_else(
                        || Duration::from_secs(1 << attempt.min(5)),
                        Duration::from_secs,
                    )
                    .min(self.retry_cap);
                if attempt + 1 < MAX_ATTEMPTS {
                    tokio::time::sleep(wait).await;
                    continue;
                }
                return Err(CoreError::Network(
                    "Microsoft 365 is throttling requests; Rustmail will try again later".into(),
                ));
            }
            let bytes =
                crate::http_body::bytes(response, max_bytes, "Microsoft Graph response").await?;
            return Ok(Raw {
                status,
                body: bytes,
            });
        }
        Err(CoreError::NeedsReauth)
    }

    async fn json(
        &self,
        method: reqwest::Method,
        url: url::Url,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> Result<Value> {
        let payload = body.map(|value| ("application/json", value.to_string().into_bytes()));
        let raw = self
            .send(method, url, payload, headers, MAX_JSON_BYTES, false)
            .await?;
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        if raw.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })
    }

    /// Verify the sign-in and the mail permission; returns the Inbox id.
    pub async fn probe(&self) -> Result<String> {
        let mut url = self.endpoint(&["mailFolders", "inbox"]);
        url.query_pairs_mut().append_pair("$select", "id");
        let raw = self
            .send(reqwest::Method::GET, url, None, &[], MAX_JSON_BYTES, false)
            .await?;
        if raw.status == 401 {
            // A fresh token that Graph refuses is a sign-in problem the user
            // can act on, so say what Graph said instead of "re-authenticate".
            let detail = graph_error_text(&raw.body);
            return Err(CoreError::Auth(format!(
                "Microsoft Graph did not accept the sign-in token{}. The app may not be allowed to use Microsoft Graph for mail: try the Scopes sign-in style with your own client ID.",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({detail})")
                }
            )));
        }
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })?;
        value["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CoreError::Network("Microsoft Graph returned no Inbox".into()))
    }

    /// The id of a well-known folder (`inbox`, `sentitems`, `drafts`,
    /// `deleteditems`, `junkemail`, `archive`), or `None` when the mailbox has
    /// none.
    pub async fn well_known_folder(&self, name: &str) -> Result<Option<String>> {
        let mut url = self.endpoint(&["mailFolders", name]);
        url.query_pairs_mut().append_pair("$select", "id");
        match self.json(reqwest::Method::GET, url, None, &[]).await {
            Ok(value) => Ok(value["id"].as_str().map(str::to_owned)),
            Err(CoreError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Every mail folder, parents before children.
    pub async fn folders(&self) -> Result<Vec<GraphFolder>> {
        let mut out: Vec<GraphFolder> = Vec::new();
        let mut level: Vec<Option<String>> = vec![None];
        for _depth in 0..MAX_FOLDER_DEPTH {
            let mut next_level = Vec::new();
            for parent in &level {
                let mut url = match parent {
                    None => self.endpoint(&["mailFolders"]),
                    Some(id) => self.endpoint(&["mailFolders", id, "childFolders"]),
                };
                url.query_pairs_mut()
                    .append_pair("$top", "100")
                    .append_pair("$select", "id,displayName,parentFolderId,childFolderCount");
                let mut link = Some(url);
                let mut pages = 0;
                while let Some(url) = link.take() {
                    pages += 1;
                    if pages > MAX_PAGES {
                        return Err(CoreError::Network("too many folder pages".into()));
                    }
                    let value = self.json(reqwest::Method::GET, url, None, &[]).await?;
                    for item in value["value"].as_array().into_iter().flatten() {
                        let (Some(id), Some(name)) =
                            (item["id"].as_str(), item["displayName"].as_str())
                        else {
                            continue;
                        };
                        if out.len() >= MAX_FOLDERS {
                            return Err(CoreError::Network(
                                "the mailbox has too many folders".into(),
                            ));
                        }
                        if item["childFolderCount"].as_u64().unwrap_or(0) > 0 {
                            next_level.push(Some(id.to_owned()));
                        }
                        out.push(GraphFolder {
                            id: id.to_owned(),
                            name: name.to_owned(),
                            parent_id: parent.clone(),
                        });
                    }
                    if let Some(next) = value["@odata.nextLink"].as_str() {
                        link = Some(self.trusted_link(next)?);
                    }
                }
            }
            if next_level.is_empty() {
                return Ok(out);
            }
            level = next_level;
        }
        Err(CoreError::Network(
            "the folder tree is nested too deeply".into(),
        ))
    }

    /// One page of changes in a folder. Pass `link` (a saved `deltaLink` or the
    /// previous page's `nextLink`) to continue; `received_after_ms` limits a
    /// first sync to recent mail.
    pub async fn delta(
        &self,
        folder_id: &str,
        link: Option<&str>,
        received_after_ms: Option<i64>,
    ) -> Result<DeltaResult> {
        let url = match link {
            Some(link) => self.trusted_link(link)?,
            None => {
                let mut url = self.endpoint(&["mailFolders", folder_id, "messages", "delta"]);
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("$select", MESSAGE_SELECT);
                    if let Some(ms) = received_after_ms
                        && let Some(date) =
                            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                    {
                        query.append_pair(
                            "$filter",
                            &format!("receivedDateTime ge {}", date.format("%Y-%m-%dT%H:%M:%SZ")),
                        );
                    }
                }
                url
            }
        };
        let raw = self
            .send(
                reqwest::Method::GET,
                url,
                None,
                &[("Prefer", "odata.maxpagesize=100, IdType=\"ImmutableId\"")],
                MAX_JSON_BYTES,
                false,
            )
            .await?;
        if raw.status == 410 {
            return Ok(DeltaResult::Expired);
        }
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })?;
        let mut page = DeltaPage::default();
        for item in value["value"].as_array().into_iter().flatten() {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            if item.get("@removed").is_some() {
                page.items.push(DeltaItem::Removed(id.to_owned()));
                continue;
            }
            page.items.push(DeltaItem::Upsert(parse_message(item, id)));
        }
        page.next_link = value["@odata.nextLink"].as_str().map(str::to_owned);
        page.delta_link = value["@odata.deltaLink"].as_str().map(str::to_owned);
        if let Some(link) = page.next_link.as_deref().or(page.delta_link.as_deref()) {
            // Fail early on a link we would later refuse to follow.
            self.trusted_link(link)?;
        }
        Ok(DeltaResult::Page(page))
    }

    /// The folder a message sits in now, or `None` when it no longer exists.
    /// A removal in a delta can be a move, which this tells apart from a delete.
    pub async fn message_parent(&self, id: &str) -> Result<Option<String>> {
        let mut url = self.endpoint(&["messages", id]);
        url.query_pairs_mut()
            .append_pair("$select", "parentFolderId");
        match self
            .json(
                reqwest::Method::GET,
                url,
                None,
                &[("Prefer", "IdType=\"ImmutableId\"")],
            )
            .await
        {
            Ok(value) => Ok(value["parentFolderId"].as_str().map(str::to_owned)),
            Err(CoreError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The whole message as RFC 5322 bytes.
    pub async fn message_mime(&self, id: &str) -> Result<Vec<u8>> {
        let url = self.endpoint(&["messages", id, "$value"]);
        let raw = self
            .send(
                reqwest::Method::GET,
                url,
                None,
                &[("Prefer", "IdType=\"ImmutableId\"")],
                MAX_MIME_BYTES,
                false,
            )
            .await?;
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        Ok(raw.body)
    }

    pub async fn update_message(&self, id: &str, patch: &Value) -> Result<()> {
        let url = self.endpoint(&["messages", id]);
        self.json(reqwest::Method::PATCH, url, Some(patch), &[])
            .await
            .map(|_| ())
    }

    pub async fn set_read(&self, id: &str, read: bool) -> Result<()> {
        self.update_message(id, &json!({ "isRead": read })).await
    }

    pub async fn set_flagged(&self, id: &str, flagged: bool) -> Result<()> {
        let status = if flagged { "flagged" } else { "notFlagged" };
        self.update_message(id, &json!({ "flag": { "flagStatus": status } }))
            .await
    }

    /// The categories of a message as the server has them now.
    pub async fn categories(&self, id: &str) -> Result<Vec<String>> {
        let mut url = self.endpoint(&["messages", id]);
        url.query_pairs_mut().append_pair("$select", "categories");
        let value = self.json(reqwest::Method::GET, url, None, &[]).await?;
        Ok(value["categories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|category| category.as_str().map(str::to_owned))
            .collect())
    }

    pub async fn set_categories(&self, id: &str, categories: &[String]) -> Result<()> {
        self.update_message(id, &json!({ "categories": categories }))
            .await
    }

    /// Move a message; Graph gives the moved copy a new id.
    pub async fn move_message(&self, id: &str, destination_id: &str) -> Result<String> {
        let url = self.endpoint(&["messages", id, "move"]);
        let value = self
            .json(
                reqwest::Method::POST,
                url,
                Some(&json!({ "destinationId": destination_id })),
                &[],
            )
            .await?;
        value["id"].as_str().map(str::to_owned).ok_or_else(|| {
            CoreError::Network("Microsoft Graph returned no id for the moved message".into())
        })
    }

    /// Delete to Deleted Items (a missing message counts as deleted).
    pub async fn delete_message(&self, id: &str) -> Result<()> {
        let url = self.endpoint(&["messages", id]);
        self.expect_gone(reqwest::Method::DELETE, url, None).await
    }

    /// Delete without the Deleted Items copy.
    pub async fn permanent_delete(&self, id: &str) -> Result<()> {
        let url = self.endpoint(&["messages", id, "permanentDelete"]);
        self.expect_gone(reqwest::Method::POST, url, Some(json!({})))
            .await
    }

    async fn expect_gone(
        &self,
        method: reqwest::Method,
        url: url::Url,
        body: Option<Value>,
    ) -> Result<()> {
        let payload = body.map(|value| ("application/json", value.to_string().into_bytes()));
        let raw = self
            .send(method, url, payload, &[], MAX_JSON_BYTES, false)
            .await?;
        // Graph answers `403 ErrorCannotDeleteObject` for a message that is
        // already gone when immutable ids are in use.
        let already_gone = raw.status == 403
            && String::from_utf8_lossy(&raw.body).contains("ErrorCannotDeleteObject");
        if (200..300).contains(&raw.status) || raw.status == 404 || already_gone {
            return Ok(());
        }
        Err(status_error(raw.status, &raw.body))
    }

    /// Send a complete RFC 5322 message. Graph keeps the Sent Items copy, so
    /// the caller must not append one itself.
    pub async fn send_mime(&self, raw_message: &[u8]) -> Result<()> {
        if raw_message.len() > MAX_MIME_BYTES {
            return Err(CoreError::Other(format!(
                "the message is larger than the {} MiB limit for sending through Microsoft 365",
                MAX_MIME_BYTES / (1024 * 1024)
            )));
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw_message);
        let url = self.endpoint(&["sendMail"]);
        let raw = self
            .send(
                reqwest::Method::POST,
                url,
                Some(("text/plain", encoded.into_bytes())),
                &[],
                MAX_JSON_BYTES,
                true,
            )
            .await?;
        if raw.status == 202 || raw.status == 200 || raw.status == 204 {
            return Ok(());
        }
        Err(status_error(raw.status, &raw.body))
    }

    /// Every message id in a folder (used to empty Deleted Items).
    pub async fn list_message_ids(&self, folder_id: &str) -> Result<Vec<String>> {
        let mut url = self.endpoint(&["mailFolders", folder_id, "messages"]);
        url.query_pairs_mut()
            .append_pair("$select", "id")
            .append_pair("$top", "100");
        let mut link = Some(url);
        let mut ids = Vec::new();
        let mut pages = 0;
        while let Some(url) = link.take() {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(CoreError::Network("too many message pages".into()));
            }
            let value = self.json(reqwest::Method::GET, url, None, &[]).await?;
            for item in value["value"].as_array().into_iter().flatten() {
                if let Some(id) = item["id"].as_str() {
                    ids.push(id.to_owned());
                }
            }
            if ids.len() > MAX_LISTED_IDS {
                return Err(CoreError::Network(
                    "the folder has too many messages to list".into(),
                ));
            }
            if let Some(next) = value["@odata.nextLink"].as_str() {
                link = Some(self.trusted_link(next)?);
            }
        }
        Ok(ids)
    }

    /// `$batch`: up to 20 requests per call. A throttled sub-request is
    /// retried once after its own `Retry-After`.
    pub async fn batch(&self, requests: Vec<BatchRequest>) -> Result<Vec<BatchResponse>> {
        let mut out = Vec::with_capacity(requests.len());
        let mut base_index = 0;
        for chunk in requests.chunks(BATCH_SIZE) {
            let mut pending: Vec<usize> = (0..chunk.len()).collect();
            let mut results: Vec<Option<BatchResponse>> = vec![None; chunk.len()];
            for round in 0..2 {
                if pending.is_empty() {
                    break;
                }
                let entries: Vec<Value> = pending
                    .iter()
                    .map(|&position| {
                        let request = &chunk[position];
                        let mut entry = json!({
                            "id": position.to_string(),
                            "method": request.method,
                            "url": request.url,
                        });
                        if let Some(body) = &request.body {
                            entry["body"] = body.clone();
                            entry["headers"] = json!({ "Content-Type": "application/json" });
                        }
                        entry
                    })
                    .collect();
                let mut url = self.base.clone();
                if let Ok(mut path) = url.path_segments_mut() {
                    path.pop_if_empty();
                    path.push("$batch");
                }
                let reply = self
                    .json(
                        reqwest::Method::POST,
                        url,
                        Some(&json!({ "requests": entries })),
                        &[],
                    )
                    .await?;
                let mut retry = Vec::new();
                let mut wait = Duration::ZERO;
                for response in reply["responses"].as_array().into_iter().flatten() {
                    let Some(position) = response["id"]
                        .as_str()
                        .and_then(|id| id.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    if position >= chunk.len() {
                        continue;
                    }
                    let status = response["status"].as_u64().unwrap_or(0) as u16;
                    if status == 429 && round == 0 {
                        let seconds = response
                            .pointer("/headers/Retry-After")
                            .and_then(Value::as_str)
                            .and_then(|value| value.parse::<u64>().ok())
                            .unwrap_or(1);
                        wait = wait.max(Duration::from_secs(seconds).min(self.retry_cap));
                        retry.push(position);
                        continue;
                    }
                    results[position] = Some(BatchResponse {
                        index: base_index + position,
                        status,
                        body: response["body"].clone(),
                    });
                }
                if retry.is_empty() {
                    break;
                }
                tokio::time::sleep(wait).await;
                pending = retry;
            }
            for (position, result) in results.into_iter().enumerate() {
                out.push(result.unwrap_or(BatchResponse {
                    index: base_index + position,
                    status: 0,
                    body: Value::Null,
                }));
            }
            base_index += chunk.len();
        }
        Ok(out)
    }

    /// Path of a message for a `$batch` entry.
    pub fn batch_message_url(&self, id: &str, tail: Option<&str>) -> String {
        let mut url = self.endpoint(&["messages", id]);
        if let Some(tail) = tail
            && let Ok(mut path) = url.path_segments_mut()
        {
            path.push(tail);
        }
        let base = self.base.path().trim_end_matches('/');
        url.path()
            .strip_prefix(base)
            .unwrap_or(url.path())
            .to_owned()
    }
}

/// A contact as the sync stores it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphContact {
    pub id: String,
    pub change_key: Option<String>,
    pub name: String,
    pub emails: Vec<String>,
    pub phone: String,
    pub company: String,
    pub job_title: String,
    pub website: String,
    pub birthday: String,
    pub address: String,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactChange {
    Upsert(Box<GraphContact>),
    Removed(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContactPage {
    pub items: Vec<ContactChange>,
    pub next_link: Option<String>,
    pub delta_link: Option<String>,
}

/// The version tag of an event reply, in the form the calendar rows store
/// (`@odata.etag`, else `changeKey`).
fn event_version(value: &Value) -> Option<String> {
    value["@odata.etag"]
        .as_str()
        .or_else(|| value["changeKey"].as_str())
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A recurring series as Graph describes it: the master event with its
/// recurrence, the cancelled dates and the changed or cancelled occurrences.
#[derive(Debug, Clone)]
pub struct SeriesMaster {
    pub event: crate::graph::DeltaEvent,
    pub recurrence: Value,
    /// The master's first start as written by Graph (local time and zone name).
    pub start_local: Option<(String, String)>,
    pub cancelled: Vec<String>,
    pub exceptions: Vec<SeriesException>,
}

/// One occurrence of a series that was moved, changed or cancelled.
#[derive(Debug, Clone)]
pub struct SeriesException {
    pub original_start_ms: i64,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
    pub cancelled: bool,
}

/// One slot of a free/busy answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusySlot {
    pub start_ms: i64,
    pub end_ms: i64,
    /// `free`, `tentative`, `busy`, `oof`, `workingElsewhere`, `unknown`.
    pub status: String,
}

/// The answer to a meeting invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventResponse {
    Accept,
    Tentative,
    Decline,
}

impl EventResponse {
    fn action(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Tentative => "tentativelyAccept",
            Self::Decline => "decline",
        }
    }
}

/// Where an attachment upload session sends its bytes. The address comes from
/// Microsoft (it may be on another host than Graph), so it must be https
/// (or loopback for tests) without credentials; no bearer token is sent to it.
fn checked_upload_url(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value)
        .map_err(|_| CoreError::Network("Microsoft Graph sent an invalid upload address".into()))?;
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "[::1]"));
    if value.len() > MAX_NEXT_LINK_BYTES
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(CoreError::Network(
            "refusing an upload address that is not a plain https address".into(),
        ));
    }
    Ok(url)
}

impl GraphMail {
    /// Server-side search (`$search`) over the whole mailbox. The newest hits
    /// are not guaranteed to come first; Graph does not sort a search.
    pub async fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<GraphMessage>> {
        let cleaned: String = query
            .chars()
            .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
            .take(200)
            .collect();
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            return Ok(Vec::new());
        }
        let mut url = self.endpoint(&["messages"]);
        url.query_pairs_mut()
            .append_pair("$search", &format!("\"{cleaned}\""))
            .append_pair("$top", &limit.clamp(1, 100).to_string())
            .append_pair("$select", MESSAGE_SELECT);
        let value = self
            .json(
                reqwest::Method::GET,
                url,
                None,
                &[("Prefer", "IdType=\"ImmutableId\"")],
            )
            .await?;
        Ok(value["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["id"].as_str().map(|id| parse_message(item, id)))
            .collect())
    }

    /// Create a draft from a complete RFC 5322 message (headers such as
    /// `Message-ID` and `References` are kept). Returns the draft's id.
    pub async fn create_draft_mime(&self, raw_message: &[u8]) -> Result<String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw_message);
        let url = self.endpoint(&["messages"]);
        let raw = self
            .send(
                reqwest::Method::POST,
                url,
                Some(("text/plain", encoded.into_bytes())),
                &[("Prefer", "IdType=\"ImmutableId\"")],
                MAX_JSON_BYTES,
                false,
            )
            .await?;
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })?;
        value["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CoreError::Network("Microsoft Graph returned no draft id".into()))
    }

    /// Create a draft from a JSON message (for a message whose attachments
    /// are added separately). Returns the draft's id.
    pub async fn create_draft_json(&self, message: &Value) -> Result<String> {
        let url = self.endpoint(&["messages"]);
        let value = self
            .json(
                reqwest::Method::POST,
                url,
                Some(message),
                &[("Prefer", "IdType=\"ImmutableId\"")],
            )
            .await?;
        value["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CoreError::Network("Microsoft Graph returned no draft id".into()))
    }

    /// Start a reply draft to a stored message: Graph fills in the threading
    /// headers (`In-Reply-To`, `References`), which a JSON draft cannot carry.
    pub async fn create_reply_draft(&self, message_id: &str) -> Result<String> {
        let url = self.endpoint(&["messages", message_id, "createReply"]);
        let value = self
            .json(
                reqwest::Method::POST,
                url,
                Some(&json!({})),
                &[("Prefer", "IdType=\"ImmutableId\"")],
            )
            .await?;
        value["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CoreError::Network("Microsoft Graph returned no reply draft id".into()))
    }

    /// Attach a file of up to a few MiB to a draft in one request.
    pub async fn add_attachment(
        &self,
        draft_id: &str,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let url = self.endpoint(&["messages", draft_id, "attachments"]);
        self.json(
            reqwest::Method::POST,
            url,
            Some(&json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": name,
                "contentType": content_type,
                "contentBytes": base64::engine::general_purpose::STANDARD.encode(bytes),
            })),
            &[],
        )
        .await
        .map(|_| ())
    }

    /// Attach a large file to a draft through an upload session: the bytes go
    /// to the address Microsoft names, in chunks that are a multiple of
    /// 320 KiB, and nothing but the chunk is sent there (no bearer token).
    pub async fn upload_attachment(
        &self,
        draft_id: &str,
        name: &str,
        content_type: &str,
        path: &std::path::Path,
    ) -> Result<()> {
        use tokio::io::AsyncReadExt;
        let size = tokio::fs::metadata(path).await?.len();
        if size == 0 {
            return self.add_attachment(draft_id, name, content_type, &[]).await;
        }
        let url = self.endpoint(&["messages", draft_id, "attachments", "createUploadSession"]);
        let session = self
            .json(
                reqwest::Method::POST,
                url,
                Some(&json!({ "AttachmentItem": {
                    "attachmentType": "file",
                    "name": name,
                    "size": size,
                    "contentType": content_type,
                }})),
                &[],
            )
            .await?;
        let upload_url = checked_upload_url(session["uploadUrl"].as_str().ok_or_else(|| {
            CoreError::Network("Microsoft Graph returned no upload address".into())
        })?)?;
        let mut file = tokio::fs::File::open(path).await?;
        let mut offset: u64 = 0;
        while offset < size {
            let length = (size - offset).min(UPLOAD_CHUNK as u64) as usize;
            let mut chunk = vec![0u8; length];
            file.read_exact(&mut chunk).await?;
            let range = format!("bytes {}-{}/{}", offset, offset + length as u64 - 1, size);
            let mut attempts = 0;
            loop {
                attempts += 1;
                let response = self
                    .http
                    .put(upload_url.clone())
                    .header(reqwest::header::CONTENT_RANGE, &range)
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(chunk.clone())
                    .send()
                    .await
                    .map_err(|error| {
                        CoreError::Network(format!(
                            "uploading an attachment failed: {}",
                            error.without_url()
                        ))
                    })?;
                let status = response.status().as_u16();
                if (200..300).contains(&status) {
                    break;
                }
                if matches!(status, 429 | 500 | 502 | 503 | 504) && attempts < MAX_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(250 * attempts as u64)).await;
                    continue;
                }
                return Err(CoreError::Network(format!(
                    "Microsoft 365 refused an attachment chunk (HTTP {status})"
                )));
            }
            offset += length as u64;
        }
        Ok(())
    }

    /// Send a draft that exists on the server. A lost connection is reported
    /// as `SendUncertain` and never retried.
    pub async fn send_draft(&self, draft_id: &str) -> Result<()> {
        let url = self.endpoint(&["messages", draft_id, "send"]);
        let raw = self
            .send(reqwest::Method::POST, url, None, &[], MAX_JSON_BYTES, true)
            .await?;
        if matches!(raw.status, 200 | 202 | 204) {
            return Ok(());
        }
        Err(status_error(raw.status, &raw.body))
    }

    /// The contact folders of the mailbox (the default "Contacts" included).
    pub async fn contact_folders(&self) -> Result<Vec<(String, String)>> {
        let mut url = self.endpoint(&["contactFolders"]);
        url.query_pairs_mut()
            .append_pair("$select", "id,displayName")
            .append_pair("$top", "100");
        let mut link = Some(url);
        let mut out = Vec::new();
        let mut pages = 0;
        while let Some(url) = link.take() {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(CoreError::Network("too many contact folder pages".into()));
            }
            let value = self.json(reqwest::Method::GET, url, None, &[]).await?;
            for item in value["value"].as_array().into_iter().flatten() {
                if let Some(id) = item["id"].as_str() {
                    out.push((
                        id.to_owned(),
                        item["displayName"]
                            .as_str()
                            .unwrap_or("Contacts")
                            .to_owned(),
                    ));
                }
            }
            if let Some(next) = value["@odata.nextLink"].as_str() {
                link = Some(self.trusted_link(next)?);
            }
        }
        Ok(out)
    }

    /// One page of contact changes in a folder; `Ok(None)` when the saved
    /// delta link has expired (`410`).
    pub async fn contacts_delta(
        &self,
        folder_id: &str,
        link: Option<&str>,
    ) -> Result<Option<ContactPage>> {
        let url = match link {
            Some(link) => self.trusted_link(link)?,
            None => self.endpoint(&["contactFolders", folder_id, "contacts", "delta"]),
        };
        let raw = self
            .send(
                reqwest::Method::GET,
                url,
                None,
                &[("Prefer", "odata.maxpagesize=100")],
                MAX_JSON_BYTES,
                false,
            )
            .await?;
        if raw.status == 410 {
            return Ok(None);
        }
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })?;
        let mut page = ContactPage::default();
        for item in value["value"].as_array().into_iter().flatten() {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            if item.get("@removed").is_some() {
                page.items.push(ContactChange::Removed(id.to_owned()));
            } else {
                page.items
                    .push(ContactChange::Upsert(Box::new(parse_contact(item, id))));
            }
        }
        page.next_link = value["@odata.nextLink"].as_str().map(str::to_owned);
        page.delta_link = value["@odata.deltaLink"].as_str().map(str::to_owned);
        if let Some(link) = page.next_link.as_deref().or(page.delta_link.as_deref()) {
            self.trusted_link(link)?;
        }
        Ok(Some(page))
    }

    /// Create a contact in a folder; returns its id and change key.
    pub async fn create_contact(
        &self,
        folder_id: &str,
        body: &Value,
    ) -> Result<(String, Option<String>)> {
        let url = self.endpoint(&["contactFolders", folder_id, "contacts"]);
        let value = self
            .json(reqwest::Method::POST, url, Some(body), &[])
            .await?;
        let id = value["id"]
            .as_str()
            .ok_or_else(|| CoreError::Network("Microsoft Graph returned no contact id".into()))?;
        Ok((
            id.to_owned(),
            value["changeKey"].as_str().map(str::to_owned),
        ))
    }

    /// One contact as it is now on the server; `None` when it is gone.
    pub async fn get_contact(&self, contact_id: &str) -> Result<Option<GraphContact>> {
        let url = self.endpoint(&["contacts", contact_id]);
        let raw = self
            .send(reqwest::Method::GET, url, None, &[], MAX_JSON_BYTES, false)
            .await?;
        if raw.status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })?;
        Ok(Some(parse_contact(&value, contact_id)))
    }

    /// Change a contact; returns its new change key.
    pub async fn update_contact(&self, contact_id: &str, body: &Value) -> Result<Option<String>> {
        let url = self.endpoint(&["contacts", contact_id]);
        let value = self
            .json(reqwest::Method::PATCH, url, Some(body), &[])
            .await?;
        Ok(value["changeKey"].as_str().map(str::to_owned))
    }

    /// Delete a contact; one that is already gone counts as deleted.
    pub async fn delete_contact(&self, contact_id: &str) -> Result<()> {
        let url = self.endpoint(&["contacts", contact_id]);
        let raw = self
            .send(
                reqwest::Method::DELETE,
                url,
                None,
                &[],
                MAX_JSON_BYTES,
                false,
            )
            .await?;
        if (200..300).contains(&raw.status) || raw.status == 404 {
            return Ok(());
        }
        Err(status_error(raw.status, &raw.body))
    }

    /// People the signed-in user works with and the organization directory
    /// (`/me/people`, read only; needs `People.Read`): (name, address).
    pub async fn directory_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let query = query.trim();
        if query.is_empty() || query.len() > 200 || query.chars().any(char::is_control) {
            return Ok(Vec::new());
        }
        let mut url = self.endpoint(&["people"]);
        url.query_pairs_mut()
            .append_pair("$search", &format!("\"{}\"", query.replace('"', "")))
            .append_pair("$select", "displayName,scoredEmailAddresses")
            .append_pair("$top", &limit.clamp(1, 50).to_string());
        let value = self.json(reqwest::Method::GET, url, None, &[]).await?;
        let mut out = Vec::new();
        for person in value["value"].as_array().into_iter().flatten() {
            let name = person["displayName"].as_str().unwrap_or_default().trim();
            let address = person["scoredEmailAddresses"]
                .as_array()
                .and_then(|list| list.iter().find_map(|entry| entry["address"].as_str()))
                .map(str::trim)
                .filter(|address| address.contains('@'));
            if let Some(address) = address {
                out.push((name.to_owned(), address.to_owned()));
            }
        }
        // People the user never wrote to are in the tenant directory
        // (`/users`, needs `User.ReadBasic.All`); an administrator may not
        // have granted it, which only leaves this part out.
        let wanted = limit.clamp(1, 50);
        if out.len() < wanted {
            match self.directory_users(query, wanted - out.len()).await {
                Ok(users) => {
                    for (name, address) in users {
                        if !out
                            .iter()
                            .any(|(_, known)| known.eq_ignore_ascii_case(&address))
                        {
                            out.push((name, address));
                        }
                    }
                }
                Err(error @ (CoreError::NeedsReauth | CoreError::Auth(_) | CoreError::Offline)) => {
                    return Err(error);
                }
                Err(error) => tracing::debug!(%error, "tenant directory not searched"),
            }
        }
        Ok(out)
    }

    /// Users of the tenant whose name or address starts with `query`.
    async fn directory_users(&self, query: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty();
            path.push("users");
        }
        let quoted = query.replace('\'', "''");
        url.query_pairs_mut()
            .append_pair(
                "$filter",
                &format!("startswith(displayName,'{quoted}') or startswith(mail,'{quoted}')"),
            )
            .append_pair("$select", "displayName,mail")
            .append_pair("$top", &limit.clamp(1, 50).to_string());
        let value = self.json(reqwest::Method::GET, url, None, &[]).await?;
        Ok(value["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| {
                let address = user["mail"].as_str()?.trim();
                address.contains('@').then(|| {
                    (
                        user["displayName"]
                            .as_str()
                            .unwrap_or_default()
                            .trim()
                            .to_owned(),
                        address.to_owned(),
                    )
                })
            })
            .collect())
    }

    /// The mailbox's calendars.
    pub async fn calendars(&self) -> Result<Vec<crate::graph::GraphCalendar>> {
        let mut url = self.endpoint(&["calendars"]);
        url.query_pairs_mut()
            .append_pair("$select", "id,name,hexColor,isDefaultCalendar,canEdit")
            .append_pair("$top", "100");
        let mut link = Some(url);
        let mut out = Vec::new();
        let mut pages = 0;
        while let Some(url) = link.take() {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(CoreError::Network("too many calendar pages".into()));
            }
            let value = self.json(reqwest::Method::GET, url, None, &[]).await?;
            for c in value["value"].as_array().into_iter().flatten() {
                let Some(id) = c["id"].as_str() else {
                    continue;
                };
                out.push(crate::graph::GraphCalendar {
                    id: id.to_owned(),
                    name: c["name"].as_str().map(str::to_owned),
                    hex_color: c["hexColor"]
                        .as_str()
                        .filter(|color| color.starts_with('#'))
                        .map(str::to_owned),
                    is_default: c["isDefaultCalendar"].as_bool().unwrap_or(false),
                    can_edit: c["canEdit"].as_bool().unwrap_or(true),
                });
            }
            if let Some(next) = value["@odata.nextLink"].as_str() {
                link = Some(self.trusted_link(next)?);
            }
        }
        Ok(out)
    }

    /// One page of `calendarView/delta` for a calendar and window; `Ok(None)`
    /// when the saved link has expired (`410`).
    pub async fn calendar_delta(
        &self,
        calendar_id: &str,
        link: Option<&str>,
        window_ms: (i64, i64),
    ) -> Result<Option<crate::graph::DeltaPage>> {
        let url = match link {
            Some(link) => self.trusted_link(link)?,
            None => {
                let utc_z = |ms: i64| {
                    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                        .map(|date| date.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                        .ok_or_else(|| CoreError::Other("invalid timestamp".into()))
                };
                let mut url = self.endpoint(&["calendars", calendar_id, "calendarView", "delta"]);
                url.query_pairs_mut()
                    .append_pair("startDateTime", &utc_z(window_ms.0)?)
                    .append_pair("endDateTime", &utc_z(window_ms.1)?);
                url
            }
        };
        let raw = self
            .send(
                reqwest::Method::GET,
                url,
                None,
                &[("Prefer", "odata.maxpagesize=100")],
                MAX_JSON_BYTES,
                false,
            )
            .await?;
        if raw.status == 410 {
            return Ok(None);
        }
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })?;
        let page = crate::graph::DeltaPage {
            events: value["value"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(crate::graph::parse_delta_event)
                .collect(),
            next_link: value["@odata.nextLink"].as_str().map(str::to_owned),
            delta_link: value["@odata.deltaLink"].as_str().map(str::to_owned),
        };
        if let Some(link) = page.next_link.as_deref().or(page.delta_link.as_deref()) {
            self.trusted_link(link)?;
        }
        Ok(Some(page))
    }

    /// Create an event (in a calendar when given, else the default one).
    /// Returns the event id.
    pub async fn create_event(&self, calendar_id: Option<&str>, event: &Value) -> Result<String> {
        let url = match calendar_id {
            Some(id) => self.endpoint(&["calendars", id, "events"]),
            None => self.endpoint(&["events"]),
        };
        let value = self
            .json(reqwest::Method::POST, url, Some(event), &[])
            .await?;
        value["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CoreError::Network("Microsoft Graph returned no event id".into()))
    }

    /// Read a series master (`None` when it is gone) with its exceptions.
    pub async fn series_master(&self, event_id: &str) -> Result<Option<SeriesMaster>> {
        let mut url = self.endpoint(&["events", event_id]);
        url.query_pairs_mut()
            .append_pair("$expand", "exceptionOccurrences");
        let raw = self
            .send(reqwest::Method::GET, url, None, &[], MAX_JSON_BYTES, false)
            .await?;
        if raw.status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).map_err(|error| {
            CoreError::Network(format!("Microsoft Graph reply was not JSON: {error}"))
        })?;
        let Some(event) = crate::graph::parse_delta_event(&value) else {
            return Ok(None);
        };
        let start_local = value["start"]["dateTime"].as_str().map(|text| {
            (
                text.to_owned(),
                value["start"]["timeZone"]
                    .as_str()
                    .unwrap_or("UTC")
                    .to_owned(),
            )
        });
        let rfc3339_ms = |text: &Value| {
            let text = text.as_str()?;
            chrono::DateTime::parse_from_rfc3339(text)
                .or_else(|_| chrono::DateTime::parse_from_rfc3339(&format!("{text}Z")))
                .ok()
                .map(|date| date.timestamp_millis())
        };
        let exceptions = value["exceptionOccurrences"]
            .as_array()
            .into_iter()
            .flatten()
            .take(500)
            .filter_map(|item| {
                let parsed = crate::graph::parse_delta_event(item)?;
                Some(SeriesException {
                    original_start_ms: rfc3339_ms(&item["originalStart"])?,
                    start_ms: parsed.start_ms,
                    end_ms: parsed.end_ms,
                    cancelled: parsed.cancelled,
                })
            })
            .collect();
        Ok(Some(SeriesMaster {
            recurrence: value["recurrence"].clone(),
            start_local,
            cancelled: value["cancelledOccurrences"]
                .as_array()
                .into_iter()
                .flatten()
                .take(1000)
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
            exceptions,
            event,
        }))
    }

    /// Free/busy of mailboxes in a window (`POST /me/calendar/getSchedule`),
    /// one list of slots per address, in the order asked.
    pub async fn free_busy(
        &self,
        addresses: &[String],
        window_ms: (i64, i64),
    ) -> Result<Vec<(String, Vec<BusySlot>)>> {
        if addresses.is_empty() || addresses.len() > 20 {
            return Err(CoreError::Other(
                "ask for between 1 and 20 mailboxes at a time".into(),
            ));
        }
        let at = |ms: i64| {
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
                .map(|date| json!({"dateTime": date.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC"}))
                .ok_or_else(|| CoreError::Other("invalid timestamp".into()))
        };
        let body = json!({
            "schedules": addresses,
            "startTime": at(window_ms.0)?,
            "endTime": at(window_ms.1)?,
            "availabilityViewInterval": 30,
        });
        let url = self.endpoint(&["calendar", "getSchedule"]);
        let reply = self
            .json(
                reqwest::Method::POST,
                url,
                Some(&body),
                &[("Prefer", "outlook.timezone=\"UTC\"")],
            )
            .await?;
        let time = |value: &Value| {
            let text = value["dateTime"].as_str()?;
            let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
            Some(naive.and_utc().timestamp_millis())
        };
        Ok(reply["value"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| {
                let address = entry["scheduleId"].as_str().unwrap_or_default().to_owned();
                let slots = entry["scheduleItems"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(500)
                    .filter_map(|slot| {
                        Some(BusySlot {
                            start_ms: time(&slot["start"])?,
                            end_ms: time(&slot["end"])?,
                            status: slot["status"].as_str().unwrap_or("unknown").to_owned(),
                        })
                    })
                    .collect();
                (address, slots)
            })
            .collect())
    }

    /// Change an event; returns its new version tag when Graph sent one.
    pub async fn update_event(&self, event_id: &str, event: &Value) -> Result<Option<String>> {
        let url = self.endpoint(&["events", event_id]);
        let reply = self
            .json(reqwest::Method::PATCH, url, Some(event), &[])
            .await?;
        Ok(event_version(&reply))
    }

    /// The server's current version tag of an event (`None` outer: the event
    /// is gone; `None` inner: Graph named no version).
    pub async fn event_version(&self, event_id: &str) -> Result<Option<Option<String>>> {
        let mut url = self.endpoint(&["events", event_id]);
        url.query_pairs_mut().append_pair("$select", "id,changeKey");
        let raw = self
            .send(reqwest::Method::GET, url, None, &[], MAX_JSON_BYTES, false)
            .await?;
        if raw.status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&raw.status) {
            return Err(status_error(raw.status, &raw.body));
        }
        let value: Value = serde_json::from_slice(&raw.body).unwrap_or(Value::Null);
        Ok(Some(event_version(&value)))
    }

    /// Delete an event; one that is already gone counts as deleted.
    pub async fn delete_event(&self, event_id: &str) -> Result<()> {
        let url = self.endpoint(&["events", event_id]);
        self.expect_gone(reqwest::Method::DELETE, url, None).await
    }

    /// Answer a meeting invitation (the organizer is told).
    pub async fn respond_to_event(
        &self,
        event_id: &str,
        response: EventResponse,
        comment: &str,
    ) -> Result<()> {
        let url = self.endpoint(&["events", event_id, response.action()]);
        self.json(
            reqwest::Method::POST,
            url,
            Some(&json!({ "comment": comment, "sendResponse": true })),
            &[],
        )
        .await
        .map(|_| ())
    }
}

/// The Graph fields for a contact record (only what the sync reads back).
pub(crate) fn contact_body(record: &crate::models::ContactRecord) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("displayName".into(), Value::from(record.name.clone()));
    body.insert(
        "emailAddresses".into(),
        serde_json::json!([{ "address": record.email, "name": record.name }]),
    );
    let text = |key: &str, value: &str| (key.to_owned(), Value::from(value.to_owned()));
    for (key, value) in [
        text("companyName", &record.company),
        text("jobTitle", &record.job_title),
        text("businessHomePage", &record.website),
        text("personalNotes", &record.notes),
    ] {
        body.insert(key, value);
    }
    body.insert(
        "mobilePhone".into(),
        if record.phone.is_empty() {
            Value::Null
        } else {
            Value::from(record.phone.clone())
        },
    );
    body.insert(
        "birthday".into(),
        if record.birthday.len() == 10 {
            Value::from(format!("{}T00:00:00Z", record.birthday))
        } else {
            Value::Null
        },
    );
    body.insert(
        "businessAddress".into(),
        serde_json::json!({ "street": record.postal_address }),
    );
    Value::Object(body)
}

fn parse_contact(item: &Value, id: &str) -> GraphContact {
    let text = |key: &str| item[key].as_str().unwrap_or_default().trim().to_owned();
    let phone = item["mobilePhone"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            item["businessPhones"]
                .as_array()
                .and_then(|phones| phones.iter().find_map(Value::as_str))
        })
        .or_else(|| {
            item["homePhones"]
                .as_array()
                .and_then(|phones| phones.iter().find_map(Value::as_str))
        })
        .unwrap_or_default()
        .trim()
        .to_owned();
    let address = ["businessAddress", "homeAddress", "otherAddress"]
        .iter()
        .find_map(|key| {
            let parts: Vec<&str> = ["street", "city", "state", "postalCode", "countryOrRegion"]
                .iter()
                .filter_map(|part| item[*key][*part].as_str())
                .filter(|part| !part.trim().is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        })
        .unwrap_or_default();
    GraphContact {
        id: id.to_owned(),
        change_key: item["changeKey"].as_str().map(str::to_owned),
        name: text("displayName"),
        emails: item["emailAddresses"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry["address"].as_str())
            .map(|address| address.trim().to_owned())
            .filter(|address| address.contains('@'))
            .collect(),
        phone,
        company: text("companyName"),
        job_title: text("jobTitle"),
        website: item["businessHomePage"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned(),
        birthday: item["birthday"]
            .as_str()
            .unwrap_or_default()
            .split('T')
            .next()
            .unwrap_or_default()
            .to_owned(),
        address,
        notes: text("personalNotes"),
    }
}

fn parse_message(item: &Value, id: &str) -> GraphMessage {
    GraphMessage {
        id: id.to_owned(),
        internet_message_id: item["internetMessageId"]
            .as_str()
            .map(|id| id.trim().trim_matches(['<', '>']).to_owned()),
        subject: item["subject"].as_str().unwrap_or_default().to_owned(),
        is_read: item["isRead"].as_bool().unwrap_or(false),
        is_draft: item["isDraft"].as_bool().unwrap_or(false),
        flagged: item.pointer("/flag/flagStatus").and_then(Value::as_str) == Some("flagged"),
        categories: item["categories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect(),
        received_ms: item["receivedDateTime"]
            .as_str()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|date| date.timestamp_millis()),
        has_attachments: item["hasAttachments"].as_bool().unwrap_or(false),
        parent_folder_id: item["parentFolderId"].as_str().map(str::to_owned),
    }
}

/// A token fetched elsewhere (the sign-in that is being verified); it cannot
/// be refreshed, so a rejection is final.
pub struct FixedToken(pub String);

#[async_trait]
impl AccessTokenSource for FixedToken {
    async fn access_token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
    async fn invalidate(&self) {}
}

#[cfg(test)]
pub(crate) struct StaticToken(pub &'static str);

#[cfg(test)]
#[async_trait]
impl AccessTokenSource for StaticToken {
    async fn access_token(&self) -> Result<String> {
        Ok(self.0.to_owned())
    }
    async fn invalidate(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::settings::ExchangeCloud;
    use crate::exchange::testing::{FakeServer, Reply};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn client_for(server: &FakeServer, shared: &str) -> GraphMail {
        let settings = ExchangeSettings {
            enabled: true,
            client_id: "facd6cff-a294-4415-b59f-c5b01937d7bd".into(),
            cloud: ExchangeCloud::Custom,
            custom_login_url: server.url(),
            custom_graph_url: format!("{}/graph", server.url()),
            shared_mailbox: shared.into(),
            ..ExchangeSettings::default()
        };
        GraphMail::new(&settings, Arc::new(StaticToken("TOKEN")))
            .unwrap()
            .with_retry_cap(Duration::from_millis(5))
    }

    #[tokio::test]
    async fn folder_tree_is_walked_with_children_and_paging() {
        let base = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_base = base.clone();
        let server = FakeServer::start(move |request| {
            let root = seen_base.lock().unwrap().clone();
            assert_eq!(request.header("authorization"), Some("Bearer TOKEN"));
            match request.path.as_str() {
                "/graph/v1.0/me/mailFolders" if request.query.contains("skip=1") => Reply::json(
                    200,
                    r#"{"value":[{"id":"F3","displayName":"Archive","childFolderCount":0}]}"#,
                ),
                "/graph/v1.0/me/mailFolders" => Reply::json(
                    200,
                    &format!(
                        r#"{{"value":[{{"id":"F1","displayName":"Inbox","childFolderCount":1}},{{"id":"F2","displayName":"Sent Items","childFolderCount":0}}],"@odata.nextLink":"{root}/graph/v1.0/me/mailFolders?skip=1"}}"#
                    ),
                ),
                "/graph/v1.0/me/mailFolders/F1/childFolders" => Reply::json(
                    200,
                    r#"{"value":[{"id":"F4","displayName":"Projects","childFolderCount":0}]}"#,
                ),
                other => panic!("unexpected {other}"),
            }
        })
        .await;
        *base.lock().unwrap() = server.url();
        let folders = client_for(&server, "").folders().await.unwrap();
        let names: Vec<_> = folders
            .iter()
            .map(|f| (f.name.as_str(), f.parent_id.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                ("Inbox", None),
                ("Sent Items", None),
                ("Archive", None),
                ("Projects", Some("F1"))
            ]
        );
    }

    #[tokio::test]
    async fn delta_pages_report_upserts_removals_and_the_final_link() {
        let root = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_root = root.clone();
        let server = FakeServer::start(move |request| {
            let root = seen_root.lock().unwrap().clone();
            assert_eq!(request.path, "/graph/v1.0/me/mailFolders/F1/messages/delta");
            if request.query.contains("page=2") {
                return Reply::json(
                    200,
                    &format!(
                        r#"{{"value":[{{"id":"M3","@removed":{{"reason":"deleted"}}}}],"@odata.deltaLink":"{root}/graph/v1.0/me/mailFolders/F1/messages/delta?$deltatoken=T1"}}"#
                    ),
                );
            }
            assert!(request.query.contains("receivedDateTime"), "{}", request.query);
            assert!(request.header("prefer").unwrap().contains("maxpagesize"));
            Reply::json(
                200,
                &format!(
                    r#"{{"value":[{{"id":"M1","internetMessageId":"<a@x>","subject":"Hello","isRead":true,"isDraft":false,"flag":{{"flagStatus":"flagged"}},"categories":["Red"],"receivedDateTime":"2026-01-02T03:04:05Z","hasAttachments":true,"parentFolderId":"F1"}}],"@odata.nextLink":"{root}/graph/v1.0/me/mailFolders/F1/messages/delta?page=2"}}"#
                ),
            )
        })
        .await;
        *root.lock().unwrap() = server.url();
        let client = client_for(&server, "");
        let DeltaResult::Page(first) = client
            .delta("F1", None, Some(1_700_000_000_000))
            .await
            .unwrap()
        else {
            panic!("expected a page");
        };
        let DeltaItem::Upsert(message) = &first.items[0] else {
            panic!()
        };
        assert_eq!(message.internet_message_id.as_deref(), Some("a@x"));
        assert!(message.is_read && message.flagged && message.has_attachments);
        assert_eq!(message.categories, ["Red"]);
        assert_eq!(message.received_ms, Some(1_767_323_045_000));
        assert!(first.delta_link.is_none());
        let DeltaResult::Page(second) = client
            .delta("F1", first.next_link.as_deref(), None)
            .await
            .unwrap()
        else {
            panic!();
        };
        assert_eq!(second.items, [DeltaItem::Removed("M3".into())]);
        assert!(
            second
                .delta_link
                .as_deref()
                .unwrap()
                .contains("deltatoken=T1")
        );
    }

    #[tokio::test]
    async fn expired_delta_link_is_reported_not_failed() {
        let server = FakeServer::start(|_| {
            Reply::json(
                410,
                r#"{"error":{"code":"syncStateNotFound","message":"gone"}}"#,
            )
        })
        .await;
        let client = client_for(&server, "");
        let link = format!(
            "{}/graph/v1.0/me/mailFolders/F1/messages/delta?$deltatoken=old",
            server.url()
        );
        assert_eq!(
            client.delta("F1", Some(&link), None).await.unwrap(),
            DeltaResult::Expired
        );
    }

    #[tokio::test]
    async fn links_that_leave_the_graph_host_are_refused() {
        let server = FakeServer::start(|_| {
            Reply::json(
                200,
                r#"{"value":[{"id":"F1","displayName":"Inbox"}],"@odata.nextLink":"http://evil.example.org/graph/v1.0/me/mailFolders?x=1"}"#,
            )
        })
        .await;
        let client = client_for(&server, "");
        let error = client.folders().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("leaves the Microsoft Graph host"),
            "{error}"
        );
        // A saved link pointing elsewhere is never sent a token.
        assert!(
            client
                .delta("F1", Some("https://evil.example.org/graph/v1.0/x"), None)
                .await
                .is_err()
        );
        assert!(
            client
                .delta("F1", Some("http://127.0.0.1:1/other"), None)
                .await
                .is_err()
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn throttling_is_retried_with_retry_after_then_gives_up() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let server = FakeServer::start(move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                Reply::json(
                    429,
                    r#"{"error":{"code":"TooManyRequests","message":"slow"}}"#,
                )
                .with_header("Retry-After", "1")
            } else {
                Reply::json(200, r#"{"id":"INBOX1"}"#)
            }
        })
        .await;
        let client = client_for(&server, "");
        assert_eq!(client.probe().await.unwrap(), "INBOX1");
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let always = FakeServer::start(|_| Reply::empty(429).with_header("Retry-After", "0")).await;
        let error = client_for(&always, "").probe().await.unwrap_err();
        assert!(error.to_string().contains("throttling"), "{error}");
        assert_eq!(always.requests().len(), MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn a_rejected_token_is_refreshed_once() {
        struct Rotating(AtomicUsize);
        #[async_trait]
        impl AccessTokenSource for Rotating {
            async fn access_token(&self) -> Result<String> {
                Ok(format!("T{}", self.0.load(Ordering::SeqCst)))
            }
            async fn invalidate(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let server = FakeServer::start(|request| {
            if request.header("authorization") == Some("Bearer T1") {
                Reply::json(200, r#"{"id":"I"}"#)
            } else {
                Reply::json(
                    401,
                    r#"{"error":{"code":"InvalidAuthenticationToken","message":"expired"}}"#,
                )
            }
        })
        .await;
        let mut client = client_for(&server, "");
        client.tokens = Arc::new(Rotating(AtomicUsize::new(0)));
        assert_eq!(client.probe().await.unwrap(), "I");
        // A token that keeps failing at sign-in says so instead of asking for
        // the sign-in the person is in the middle of.
        let always = FakeServer::start(|_| Reply::empty(401)).await;
        let error = client_for(&always, "").probe().await.unwrap_err();
        assert!(
            matches!(&error, CoreError::Auth(text) if text.contains("did not accept")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn requests_go_through_the_account_proxy() {
        let proxy = FakeServer::start(|_| Reply::json(200, r#"{"id":"INBOX1"}"#)).await;
        let settings = ExchangeSettings {
            enabled: true,
            client_id: "facd6cff-a294-4415-b59f-c5b01937d7bd".into(),
            cloud: ExchangeCloud::Custom,
            custom_login_url: "http://127.0.0.1:9".into(),
            custom_graph_url: "http://127.0.0.1:9/graph".into(),
            proxy_url: proxy.url(),
            ..ExchangeSettings::default()
        };
        let client = GraphMail::new(&settings, Arc::new(StaticToken("TOKEN"))).unwrap();
        assert_eq!(client.probe().await.unwrap(), "INBOX1");
        let seen = proxy.requests();
        assert!(
            seen[0]
                .path
                .starts_with("http://127.0.0.1:9/graph/v1.0/me/mailFolders/inbox"),
            "{}",
            seen[0].path
        );
    }

    #[test]
    fn upload_addresses_must_be_plain_https_or_loopback() {
        assert!(checked_upload_url("https://outlook.office.com/api/upload/abc?x=1").is_ok());
        assert!(checked_upload_url("http://127.0.0.1:8080/upload/abc").is_ok());
        for bad in [
            "http://example.org/upload",
            "https://user@example.org/upload",
            "https://example.org/upload#frag",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(checked_upload_url(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn an_attachment_upload_sends_ranges_without_the_bearer_token() {
        let ranges = Arc::new(std::sync::Mutex::new(Vec::<(String, bool)>::new()));
        let seen = ranges.clone();
        let server = FakeServer::start(move |request| {
            if request.method == "PUT" {
                seen.lock().unwrap().push((
                    request
                        .header("content-range")
                        .unwrap_or_default()
                        .to_owned(),
                    request.header("authorization").is_some(),
                ));
                return Reply::json(200, "{}");
            }
            let base = request.header("host").unwrap_or_default().to_owned();
            Reply::json(
                200,
                &format!(r#"{{"uploadUrl":"http://{base}/put/session"}}"#),
            )
        })
        .await;
        let dir = std::env::temp_dir().join(format!("rustmail-upload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.bin");
        std::fs::write(&file, vec![1u8; UPLOAD_CHUNK + 1000]).unwrap();
        client_for(&server, "")
            .upload_attachment("D1", "big.bin", "application/octet-stream", &file)
            .await
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let ranges = ranges.lock().unwrap().clone();
        let total = UPLOAD_CHUNK + 1000;
        assert_eq!(
            ranges,
            vec![
                (format!("bytes 0-{}/{total}", UPLOAD_CHUNK - 1), false),
                (format!("bytes {UPLOAD_CHUNK}-{}/{total}", total - 1), false),
            ]
        );
        assert_eq!(UPLOAD_CHUNK % (320 * 1024), 0);
    }

    #[tokio::test]
    async fn a_search_quotes_the_text_and_reads_the_hits() {
        let server = FakeServer::start(|request| {
            assert!(
                request.query.contains("%24search=%22tax+return%22"),
                "{}",
                request.query
            );
            Reply::json(
                200,
                r#"{"value":[{"id":"M1","subject":"Tax return","parentFolderId":"inbox"}]}"#,
            )
        })
        .await;
        let hits = client_for(&server, "")
            .search_messages("tax \"return\"", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject, "Tax return");
        assert!(
            client_for(&server, "")
                .search_messages("  ", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_contact_keeps_its_addresses_phone_and_address() {
        let contact = parse_contact(
            &json!({
                "displayName": " Ann Lee ",
                "emailAddresses": [{"address": "ann@example.org"}, {"address": "no-at-sign"}],
                "businessPhones": ["+41 1"],
                "birthday": "1990-05-04T00:00:00Z",
                "businessAddress": {"street": "Main St 1", "city": "Zurich", "countryOrRegion": "CH"},
            }),
            "C1",
        );
        assert_eq!(contact.name, "Ann Lee");
        assert_eq!(contact.emails, vec!["ann@example.org"]);
        assert_eq!(contact.phone, "+41 1");
        assert_eq!(contact.birthday, "1990-05-04");
        assert_eq!(contact.address, "Main St 1, Zurich, CH");
    }

    #[tokio::test]
    async fn forbidden_and_missing_map_to_readable_errors() {
        let server = FakeServer::start(|request| {
            if request.path.ends_with("/inbox") {
                Reply::json(
                    403,
                    r#"{"error":{"code":"ErrorAccessDenied","message":"Access is denied."}}"#,
                )
            } else {
                Reply::json(
                    404,
                    r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#,
                )
            }
        })
        .await;
        let client = client_for(&server, "");
        let denied = client.probe().await.unwrap_err().to_string();
        assert!(
            denied.contains("ErrorAccessDenied") && denied.contains("permissions"),
            "{denied}"
        );
        assert_eq!(client.well_known_folder("archive").await.unwrap(), None);
        // Deleting something already gone is fine.
        client.delete_message("M9").await.unwrap();
        let cannot = FakeServer::start(|_| {
            Reply::json(
                403,
                r#"{"error":{"code":"ErrorCannotDeleteObject","message":"x"}}"#,
            )
        })
        .await;
        client_for(&cannot, "").delete_message("M9").await.unwrap();
        client_for(&cannot, "").probe().await.unwrap_err();
    }

    #[tokio::test]
    async fn mime_flags_move_delete_and_send_use_the_right_calls() {
        let server =
            FakeServer::start(
                |request| match (request.method.as_str(), request.path.as_str()) {
                    ("GET", "/graph/v1.0/me/messages/M1/$value") => {
                        Reply::bytes(200, "message/rfc822", b"Subject: hi\r\n\r\nbody".to_vec())
                    }
                    ("PATCH", "/graph/v1.0/me/messages/M1") => Reply::json(200, "{}"),
                    ("GET", "/graph/v1.0/me/messages/M5") => {
                        Reply::json(200, r#"{"parentFolderId":"F7"}"#)
                    }
                    ("GET", "/graph/v1.0/me/messages/M6") => Reply::json(
                        404,
                        r#"{"error":{"code":"ErrorItemNotFound","message":"x"}}"#,
                    ),
                    ("POST", "/graph/v1.0/me/messages/M1/move") => {
                        assert!(request.body_text().contains("\"destinationId\":\"F9\""));
                        Reply::json(201, r#"{"id":"M1-moved"}"#)
                    }
                    ("DELETE", "/graph/v1.0/me/messages/M2") => Reply::empty(204),
                    ("POST", "/graph/v1.0/me/messages/M3/permanentDelete") => Reply::empty(204),
                    ("POST", "/graph/v1.0/me/sendMail") => {
                        assert_eq!(request.header("content-type"), Some("text/plain"));
                        Reply::empty(202)
                    }
                    other => panic!("unexpected {other:?}"),
                },
            )
            .await;
        let client = client_for(&server, "");
        assert_eq!(
            client.message_mime("M1").await.unwrap(),
            b"Subject: hi\r\n\r\nbody"
        );
        assert_eq!(
            client.message_parent("M5").await.unwrap().as_deref(),
            Some("F7")
        );
        assert_eq!(client.message_parent("M6").await.unwrap(), None);
        client.set_read("M1", true).await.unwrap();
        client.set_flagged("M1", true).await.unwrap();
        client
            .set_categories("M1", &["Blue".to_owned()])
            .await
            .unwrap();
        assert_eq!(client.move_message("M1", "F9").await.unwrap(), "M1-moved");
        client.delete_message("M2").await.unwrap();
        client.permanent_delete("M3").await.unwrap();
        client
            .send_mime(b"From: a@b.c\r\nTo: d@e.f\r\n\r\nhi")
            .await
            .unwrap();
        let requests = server.requests();
        let patches: Vec<_> = requests
            .iter()
            .filter(|r| r.method == "PATCH")
            .map(|r| r.body_text())
            .collect();
        assert_eq!(
            patches,
            [
                r#"{"isRead":true}"#,
                r#"{"flag":{"flagStatus":"flagged"}}"#,
                r#"{"categories":["Blue"]}"#
            ]
        );
        let sent = requests
            .iter()
            .find(|r| r.path.ends_with("/sendMail"))
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(sent.body_text())
            .unwrap();
        assert!(decoded.starts_with(b"From: a@b.c"));
    }

    #[tokio::test]
    async fn oversized_mime_replies_and_sends_are_refused() {
        let server =
            FakeServer::start(|_| Reply::bytes(200, "message/rfc822", vec![b'a'; 2048])).await;
        let client = client_for(&server, "");
        assert_eq!(client.message_mime("M").await.unwrap().len(), 2048);
        let huge = vec![b'x'; MAX_MIME_BYTES + 1];
        assert!(
            client
                .send_mime(&huge)
                .await
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        assert!(server.requests().len() == 1);
    }

    #[tokio::test]
    async fn a_shared_mailbox_uses_the_users_path() {
        let server = FakeServer::start(|request| {
            assert_eq!(
                request.path,
                "/graph/v1.0/users/team@contoso.com/mailFolders/inbox"
            );
            Reply::json(200, r#"{"id":"I"}"#)
        })
        .await;
        client_for(&server, "team@contoso.com")
            .probe()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn batch_splits_into_groups_of_twenty_and_retries_throttled_entries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let server = FakeServer::start(move |request| {
            assert_eq!(request.path, "/graph/v1.0/$batch");
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let body: Value = serde_json::from_str(&request.body_text()).unwrap();
            let entries = body["requests"].as_array().unwrap();
            assert!(entries.len() <= 20);
            let responses: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    let id = entry["id"].as_str().unwrap();
                    // The very first entry is throttled once.
                    if n == 0 && id == "0" {
                        json!({"id": id, "status": 429, "headers": {"Retry-After": "0"}, "body": {}})
                    } else {
                        json!({"id": id, "status": 200, "body": {}})
                    }
                })
                .collect();
            Reply::json(200, &json!({ "responses": responses }).to_string())
        })
        .await;
        let client = client_for(&server, "");
        let requests: Vec<_> = (0..45)
            .map(|n| BatchRequest {
                method: "PATCH",
                url: client.batch_message_url(&format!("M{n}"), None),
                body: Some(json!({"isRead": true})),
            })
            .collect();
        assert_eq!(requests[0].url, "/me/messages/M0");
        let responses = client.batch(requests).await.unwrap();
        assert_eq!(responses.len(), 45);
        assert!(responses.iter().all(|r| r.status == 200));
        assert_eq!(responses[44].index, 44);
        // 3 chunks plus one retry of the throttled entry.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
