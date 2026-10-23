//! MCP tools for mail privacy lists (scope `mail`): trusted senders (remote images load
//! without asking) and internal addresses (everything else gets an "external" warning).
//! Listing is read-only; editing changes the user's settings.

use crate::tools::UserCtx;
use ferrite_core::mail::trust::List;
use ferrite_core::{Core, Error};
use ferrite_mcp::{Tool, ToolResult};
use serde_json::{Value, json};
use std::sync::OnceLock;

pub fn definitions() -> &'static Vec<Tool> {
    static DEFS: OnceLock<Vec<Tool>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let edit = [
            ("add", "string[]", "Addresses (ada@example.org) or whole domains (example.org) to add", false),
            ("remove", "string[]", "Addresses or domains to remove", false),
        ];
        vec![
            Tool::new(
                "mail_trusted_senders",
                "Trusted mail senders",
                "Senders and domains whose remote images are shown without asking.",
                &[],
            )
            .read_only(),
            Tool::new(
                "mail_trusted_senders_edit",
                "Edit trusted senders",
                "Add or remove trusted senders (an address or a whole domain): their remote images load without asking.",
                &edit,
            ),
            Tool::new(
                "mail_internal_addresses",
                "Internal mail addresses",
                "Addresses and domains considered internal, and whether external senders and recipients are flagged.",
                &[],
            )
            .read_only(),
            Tool::new(
                "mail_internal_addresses_edit",
                "Edit internal addresses",
                "Add or remove internal addresses or domains; optionally turn the external warnings on or off.",
                &[edit[0], edit[1], ("warn", "boolean", "Flag senders and recipients outside the list", false)],
            ),
        ]
    })
}

pub fn handles(name: &str) -> bool {
    definitions().iter().any(|t| t.name == name)
}

fn strings(a: &Value, k: &str) -> Vec<String> {
    a.get(k)
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

async fn list(core: &Core, uid: &str, which: List) -> Result<Value, Error> {
    let entries: Vec<Value> = core
        .mail_list_entries(uid, which)
        .await?
        .into_iter()
        .map(|e| json!({"kind": e.kind, "value": e.value}))
        .collect();
    Ok(json!(entries))
}

async fn apply(core: &Core, uid: &str, which: List, a: &Value) -> Result<(), Error> {
    for v in strings(a, "add") {
        core.mail_add_entry(uid, which, None, &v).await?;
    }
    for v in strings(a, "remove") {
        match core.mail_remove_entry(uid, which, &v).await {
            Ok(()) | Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub async fn dispatch(core: &Core, ctx: &UserCtx, name: &str, a: &Value) -> Result<ToolResult, Error> {
    let uid = ctx.user.id.as_str();
    match name {
        "mail_trusted_senders" => Ok(ToolResult::json(json!({"trusted_senders": list(core, uid, List::Trusted).await?}))),
        "mail_trusted_senders_edit" => {
            apply(core, uid, List::Trusted, a).await?;
            Ok(ToolResult::json(json!({"trusted_senders": list(core, uid, List::Trusted).await?})))
        }
        "mail_internal_addresses" => Ok(ToolResult::json(json!({
            "warn": core.mail_security_settings(uid).await?.internal_enabled,
            "internal_addresses": list(core, uid, List::Internal).await?,
        }))),
        "mail_internal_addresses_edit" => {
            apply(core, uid, List::Internal, a).await?;
            if let Some(w) = a.get("warn").and_then(Value::as_bool) {
                core.mail_update_security_settings(
                    uid,
                    &ferrite_core::mail::trust::SettingsPatch {
                        internal_enabled: Some(w),
                        ..Default::default()
                    },
                )
                .await?;
            }
            Ok(ToolResult::json(json!({
                "warn": core.mail_security_settings(uid).await?.internal_enabled,
                "internal_addresses": list(core, uid, List::Internal).await?,
            })))
        }
        other => Err(Error::bad(format!("unknown tool {other}"))),
    }
}
