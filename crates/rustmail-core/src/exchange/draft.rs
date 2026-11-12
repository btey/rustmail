//! Drafts and large messages for Exchange Online.
//!
//! A local draft is pushed to the server's Drafts folder by rebuilding it:
//! the new copy is created first, then the previous one is deleted, and the
//! mapping to the local row follows (Graph gives every copy a new id). A
//! message up to [`MAX_SIMPLE_MIME_BYTES`] travels as MIME, so `Message-ID`,
//! `In-Reply-To` and `References` survive. A larger message becomes a JSON
//! draft whose big attachments go through upload sessions. A JSON draft
//! cannot carry the threading headers (Graph accepts only `x-` headers
//! there), so a large reply to a message stored on the server starts from
//! `createReply`, which sets `In-Reply-To` and `References` itself; a large
//! reply to a message Graph does not know has no threading headers.

use super::client::{GraphMail, MAX_INLINE_ATTACHMENT_BYTES, MAX_SIMPLE_MIME_BYTES};
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::models::{AccountConfig, Address};
use crate::sync::engine::SyncCtx;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

/// Everything the local database knows about an outgoing message.
struct Parts {
    detail: crate::models::MessageDetail,
    reply_to: Option<Address>,
    bcc: Vec<Address>,
    references: Vec<String>,
    in_reply_to: Option<String>,
    stored_message_id: Option<String>,
    /// (staged file, display name)
    attachments: Vec<(std::path::PathBuf, String)>,
}

async fn load_parts(ctx: &SyncCtx, draft_id: i64) -> Result<Parts> {
    let staging_root = tokio::fs::canonicalize(ctx.paths.draft_attachments_dir())
        .await
        .ok();
    let (detail, reply_to, bcc, mut references, in_reply_to, stored_message_id, files) = ctx
        .db
        .read(move |conn| {
            let detail = repo::messages::detail(conn, draft_id)?;
            let reply_to =
                repo::sender_identities::get_verified(conn, detail.account_id, &detail.from.email)?
                    .and_then(|identity| identity.reply_to());
            let (bcc_json, stored_message_id): (String, Option<String>) = conn.query_row(
                "SELECT bcc_json,message_id FROM messages WHERE id=?1 AND is_draft=1",
                params![draft_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let parent_id = conn
                .query_row(
                    "SELECT in_reply_to_message_id FROM drafts_meta WHERE message_id=?1",
                    params![draft_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten();
            let mut references = Vec::new();
            let mut in_reply_to = None;
            if let Some(parent_id) = parent_id {
                let mut stmt =
                    conn.prepare("SELECT ref_message_id FROM message_refs WHERE message_id=?1")?;
                references = stmt
                    .query_map(params![parent_id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if let Some(parent) = repo::messages::get_row(conn, parent_id)?
                    && let Some(message_id) = parent.message_id
                {
                    references.push(message_id.clone());
                    in_reply_to = Some(message_id);
                }
            }
            let mut stmt =
                conn.prepare("SELECT file_path,filename FROM draft_attachments WHERE draft_id=?1")?;
            let files = stmt
                .query_map(params![draft_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let bcc = serde_json::from_str::<Vec<Address>>(&bcc_json)?;
            Ok((
                detail,
                reply_to,
                bcc,
                references,
                in_reply_to,
                stored_message_id,
                files,
            ))
        })
        .await?;
    references.sort();
    references.dedup();
    let mut attachments = Vec::new();
    for (path, filename) in files {
        // Only files the app staged itself may be read and sent.
        let canonical = tokio::fs::canonicalize(&path)
            .await
            .map_err(|error| CoreError::Other(format!("attachment {filename}: {error}")))?;
        if !staging_root
            .as_ref()
            .is_some_and(|root| canonical.starts_with(root))
        {
            return Err(CoreError::Other(format!(
                "attachment {filename}: refusing file outside the staging area"
            )));
        }
        attachments.push((canonical, filename));
    }
    Ok(Parts {
        detail,
        reply_to,
        bcc,
        references,
        in_reply_to,
        stored_message_id,
        attachments,
    })
}

/// The whole message as MIME, with the Message-ID it will carry.
async fn build_raw(parts: &Parts) -> Result<(String, Vec<u8>)> {
    let mut outgoing_attachments = Vec::new();
    for (path, filename) in &parts.attachments {
        let size = tokio::fs::metadata(path).await?.len();
        outgoing_attachments.push(crate::mime::OutgoingAttachment {
            mime_type: crate::queue::mime_guess_from_name(filename),
            filename: filename.clone(),
            bytes: crate::file_io::read(
                path.clone(),
                usize::try_from(size).unwrap_or(usize::MAX),
                "draft attachment",
            )
            .await?,
        });
    }
    let detail = &parts.detail;
    let domain = detail
        .from
        .email
        .rsplit_once('@')
        .map(|(_, value)| value)
        .unwrap_or("localhost");
    let (message_id, raw) = crate::mime::build_message_owned(crate::mime::OutgoingMessage {
        from: detail.from.clone(),
        reply_to: parts.reply_to.clone(),
        to: &detail.to,
        cc: &detail.cc,
        bcc: &parts.bcc,
        subject: &detail.subject,
        body_text: detail.text_body.as_deref().unwrap_or_default(),
        body_html: detail.html_body.as_deref(),
        in_reply_to: parts.in_reply_to.as_deref(),
        references: &parts.references,
        message_id: parts.stored_message_id.as_deref(),
        message_id_domain: domain,
        attachments: outgoing_attachments,
    })?;
    Ok((message_id.trim_matches(['<', '>']).to_owned(), raw))
}

fn recipients(list: &[Address]) -> Vec<Value> {
    list.iter()
        .map(|address| {
            json!({ "emailAddress": {
                "address": address.email,
                "name": address.name.clone().unwrap_or_default(),
            }})
        })
        .collect()
}

/// The message as a Graph JSON draft (no attachments yet).
fn json_draft(parts: &Parts, message_id: &str) -> Value {
    let detail = &parts.detail;
    let (content_type, content) = match detail.html_body.as_deref() {
        Some(html) if !html.trim().is_empty() => ("html", html.to_owned()),
        _ => ("text", detail.text_body.clone().unwrap_or_default()),
    };
    let mut draft = json!({
        "subject": detail.subject,
        "body": { "contentType": content_type, "content": content },
        "toRecipients": recipients(&detail.to),
        "ccRecipients": recipients(&detail.cc),
        "bccRecipients": recipients(&parts.bcc),
        "internetMessageId": format!("<{message_id}>"),
    });
    if let Some(reply_to) = &parts.reply_to {
        draft["replyTo"] = json!(recipients(std::slice::from_ref(reply_to)));
    }
    draft
}

/// A message too big for one MIME request: a JSON draft, then its
/// attachments (small ones inline, big ones by upload session).
async fn create_large(
    client: &GraphMail,
    parts: &Parts,
    message_id: &str,
    reply_to: Option<&str>,
) -> Result<String> {
    let draft_id = match reply_to {
        Some(original) => {
            let draft_id = client.create_reply_draft(original).await?;
            let mut body = json_draft(parts, message_id);
            // The reply keeps the id the server gave it.
            if let Some(object) = body.as_object_mut() {
                object.remove("internetMessageId");
            }
            if let Err(error) = client.update_message(&draft_id, &body).await {
                let _ = client.permanent_delete(&draft_id).await;
                return Err(error);
            }
            draft_id
        }
        None => {
            client
                .create_draft_json(&json_draft(parts, message_id))
                .await?
        }
    };
    let uploaded: Result<()> = async {
        for (path, name) in &parts.attachments {
            let content_type = crate::queue::mime_guess_from_name(name);
            let size = tokio::fs::metadata(path).await?.len();
            if size <= MAX_INLINE_ATTACHMENT_BYTES {
                let bytes = crate::file_io::read(
                    path.clone(),
                    usize::try_from(size).unwrap_or(usize::MAX),
                    "draft attachment",
                )
                .await?;
                client
                    .add_attachment(&draft_id, name, &content_type, &bytes)
                    .await?;
            } else {
                client
                    .upload_attachment(&draft_id, name, &content_type, path)
                    .await?;
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = uploaded {
        // Do not leave a half-built draft behind.
        let _ = client.permanent_delete(&draft_id).await;
        return Err(error);
    }
    Ok(draft_id)
}

/// The server id of the message this draft replies to, when it is stored.
async fn reply_target(ctx: &SyncCtx, account_id: i64, parts: &Parts) -> Option<String> {
    let in_reply_to = parts.in_reply_to.clone()?;
    ctx.db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT em.graph_id FROM exchange_messages em
                     JOIN messages m ON m.id = em.message_id
                     WHERE em.account_id = ?1 AND m.message_id = ?2 LIMIT 1",
                    params![account_id, in_reply_to],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })
        .await
        .ok()
        .flatten()
}

/// Whether the local row is still an unsent draft of this account.
async fn is_unsent_draft(ctx: &SyncCtx, draft_id: i64) -> Result<Option<i64>> {
    ctx.db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT folder_id FROM messages WHERE id=?1 AND is_draft=1",
                    params![draft_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
        })
        .await
}

/// Make the server's copy of a local draft match the local one.
pub(crate) async fn push_draft(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
    draft_id: i64,
) -> Result<()> {
    let account_id = config.id;
    // Sent or deleted meanwhile: nothing to push.
    let Some(folder_id) = is_unsent_draft(ctx, draft_id).await? else {
        return Ok(());
    };
    let parts = load_parts(ctx, draft_id).await?;
    let (message_id, raw) = build_raw(&parts).await?;
    let new_id = if raw.len() <= MAX_SIMPLE_MIME_BYTES {
        client.create_draft_mime(&raw).await?
    } else {
        let reply_to = reply_target(ctx, account_id, &parts).await;
        create_large(client, &parts, &message_id, reply_to.as_deref()).await?
    };
    let stable = message_id.clone();
    let previous: Option<String> = ctx
        .db
        .write({
            let new_id = new_id.clone();
            move |conn| {
                let tx = conn.transaction()?;
                // The draft may have been sent or deleted while the copy was
                // being created: then the new copy must not stay mapped.
                let still_draft: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND is_draft=1)",
                        params![draft_id],
                        |row| row.get(0),
                    )?;
                if !still_draft {
                    return Ok(Some(format!("\0{new_id}")));
                }
                tx.execute(
                    "UPDATE messages SET message_id=?2 WHERE id=?1 AND is_draft=1",
                    params![draft_id, stable],
                )?;
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT graph_id FROM exchange_messages WHERE account_id=?1 AND message_id=?2",
                        params![account_id, draft_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                tx.execute(
                    "DELETE FROM exchange_messages WHERE account_id=?1 AND message_id=?2",
                    params![account_id, draft_id],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO exchange_messages(account_id, graph_id, message_id, folder_id)
                     VALUES (?1,?2,?3,?4)",
                    params![account_id, new_id, draft_id, folder_id],
                )?;
                tx.commit()?;
                Ok(previous)
            }
        })
        .await?;
    match previous {
        Some(id) if id.starts_with('\0') => {
            let _ = client.permanent_delete(&id[1..]).await;
        }
        Some(old) if old != new_id => {
            client.permanent_delete(&old).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Remove the server copy of a draft that was deleted locally.
pub(crate) async fn delete_remote_draft(
    ctx: &SyncCtx,
    account_id: i64,
    client: &GraphMail,
    graph_id: &str,
) -> Result<()> {
    client.permanent_delete(graph_id).await?;
    let graph_id = graph_id.to_owned();
    ctx.db
        .write(move |conn| {
            conn.execute(
                "DELETE FROM exchange_messages WHERE account_id=?1 AND graph_id=?2",
                params![account_id, graph_id],
            )?;
            Ok(())
        })
        .await
}

/// After a send went through, the server's draft copy is not wanted any more
/// (Microsoft 365 files its own Sent Items copy). Best effort.
pub(crate) async fn discard_after_send(ctx: &SyncCtx, config: &AccountConfig, draft_id: i64) {
    let account_id = config.id;
    let Ok(Some(graph_id)) = ctx
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT graph_id FROM exchange_messages WHERE account_id=?1 AND message_id=?2",
                    params![account_id, draft_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })
        .await
    else {
        return;
    };
    // Unmap first so the sent copy can be adopted by its Message-ID.
    let _ = ctx
        .db
        .write(move |conn| {
            conn.execute(
                "DELETE FROM exchange_messages WHERE account_id=?1 AND message_id=?2",
                params![account_id, draft_id],
            )?;
            Ok(())
        })
        .await;
    if let Ok(client) = super::graph_for(ctx, config) {
        let _ = client.permanent_delete(&graph_id).await;
    }
}

/// Send a message that is too big for the MIME call: JSON draft, attachments
/// (upload sessions for the big ones), then send the draft.
pub(crate) async fn send_large(ctx: &SyncCtx, config: &AccountConfig, draft_id: i64) -> Result<()> {
    if config.settings.security.enabled() {
        return Err(CoreError::Other(
            "A signed or encrypted message of this size cannot be sent through Microsoft 365: it would lose its signature. Send it smaller, or without protection.".into(),
        ));
    }
    let client = super::graph_for(ctx, config)?;
    let parts = load_parts(ctx, draft_id).await?;
    let message_id = match parts.stored_message_id.as_deref() {
        Some(id) => id.trim_matches(['<', '>']).to_owned(),
        None => build_raw(&parts).await?.0,
    };
    let reply_to = reply_target(ctx, config.id, &parts).await;
    let remote = create_large(&client, &parts, &message_id, reply_to.as_deref()).await?;
    client.send_draft(&remote).await
}
