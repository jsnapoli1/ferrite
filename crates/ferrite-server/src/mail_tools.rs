//! MCP tools for webmail (scope `mail`). Reading is read-only; drafting, sending, marking and
//! moving change mail, so the assistant only uses them when the user allowed changes.

use crate::tools::UserCtx;
use ferrite_core::mail::Account;
use ferrite_core::mail::compose::{Compose, ReplyMode};
use ferrite_core::mail::store::{FlagAction, MessageSummary};
use ferrite_core::{Core, Error};
use ferrite_mcp::{Tool, ToolResult};
use serde_json::{Value, json};
use std::sync::OnceLock;

/// Body text returned per message.
const MAX_TEXT: usize = 20_000;

pub fn definitions() -> &'static Vec<Tool> {
    static DEFS: OnceLock<Vec<Tool>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let account = ("account", "string", "Account id or email address (default: your first account)", false);
        let recipients = [
            ("to", "string[]", "Recipients (\"Name <a@b.c>\" or a@b.c)", false),
            ("cc", "string[]", "Cc recipients", false),
            ("bcc", "string[]", "Bcc recipients", false),
            ("subject", "string", "Subject (for replies: default \"Re: …\")", false),
            ("text", "string", "Plain-text body (for replies, written above the quoted original)", true),
            (
                "reply_to",
                "string",
                "Message id to reply to (threads the reply; recipients default to the sender)",
                false,
            ),
            ("reply_all", "boolean", "With reply_to: also copy everyone else on the original", false),
            account,
        ];
        vec![
            Tool::new(
                "mail_accounts",
                "Mail accounts",
                "Your connected mail accounts with their folders and unread counts.",
                &[],
            )
            .read_only(),
            Tool::new(
                "mail_list",
                "List mail",
                "Messages in a mail folder, newest first.",
                &[
                    account,
                    (
                        "folder",
                        "string",
                        "Folder name, id or role (inbox, sent, drafts, archive, trash, junk); default inbox",
                        false,
                    ),
                    ("unread_only", "boolean", "Only unread messages", false),
                    ("limit", "integer", "How many (default 20, max 100)", false),
                ],
            )
            .read_only(),
            Tool::new(
                "mail_read",
                "Read mail",
                "Read a message (headers, text, attachment names), or its whole conversation.",
                &[
                    ("message_id", "string", "Message id from mail_list or mail_search", true),
                    ("thread", "boolean", "Return every message of the conversation", false),
                ],
            )
            .read_only(),
            Tool::new(
                "mail_search",
                "Search mail",
                "Search your mail (cached messages and the mail server).",
                &[
                    ("query", "string", "Words to look for in senders, recipients, subjects and text", true),
                    account,
                    ("folder", "string", "Limit to one folder (name, id or role)", false),
                ],
            )
            .read_only(),
            Tool::new(
                "mail_draft",
                "Draft mail",
                "Save a message in the Drafts folder for the user to review and send.",
                &recipients,
            ),
            Tool::new(
                "mail_send",
                "Send mail",
                "Send a message from one of your mail accounts (a copy is kept in Sent).",
                &recipients,
            ),
            Tool::new(
                "mail_mark",
                "Mark mail",
                "Mark messages read or unread, flagged or unflagged.",
                &[
                    ("message_ids", "string[]", "Message ids", true),
                    ("as", "string", "read, unread, flagged or unflagged", true),
                ],
            ),
            Tool::new(
                "mail_move",
                "Move mail",
                "Move messages to another folder of their account (e.g. archive).",
                &[
                    ("message_ids", "string[]", "Message ids", true),
                    ("folder", "string", "Destination folder name, id or role (archive, inbox, junk, …)", true),
                ],
            ),
            Tool::new(
                "mail_delete",
                "Delete mail",
                "Move messages to Trash (messages already in Trash are deleted for good).",
                &[("message_ids", "string[]", "Message ids", true)],
            )
            .destructive(),
        ]
    })
}

fn s<'a>(a: &'a Value, k: &str) -> Result<&'a str, Error> {
    a.get(k)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::bad(format!("`{k}` is required")))
}

fn opt<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn strings(a: &Value, k: &str) -> Vec<String> {
    a.get(k)
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn summary(m: &MessageSummary) -> Value {
    json!({
        "id": m.id, "date": m.date, "from": m.from().display(), "subject": m.subject, "preview": m.preview,
        "unread": !m.seen, "flagged": m.flagged, "attachments": m.has_attachments, "thread_size": m.thread_size,
    })
}

async fn account(core: &Core, uid: &str, which: Option<&str>) -> Result<Account, Error> {
    let accounts = core.mail_accounts(uid).await?;
    match which {
        Some(w) => accounts
            .into_iter()
            .find(|a| a.id == w || a.email.eq_ignore_ascii_case(w))
            .ok_or_else(|| Error::bad(format!("no mail account {w:?}"))),
        None => accounts
            .into_iter()
            .next()
            .ok_or_else(|| Error::bad("no mail account is connected; add one in Mail first")),
    }
}

/// The compose fields from tool arguments (replies start from the quoted original).
async fn compose(core: &Core, uid: &str, a: &Value) -> Result<(String, Compose), Error> {
    let text = s(a, "text")?;
    let (account_id, mut c) = match opt(a, "reply_to") {
        Some(parent) => {
            let mode = if a.get("reply_all").and_then(Value::as_bool).unwrap_or(false) {
                ReplyMode::ReplyAll
            } else {
                ReplyMode::Reply
            };
            let t = core.mail_compose_template(uid, parent, mode).await?;
            let mut c = t.compose;
            c.text = format!("{text}{}", c.text);
            (t.account_id, c)
        }
        None => (
            account(core, uid, opt(a, "account")).await?.id,
            Compose {
                text: text.to_string(),
                ..Default::default()
            },
        ),
    };
    for (k, field) in [("to", &mut c.to), ("cc", &mut c.cc), ("bcc", &mut c.bcc)] {
        let v = strings(a, k);
        if !v.is_empty() {
            *field = v;
        }
    }
    if let Some(sub) = opt(a, "subject") {
        c.subject = sub.to_string();
    }
    Ok((account_id, c))
}

pub async fn dispatch(core: &Core, ctx: &UserCtx, name: &str, a: &Value) -> Result<ToolResult, Error> {
    let uid = ctx.user.id.as_str();
    match name {
        "mail_accounts" => {
            let mut out = Vec::new();
            for acc in core.mail_accounts(uid).await? {
                let folders: Vec<Value> = core
                    .mail_folders(uid, &acc.id)
                    .await?
                    .iter()
                    .filter(|f| f.selectable)
                    .map(|f| json!({"id": f.id, "name": f.name, "role": f.role, "total": f.total, "unread": f.unread}))
                    .collect();
                out.push(json!({"id": acc.id, "email": acc.email, "name": acc.name, "folders": folders, "last_error": acc.last_error}));
            }
            Ok(ToolResult::json(json!({"accounts": out})))
        }
        "mail_list" => {
            let acc = account(core, uid, opt(a, "account")).await?;
            let folder = core.find_mail_folder(uid, &acc.id, opt(a, "folder").unwrap_or("inbox")).await?;
            let unread = a.get("unread_only").and_then(Value::as_bool).unwrap_or(false);
            let limit = a.get("limit").and_then(Value::as_i64).unwrap_or(20).clamp(1, 100);
            let page = core.mail_messages(uid, &folder.id, None, unread, limit).await?;
            Ok(ToolResult::json(json!({
                "account": acc.email, "folder": folder.name, "unread": folder.unread, "total": folder.total,
                "messages": page.messages.iter().map(summary).collect::<Vec<_>>(),
            })))
        }
        "mail_read" => {
            let id = s(a, "message_id")?;
            let ids: Vec<String> = if a.get("thread").and_then(Value::as_bool).unwrap_or(false) {
                core.mail_thread(uid, id).await?.into_iter().map(|m| m.id).collect()
            } else {
                vec![id.to_string()]
            };
            let mut out = Vec::new();
            for id in ids {
                let v = core.mail_message(uid, &id).await?;
                let m = &v.summary;
                let mut text = v.text.clone();
                if text.len() > MAX_TEXT {
                    let mut end = MAX_TEXT;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                    text.push_str("\n…[truncated]");
                }
                out.push(json!({
                    "id": m.id, "date": m.date, "from": m.from().display(),
                    "to": m.to_addrs.iter().map(|x| x.display()).collect::<Vec<_>>(),
                    "cc": m.cc_addrs.iter().map(|x| x.display()).collect::<Vec<_>>(),
                    "subject": m.subject, "text": text, "unread": !m.seen, "folder": v.folder_role,
                    "attachments": v.attachments.iter().filter(|x| !x.inline).map(|x| json!({"name": x.name, "type": x.mime, "size": x.size})).collect::<Vec<_>>(),
                }));
            }
            Ok(ToolResult::json(json!({"messages": out})))
        }
        "mail_search" => {
            let acc = match opt(a, "account") {
                Some(w) => Some(account(core, uid, Some(w)).await?),
                None => None,
            };
            let folder = match (&acc, opt(a, "folder")) {
                (Some(acc), Some(f)) => Some(core.find_mail_folder(uid, &acc.id, f).await?.id),
                (None, Some(f)) => Some(core.find_mail_folder(uid, &account(core, uid, None).await?.id, f).await?.id),
                _ => None,
            };
            let found = core
                .mail_search(uid, s(a, "query")?, acc.as_ref().map(|x| x.id.as_str()), folder.as_deref(), 30)
                .await?;
            Ok(ToolResult::json(json!({"messages": found.iter().map(summary).collect::<Vec<_>>()})))
        }
        "mail_draft" => {
            let (account_id, c) = compose(core, uid, a).await?;
            let id = core.mail_save_draft(uid, &account_id, &c).await?;
            Ok(ToolResult::json(json!({"draft_id": id, "saved_in": "Drafts"})))
        }
        "mail_send" => {
            let (account_id, c) = compose(core, uid, a).await?;
            let message_id = core.mail_send(uid, &account_id, &c).await?;
            Ok(ToolResult::json(json!({"sent": true, "message_id": message_id, "to": c.to, "cc": c.cc})))
        }
        "mail_mark" => {
            let ids = strings(a, "message_ids");
            let action = match s(a, "as")? {
                "read" => FlagAction::Read,
                "unread" => FlagAction::Unread,
                "flagged" | "flag" => FlagAction::Flag,
                "unflagged" | "unflag" => FlagAction::Unflag,
                other => return Err(Error::bad(format!("unknown mark {other:?}; use read, unread, flagged or unflagged"))),
            };
            core.mail_set_flags(uid, &ids, action).await?;
            Ok(ToolResult::text(format!("Marked {} message(s).", ids.len())))
        }
        "mail_move" => {
            let ids = strings(a, "message_ids");
            let first = ids.first().ok_or_else(|| Error::bad("`message_ids` is required"))?;
            let folder = s(a, "folder")?;
            if folder.eq_ignore_ascii_case("archive") {
                core.mail_archive(uid, &ids).await?;
                return Ok(ToolResult::text(format!("Archived {} message(s).", ids.len())));
            }
            let (acc, _) = core.message_row(uid, first).await?;
            let dest = core.find_mail_folder(uid, &acc.id, folder).await?;
            core.mail_move(uid, &ids, &dest.id).await?;
            Ok(ToolResult::text(format!("Moved {} message(s) to {}.", ids.len(), dest.name)))
        }
        "mail_delete" => {
            let ids = strings(a, "message_ids");
            core.mail_delete(uid, &ids).await?;
            Ok(ToolResult::text(format!("Deleted {} message(s).", ids.len())))
        }
        other => Err(Error::bad(format!("unknown tool {other}"))),
    }
}
