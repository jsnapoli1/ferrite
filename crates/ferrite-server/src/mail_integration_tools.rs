//! MCP tools for the mail integrations (scope `mail`): filters, the autoresponder, answering
//! invitations received by mail, thread summaries and unsubscribing. Listing is read-only;
//! the others change the mail server, the calendar or send mail, so the assistant uses them
//! only when the user allowed changes.

use crate::tools::UserCtx;
use ferrite_core::mail::filters::{Filter, FilterAction, FilterTest, Vacation};
use ferrite_core::{Core, Error};
use ferrite_mcp::{Tool, ToolResult};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub fn definitions() -> &'static Vec<Tool> {
    static DEFS: OnceLock<Vec<Tool>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let account = ("account", "string", "Account id or email address (default: your first account)", false);
        vec![
            Tool::new(
                "mail_filters",
                "Mail filters",
                "The Sieve filters of a mail account, in the order they run.",
                &[account],
            )
            .read_only(),
            Tool::new(
                "mail_filter_add",
                "Add mail filter",
                "Add a filter that runs on the mail server when mail arrives: if the subject, sender or recipient matches, move the message to a folder, tag it and/or stop further filters.",
                &[
                    account,
                    ("name", "string", "Filter name", true),
                    ("field", "string", "What to test: subject, from or to", true),
                    ("operator", "string", "is, contains (default) or matches (with * and ? wildcards)", false),
                    ("values", "string[]", "Values to compare with (any of them matches)", true),
                    ("folder", "string", "Move matching mail to this folder (name or role)", false),
                    ("tag", "string", "Add this tag or flag (an IMAP keyword such as $label1, or \\Flagged)", false),
                    ("stop", "boolean", "Stop processing later filters", false),
                ],
            ),
            Tool::new(
                "mail_filter_remove",
                "Remove mail filter",
                "Remove a filter (by name) from a mail account.",
                &[account, ("name", "string", "Filter name", true)],
            )
            .destructive(),
            Tool::new(
                "mail_out_of_office",
                "Out of office",
                "The autoresponder (out-of-office reply) of a mail account.",
                &[account],
            )
            .read_only(),
            Tool::new(
                "mail_out_of_office_set",
                "Set out of office",
                "Turn the autoresponder of a mail account on (with dates, subject and message) or off.",
                &[
                    account,
                    ("enabled", "boolean", "On or off", true),
                    ("start", "string", "First moment, RFC 3339 (default: now)", false),
                    ("end", "string", "Last moment, RFC 3339 (optional)", false),
                    ("subject", "string", "Subject of the reply (default \"Re: ${subject}\")", false),
                    ("message", "string", "The reply text", false),
                    ("days", "integer", "Answer each sender at most once per this many days (default 4)", false),
                ],
            ),
            Tool::new(
                "mail_invitation_respond",
                "Answer invitation",
                "Accept, tentatively accept or decline a meeting invitation received by mail: updates your calendar and sends the reply to the organizer.",
                &[
                    ("message_id", "string", "Message id of the invitation (from mail_list or mail_search)", true),
                    ("response", "string", "accepted, tentative or declined", true),
                    ("comment", "string", "A note for the organizer", false),
                ],
            ),
            Tool::new(
                "mail_summarize_thread",
                "Summarize mail thread",
                "An AI summary of a mail conversation (three or more messages), when AI is available for mail.",
                &[("message_id", "string", "Any message id of the conversation", true)],
            )
            .read_only(),
            Tool::new(
                "mail_unsubscribe",
                "Unsubscribe",
                "Unsubscribe from the mailing list a message came from (one-click link or unsubscribe mail).",
                &[("message_id", "string", "Message id", true)],
            ),
        ]
    })
}

pub fn handles(name: &str) -> bool {
    definitions().iter().any(|t| t.name == name)
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

async fn account_id(core: &Core, uid: &str, which: Option<&str>) -> Result<String, Error> {
    let accounts = core.mail_accounts(uid).await?;
    match which {
        Some(w) => accounts
            .into_iter()
            .find(|a| a.id == w || a.email.eq_ignore_ascii_case(w))
            .map(|a| a.id)
            .ok_or_else(|| Error::bad(format!("no mail account {w:?}"))),
        None => accounts
            .into_iter()
            .next()
            .map(|a| a.id)
            .ok_or_else(|| Error::bad("no mail account is connected; add one in Mail first")),
    }
}

fn time(a: &Value, k: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>, Error> {
    match opt(a, k) {
        None => Ok(None),
        Some(v) => chrono::DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.with_timezone(&chrono::Utc)))
            .map_err(|_| Error::bad(format!("`{k}` must be RFC 3339, e.g. 2026-11-05T00:00:00Z"))),
    }
}

pub async fn dispatch(core: &Core, ctx: &UserCtx, name: &str, a: &Value) -> Result<ToolResult, Error> {
    let uid = ctx.user.id.as_str();
    match name {
        "mail_filters" => {
            let acc = account_id(core, uid, opt(a, "account")).await?;
            Ok(ToolResult::json(json!({"filters": core.mail_filters(uid, &acc).await?})))
        }
        "mail_filter_add" => {
            let acc = account_id(core, uid, opt(a, "account")).await?;
            let mut list = core.mail_filters(uid, &acc).await?;
            let values: Vec<String> = a
                .get("values")
                .and_then(Value::as_array)
                .map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut actions = Vec::new();
            if let Some(folder) = opt(a, "folder") {
                let f = core.find_mail_folder(uid, &acc, folder).await?;
                actions.push(FilterAction {
                    kind: "fileinto".into(),
                    mailbox: Some(ferrite_core::mail::parse::decode_mailbox_name(&f.name)),
                    flag: None,
                    recipient: None,
                });
            }
            if let Some(tag) = opt(a, "tag") {
                actions.push(FilterAction {
                    kind: "addflag".into(),
                    mailbox: None,
                    flag: Some(tag.to_string()),
                    recipient: None,
                });
            }
            if a.get("stop").and_then(Value::as_bool).unwrap_or(false) {
                actions.push(FilterAction {
                    kind: "stop".into(),
                    mailbox: None,
                    flag: None,
                    recipient: None,
                });
            }
            let name = s(a, "name")?.trim().to_string();
            if list.iter().any(|f| f.name.eq_ignore_ascii_case(&name)) {
                return Err(Error::bad(format!("a filter named {name:?} exists already")));
            }
            list.push(Filter {
                name,
                enable: true,
                operator: "allof".into(),
                tests: vec![FilterTest {
                    field: s(a, "field")?.to_ascii_lowercase(),
                    operator: opt(a, "operator").unwrap_or("contains").to_ascii_lowercase(),
                    values,
                }],
                actions,
                priority: 0,
            });
            let saved = core.set_mail_filters(uid, &acc, &list).await?;
            Ok(ToolResult::json(json!({"filters": saved})))
        }
        "mail_filter_remove" => {
            let acc = account_id(core, uid, opt(a, "account")).await?;
            let name = s(a, "name")?;
            let list = core.mail_filters(uid, &acc).await?;
            let before = list.len();
            let kept: Vec<Filter> = list.into_iter().filter(|f| !f.name.eq_ignore_ascii_case(name)).collect();
            if kept.len() == before {
                return Err(Error::bad(format!("no filter named {name:?}")));
            }
            core.set_mail_filters(uid, &acc, &kept).await?;
            Ok(ToolResult::text(format!("Removed the filter “{name}”.")))
        }
        "mail_out_of_office" => {
            let acc = account_id(core, uid, opt(a, "account")).await?;
            Ok(ToolResult::json(json!({"out_of_office": core.mail_vacation(uid, &acc).await?})))
        }
        "mail_out_of_office_set" => {
            let acc = account_id(core, uid, opt(a, "account")).await?;
            let cur = core.mail_vacation(uid, &acc).await?.vacation;
            let enabled = a.get("enabled").and_then(Value::as_bool).ok_or_else(|| Error::bad("`enabled` is required"))?;
            let v = Vacation {
                enabled,
                start: if enabled {
                    time(a, "start")?.or(Some(ferrite_core::util::now()))
                } else {
                    None
                },
                end: if enabled { time(a, "end")? } else { None },
                subject: opt(a, "subject").map(String::from).unwrap_or(cur.subject),
                message: opt(a, "message").map(String::from).unwrap_or(cur.message),
                days: a.get("days").and_then(Value::as_i64).unwrap_or(cur.days),
                ..cur
            };
            Ok(ToolResult::json(json!({"out_of_office": core.set_mail_vacation(uid, &acc, &v).await?})))
        }
        "mail_invitation_respond" => {
            let ps = ferrite_core::scheduling::parse_response(s(a, "response")?)?;
            let id = s(a, "message_id")?;
            let out = core.mail_imip_respond(uid, id, ps, None, opt(a, "comment")).await?;
            Ok(ToolResult::json(json!({"outcome": out})))
        }
        "mail_summarize_thread" => Ok(ToolResult::json(crate::mail_ai::summarize_thread(core, &ctx.user, s(a, "message_id")?).await?)),
        "mail_unsubscribe" => {
            let id = s(a, "message_id")?;
            let info = core
                .mail_unsubscribe_info(uid, id)
                .await?
                .ok_or_else(|| Error::bad("this message is not from a mailing list"))?;
            if let Some(url) = &info.one_click {
                crate::api::mail_integrations::one_click_post(url).await?;
                return Ok(ToolResult::text(format!(
                    "Unsubscribed with the list's one-click link{}.",
                    info.list.map(|l| format!(" ({l})")).unwrap_or_default()
                )));
            }
            if info.mailto.is_some() {
                let to = core.mail_unsubscribe_by_mail(uid, id).await?;
                return Ok(ToolResult::text(format!("Sent an unsubscribe request to {to}.")));
            }
            Ok(ToolResult::json(
                json!({"done": false, "open_in_browser": info.url, "note": "this list only offers a web page to unsubscribe"}),
            ))
        }
        other => Err(Error::bad(format!("unknown tool {other}"))),
    }
}
