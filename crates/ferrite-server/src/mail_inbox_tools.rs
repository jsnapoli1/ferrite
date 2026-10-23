//! MCP tools for inbox management (scope `mail`): tags, important mail, snoozing, folders and
//! marking a folder read. Listing is read-only; the rest changes mail, so the assistant only
//! uses them when the user allowed changes. Deleting a folder is destructive.

use crate::mail_tools::{account, opt, s, strings, summary};
use crate::tools::UserCtx;
use ferrite_core::mail::tags::{IMPORTANT, Tag};
use ferrite_core::mail::views::View;
use ferrite_core::{Core, Error};
use ferrite_mcp::{Tool, ToolResult};
use serde_json::{Value, json};

pub fn definitions() -> Vec<Tool> {
    let account = ("account", "string", "Account id or email address (default: your first account)", false);
    let ids = ("message_ids", "string[]", "Message ids (from mail_list, mail_search or mail_important)", true);
    let tag = ("tag", "string", "Tag name (e.g. Important, Work, To Do) or its keyword ($label1 …)", true);
    vec![
        Tool::new(
            "mail_tags",
            "Mail tags",
            "Your mail tags (name, colour and the IMAP keyword stored on messages).",
            &[],
        )
        .read_only(),
        Tool::new(
            "mail_tag",
            "Tag mail",
            "Add a tag to messages (Important marks them important for the priority inbox).",
            &[ids, tag],
        ),
        Tool::new("mail_untag", "Untag mail", "Remove a tag from messages.", &[ids, tag]),
        Tool::new(
            "mail_important",
            "Important mail",
            "Messages in your inboxes marked important (by you or learned from how you treat each sender), newest first.",
            &[
                ("unread_only", "boolean", "Only unread messages", false),
                ("limit", "integer", "How many (default 20, max 100)", false),
            ],
        )
        .read_only(),
        Tool::new(
            "mail_snooze",
            "Snooze mail",
            "Move messages to the Snoozed folder until a time; they come back to their folder, unread, then.",
            &[
                ids,
                (
                    "until",
                    "string",
                    "When they come back, RFC 3339 with an offset (e.g. 2026-11-02T08:00:00+01:00)",
                    true,
                ),
            ],
        ),
        Tool::new(
            "mail_folder_create",
            "Create mail folder",
            "Create a mail folder (optionally inside another one).",
            &[
                account,
                ("name", "string", "Folder name", true),
                ("parent", "string", "Parent folder name, id or role (default: top level)", false),
            ],
        ),
        Tool::new(
            "mail_folder_rename",
            "Rename mail folder",
            "Rename a mail folder or move it into another one; its subfolders and messages move along.",
            &[
                account,
                ("folder", "string", "Folder name or id", true),
                ("new_name", "string", "New name (omit to only move it)", false),
                (
                    "parent",
                    "string",
                    "New parent folder name or id; \"\" for the top level (omit to keep it)",
                    false,
                ),
            ],
        ),
        Tool::new(
            "mail_folder_delete",
            "Delete mail folder",
            "Delete a mail folder with its subfolders and all their messages, on the mail server.",
            &[account, ("folder", "string", "Folder name or id", true)],
        )
        .destructive(),
        Tool::new(
            "mail_mark_all_read",
            "Mark folder read",
            "Mark every message in a mail folder read.",
            &[account, ("folder", "string", "Folder name, id or role (default inbox)", false)],
        ),
    ]
}

async fn find_tag(core: &Core, uid: &str, which: &str) -> Result<Tag, Error> {
    let w = which.trim();
    core.mail_tags(uid)
        .await?
        .into_iter()
        .find(|t| t.keyword.eq_ignore_ascii_case(w) || t.name.eq_ignore_ascii_case(w))
        .ok_or_else(|| Error::bad(format!("no tag {w:?}; mail_tags lists them")))
}

pub async fn dispatch(core: &Core, ctx: &UserCtx, name: &str, a: &Value) -> Result<ToolResult, Error> {
    let uid = ctx.user.id.as_str();
    match name {
        "mail_tags" => {
            let tags: Vec<Value> = core
                .mail_tags(uid)
                .await?
                .iter()
                .map(|t| json!({"name": t.name, "keyword": t.keyword, "color": t.color, "default": t.is_default}))
                .collect();
            Ok(ToolResult::json(json!({"tags": tags})))
        }
        "mail_tag" | "mail_untag" => {
            let ids = strings(a, "message_ids");
            if ids.is_empty() {
                return Err(Error::bad("`message_ids` is required"));
            }
            let tag = find_tag(core, uid, s(a, "tag")?).await?;
            let on = name == "mail_tag";
            core.mail_set_keyword(uid, &ids, &tag.keyword, on).await?;
            Ok(ToolResult::text(format!(
                "{} “{}” {} {} message(s).",
                if on { "Added" } else { "Removed" },
                tag.name,
                if on { "to" } else { "from" },
                ids.len()
            )))
        }
        "mail_important" => {
            let unread = a.get("unread_only").and_then(Value::as_bool).unwrap_or(false);
            let limit = a.get("limit").and_then(Value::as_i64).unwrap_or(20).clamp(1, 100);
            let page = core
                .mail_view(uid, &View::Important, None, unread, ferrite_core::mail::prefs::Sort::Newest, limit)
                .await?;
            Ok(ToolResult::json(json!({
                "tag": IMPORTANT,
                "messages": page.messages.iter().map(summary).collect::<Vec<_>>(),
            })))
        }
        "mail_snooze" => {
            let ids = strings(a, "message_ids");
            if ids.is_empty() {
                return Err(Error::bad("`message_ids` is required"));
            }
            let until = chrono::DateTime::parse_from_rfc3339(s(a, "until")?)
                .map_err(|e| Error::bad(format!("`until` is not an RFC 3339 time: {e}")))?
                .with_timezone(&chrono::Utc);
            core.mail_snooze(uid, &ids, until).await?;
            Ok(ToolResult::text(format!("Snoozed {} message(s) until {}.", ids.len(), until.to_rfc3339())))
        }
        "mail_folder_create" => {
            let acc = account(core, uid, opt(a, "account")).await?;
            let parent = match opt(a, "parent") {
                Some(p) => Some(core.find_mail_folder(uid, &acc.id, p).await?.id),
                None => None,
            };
            let f = core.create_mail_folder(uid, &acc.id, s(a, "name")?, parent.as_deref()).await?;
            Ok(ToolResult::json(json!({"created": {"id": f.id, "name": f.name}})))
        }
        "mail_folder_rename" => {
            let acc = account(core, uid, opt(a, "account")).await?;
            let f = core.find_mail_folder(uid, &acc.id, s(a, "folder")?).await?;
            let parent = match a.get("parent").and_then(Value::as_str) {
                Some("") => Some(String::new()),
                Some(p) => Some(core.find_mail_folder(uid, &acc.id, p).await?.id),
                None => None,
            };
            let new_name = opt(a, "new_name");
            if new_name.is_none() && parent.is_none() {
                return Err(Error::bad("give `new_name`, `parent` or both"));
            }
            let renamed = core.rename_mail_folder(uid, &f.id, new_name, parent.as_deref()).await?;
            Ok(ToolResult::json(json!({"renamed": {"id": renamed.id, "from": f.name, "to": renamed.name}})))
        }
        "mail_folder_delete" => {
            let acc = account(core, uid, opt(a, "account")).await?;
            let f = core.find_mail_folder(uid, &acc.id, s(a, "folder")?).await?;
            core.delete_mail_folder(uid, &f.id).await?;
            Ok(ToolResult::text(format!("Deleted the folder {}.", f.name)))
        }
        "mail_mark_all_read" => {
            let acc = account(core, uid, opt(a, "account")).await?;
            let f = core.find_mail_folder(uid, &acc.id, opt(a, "folder").unwrap_or("inbox")).await?;
            core.mark_folder_read(uid, &f.id).await?;
            Ok(ToolResult::text(format!("Marked every message in {} read.", f.name)))
        }
        other => Err(Error::bad(format!("unknown tool {other}"))),
    }
}
