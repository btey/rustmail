//! Contacts of an Exchange Online mailbox, read through Graph delta queries
//! into the same tables the CardDAV sync fills: each contact folder is an
//! address book (named `graph:<folder id>`), each contact an object whose
//! vCard is made from the Graph fields, and the first e-mail address keys the
//! unified contact. Read only: edits are not written back to Microsoft 365.

use super::client::{ContactChange, ContactPage, GraphContact, GraphMail};
use crate::db::repo;
use crate::error::{CoreError, Result};
use crate::events::CoreEvent;
use crate::models::{AccountConfig, ContactRecord, now_ms};
use crate::sync::engine::SyncCtx;

const MAX_PAGES: usize = 2_000;
const MAX_FOLDERS: usize = 200;

fn record_for(contact: &GraphContact) -> Option<ContactRecord> {
    let email = contact.emails.first()?.to_lowercase();
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
    let listed = folders.clone();
    let books: Vec<(i64, String)> = ctx
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut out = Vec::new();
            let mut first = None;
            for (graph_id, name) in listed {
                let id = repo::carddav::upsert_addressbook(
                    &tx,
                    account_id,
                    &format!("graph:{graph_id}"),
                    Some(&name),
                    true,
                )?;
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
                                match record_for(contact) {
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
        assert!(record_for(&contact).is_none());
        contact.emails = vec!["Ann@Example.org".into(), "second@example.org".into()];
        let record = record_for(&contact).unwrap();
        assert_eq!(record.email, "ann@example.org");
        assert_eq!(record.name, "Ann Lee");
    }
}
