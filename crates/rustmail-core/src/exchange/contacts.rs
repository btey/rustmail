//! Contacts of an Exchange Online mailbox, read through Graph delta queries
//! into the same tables the CardDAV sync fills: each contact folder is an
//! address book (named `graph:<folder id>`), each contact an object whose
//! vCard is made from the Graph fields, and the first e-mail address keys the
//! unified contact. Local edits, new contacts and deletions are written back
//! before each pull (`push_contacts`); when the contact changed on the server
//! meanwhile, the server's version wins.

use super::client::{self, ContactChange, ContactPage, GraphContact, GraphMail};
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::CoreEvent;
use crate::models::{AccountConfig, ContactRecord, now_ms};
use crate::sync::engine::SyncCtx;

const MAX_PAGES: usize = 2_000;
const MAX_FOLDERS: usize = 200;
/// Contact pictures read for one page of changes.
const MAX_PHOTOS_PER_PAGE: usize = 100;

/// Write local contact changes to Microsoft 365. Returns whether anything
/// was sent. A folder that answers `403` is marked read only.
async fn push_contacts(ctx: &SyncCtx, config: &AccountConfig, client: &GraphMail) -> Result<bool> {
    let account_id = config.id;
    let objects = ctx
        .db
        .read(move |conn| repo::carddav::dirty_objects(conn, account_id))
        .await?;
    let mut changed = false;
    for object in objects {
        let book_id = object.addressbook_id;
        let Some(url) = ctx
            .db
            .read(move |conn| repo::carddav::addressbook_url(conn, book_id))
            .await?
        else {
            continue;
        };
        let Some(folder) = url.strip_prefix("graph:").map(str::to_owned) else {
            continue;
        };
        let id = object.id;
        let outcome: Result<()> = async {
            if object.deleted {
                if object.remote_exists {
                    client.delete_contact(&object.href).await?;
                }
                ctx.db
                    .write(move |conn| repo::carddav::delete_object(conn, id))
                    .await?;
                return Ok(());
            }
            let Some(contact_id) = object.contact_id else {
                return Ok(());
            };
            let Some(record) = ctx
                .db
                .read(move |conn| repo::carddav::contact(conn, contact_id))
                .await?
            else {
                return Ok(());
            };
            let body = client::contact_body(&record);
            let vcard =
                crate::carddav::vcard::serialize(&record, &format!("graph-{}", object.href));
            if object.remote_exists {
                match client.get_contact(&object.href).await? {
                    // Gone on the server: keep the local contact, stop syncing it.
                    None => {
                        ctx.db
                            .write(move |conn| repo::carddav::delete_object(conn, id))
                            .await?;
                    }
                    // Changed on the server since the last pull: the server
                    // wins and its version arrives with the next delta; the
                    // local edit is kept as a second contact (without the
                    // address, which names the first one) so nothing is lost.
                    Some(server)
                        if server.change_key.is_some() && server.change_key != object.etag =>
                    {
                        let mut copy = body.clone();
                        copy["displayName"] =
                            format!("{} (your edit)", record.name).into();
                        copy["emailAddresses"] = serde_json::json!([]);
                        copy["personalNotes"] = format!(
                            "{}\nThis is your edit of {}: the contact was also changed on Microsoft 365, whose version was kept.",
                            record.notes, record.email
                        )
                        .trim()
                        .into();
                        client.create_contact(&folder, &copy).await?;
                        ctx.bus.emit(CoreEvent::Notice {
                            account_id,
                            message: format!(
                                "{} was changed here and on Microsoft 365. The server's version was kept and your edit was saved as \"{} (your edit)\".",
                                record.name, record.name
                            ),
                        });
                        ctx.db
                            .write(move |conn| {
                                conn.execute(
                                    "UPDATE carddav_objects SET dirty=0 WHERE id=?1",
                                    [id],
                                )?;
                                Ok(())
                            })
                            .await?;
                    }
                    Some(_) => {
                        let key = client.update_contact(&object.href, &body).await?;
                        if let Some(photo) = record.extras.photo.as_deref() {
                            client.set_contact_photo(&object.href, photo).await?;
                        }
                        ctx.db
                            .write(move |conn| {
                                repo::carddav::clear_dirty(conn, id, key.as_deref(), &vcard)
                            })
                            .await?;
                    }
                }
            } else {
                let (graph_id, key) = client.create_contact(&folder, &body).await?;
                if let Some(photo) = record.extras.photo.as_deref() {
                    client.set_contact_photo(&graph_id, photo).await?;
                }
                ctx.db
                    .write(move |conn| {
                        conn.execute(
                            "UPDATE carddav_objects SET href=?2,etag=?3,vcard_raw=?4,
                               remote_exists=1,dirty=0 WHERE id=?1",
                            rusqlite::params![id, graph_id, key, vcard],
                        )?;
                        Ok(())
                    })
                    .await?;
            }
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => changed = true,
            Err(CoreError::Other(text)) if text.contains("denied this request") => {
                ctx.db
                    .write(move |conn| repo::carddav::mark_addressbook_read_only(conn, book_id))
                    .await?;
                tracing::warn!(book_id, "Microsoft 365 contact folder is read only");
            }
            Err(error) => return Err(error),
        }
    }
    Ok(changed)
}

fn record_for(contact: &GraphContact, photo: Option<Vec<u8>>) -> Option<ContactRecord> {
    let email = contact.emails.first()?.to_lowercase();
    let email_for_extras = email.clone();
    let has_photo = photo.is_some();
    let name = if contact.name.is_empty() {
        email.clone()
    } else {
        contact.name.clone()
    };
    Some(ContactRecord {
        id: 0,
        name,
        email,
        phone: contact.phone.clone(),
        company: contact.company.clone(),
        job_title: contact.job_title.clone(),
        website: contact.website.clone(),
        birthday: contact.birthday.clone(),
        postal_address: contact.address.clone(),
        notes: contact.notes.clone(),
        tags: String::new(),
        is_favorite: false,
        interactions: 0,
        last_interacted: None,
        account_ids: Vec::new(),
        is_managed: true,
        extras: crate::models::ContactExtras {
            emails: contact.emails.iter().skip(1).cloned().collect(),
            phones: contact.more_phones.clone(),
            addresses: contact.more_addresses.clone(),
            photo,
        }
        .cleaned(&email_for_extras),
        has_photo,
    })
}

/// One sync of every contact folder. Returns whether anything changed.
pub(crate) async fn sync_contacts(
    ctx: &SyncCtx,
    config: &AccountConfig,
    client: &GraphMail,
) -> Result<bool> {
    let account_id = config.id;
    let folders = client.contact_folders().await?;
    if folders.len() > MAX_FOLDERS {
        return Err(CoreError::Network(
            "the mailbox has too many contact folders".into(),
        ));
    }
    // Contacts changed here go first, so the pull below cannot undo them.
    if let Err(error) = push_contacts(ctx, config, client).await {
        if matches!(error, CoreError::NeedsReauth | CoreError::Auth(_)) {
            return Err(error);
        }
        tracing::debug!(account_id, %error, "exchange contacts not written back");
    }
    let listed = folders.clone();
    let books: Vec<(i64, String)> = ctx
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut out = Vec::new();
            let mut first = None;
            for (graph_id, name) in listed {
                let url = format!("graph:{graph_id}");
                // A folder Microsoft 365 refused writes to stays read only.
                let refused: bool = tx
                    .query_row(
                        "SELECT read_only FROM carddav_addressbooks WHERE account_id=?1 AND url=?2",
                        rusqlite::params![account_id, url],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                let id =
                    repo::carddav::upsert_addressbook(&tx, account_id, &url, Some(&name), refused)?;
                first.get_or_insert(id);
                out.push((id, graph_id));
            }
            if let Some(id) = first {
                repo::carddav::ensure_default(&tx, account_id, id)?;
            }
            tx.commit()?;
            Ok(out)
        })
        .await?;
    let mut changed = false;
    for (book_id, graph_id) in books {
        changed |= sync_folder(ctx, client, account_id, book_id, &graph_id).await?;
    }
    if changed {
        ctx.bus.emit(CoreEvent::ContactsUpdated { account_id });
    }
    Ok(changed)
}

async fn sync_folder(
    ctx: &SyncCtx,
    client: &GraphMail,
    account_id: i64,
    book_id: i64,
    graph_id: &str,
) -> Result<bool> {
    let (_, saved) = ctx
        .db
        .read(move |conn| repo::carddav::sync_state(conn, book_id))
        .await?;
    let mut link = saved;
    let mut restarted = false;
    let mut changed = false;
    let mut pages = 0;
    loop {
        pages += 1;
        if pages > MAX_PAGES {
            return Err(CoreError::Network("too many contact pages".into()));
        }
        let page: ContactPage = match client.contacts_delta(graph_id, link.as_deref()).await? {
            Some(page) => page,
            None => {
                // The saved link is too old: read the folder again. Contacts
                // already stored are matched by their Graph id.
                if restarted {
                    return Err(CoreError::Network(
                        "Microsoft 365 keeps rejecting the contact sync state".into(),
                    ));
                }
                restarted = true;
                link = None;
                continue;
            }
        };
        if !page.items.is_empty() {
            changed = true;
            let items = page.items.clone();
            // Pictures are read before the database write, a few per page; a
            // failed or missing one leaves the contact without a picture.
            let mut photos: std::collections::HashMap<String, Vec<u8>> =
                std::collections::HashMap::new();
            for item in &items {
                if let ContactChange::Upsert(contact) = item
                    && photos.len() < MAX_PHOTOS_PER_PAGE
                    && !contact.emails.is_empty()
                    && let Ok(Some(bytes)) = client.contact_photo(&contact.id).await
                {
                    photos.insert(contact.id.clone(), bytes);
                }
            }
            ctx.db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    for item in &items {
                        match item {
                            ContactChange::Removed(id) => {
                                repo::carddav::remove_remote(&tx, book_id, id)?;
                            }
                            ContactChange::Upsert(contact) => {
                                let uid = format!("graph-{}", contact.id);
                                match record_for(contact, photos.get(&contact.id).cloned()) {
                                    Some(record) => {
                                        let vcard = crate::carddav::vcard::serialize(&record, &uid);
                                        repo::carddav::upsert_remote(
                                            &tx,
                                            account_id,
                                            book_id,
                                            &contact.id,
                                            contact.change_key.as_deref(),
                                            &vcard,
                                            &record,
                                        )?;
                                    }
                                    // No e-mail address: the contact cannot be shown
                                    // in the e-mail keyed directory; remember it only.
                                    None => repo::carddav::upsert_unmapped_remote(
                                        &tx,
                                        book_id,
                                        &contact.id,
                                        contact.change_key.as_deref(),
                                        "",
                                    )?,
                                }
                            }
                        }
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await?;
        }
        let resume = page.next_link.clone().or(page.delta_link.clone());
        ctx.db
            .write(move |conn| {
                repo::carddav::set_sync_state(conn, book_id, None, resume.as_deref(), now_ms())
            })
            .await?;
        match page.next_link {
            Some(next) => link = Some(next),
            None => break,
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contact_without_an_address_has_no_record_and_the_first_address_wins() {
        let mut contact = GraphContact {
            id: "c1".into(),
            name: "Ann Lee".into(),
            ..Default::default()
        };
        assert!(record_for(&contact, None).is_none());
        contact.emails = vec!["Ann@Example.org".into(), "second@example.org".into()];
        let record = record_for(&contact, None).unwrap();
        assert_eq!(record.email, "ann@example.org");
        assert_eq!(record.name, "Ann Lee");
    }
}
