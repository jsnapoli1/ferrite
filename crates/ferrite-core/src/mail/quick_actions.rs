//! Quick actions: user-defined one-click sequences per account (like Nextcloud Mail's), e.g.
//! "mark read, tag To Do, move to Projects". Moving or deleting ends a sequence, since the
//! messages are no longer where the other steps would find them.

use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use serde::{Deserialize, Serialize};

pub const MAX_STEPS: usize = 10;
pub const MAX_ACTIONS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    Read,
    Unread,
    /// Favourite (\Flagged).
    Flag,
    /// The Important tag ($label1).
    Important,
    /// Junk (moves to the Junk folder when the account says so).
    Spam,
    Tag {
        keyword: String,
    },
    Move {
        folder_id: String,
    },
    Delete,
}

impl Step {
    fn ends(&self) -> bool {
        matches!(self, Step::Move { .. } | Step::Delete | Step::Spam)
    }
}

/// Checks a sequence: 1 to [`MAX_STEPS`] steps, valid keywords, and a move, delete or spam
/// step only at the end.
pub fn validate(steps: &[Step]) -> Result<()> {
    if steps.is_empty() {
        return Err(Error::bad("a quick action needs at least one step"));
    }
    if steps.len() > MAX_STEPS {
        return Err(Error::bad(format!("a quick action has at most {MAX_STEPS} steps")));
    }
    for (i, s) in steps.iter().enumerate() {
        if s.ends() && i + 1 != steps.len() {
            return Err(Error::bad("moving, deleting or marking as spam must be the last step"));
        }
        if let Step::Tag { keyword } = s {
            if !super::tags::valid_keyword(keyword) {
                return Err(Error::bad(format!("{keyword:?} is not a tag")));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct QuickAction {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub steps: Vec<Step>,
    pub position: i64,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    account_id: String,
    name: String,
    steps: String,
    position: i64,
}

impl From<Row> for QuickAction {
    fn from(r: Row) -> Self {
        QuickAction {
            id: r.id,
            account_id: r.account_id,
            name: r.name,
            steps: serde_json::from_str(&r.steps).unwrap_or_default(),
            position: r.position,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuickActionInput {
    pub name: String,
    pub steps: Vec<Step>,
}

impl Core {
    pub async fn mail_quick_actions(&self, user_id: &str, account_id: Option<&str>) -> Result<Vec<QuickAction>> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, account_id, name, steps, position FROM mail_quick_actions WHERE user_id = ? AND (? IS NULL OR account_id = ?) ORDER BY position, created_at",
        )
        .bind(user_id)
        .bind(account_id)
        .bind(account_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(QuickAction::from).collect())
    }

    async fn quick_action(&self, user_id: &str, id: &str) -> Result<QuickAction> {
        let r: Row = sqlx::query_as("SELECT id, account_id, name, steps, position FROM mail_quick_actions WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(r.into())
    }

    async fn check_steps(&self, user_id: &str, account_id: &str, input: &QuickActionInput) -> Result<String> {
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err(Error::bad("give the quick action a name (at most 60 characters)"));
        }
        validate(&input.steps)?;
        for s in &input.steps {
            if let Step::Move { folder_id } = s {
                let (a, f) = self.mail_folder(user_id, folder_id).await?;
                if a.id != account_id || !f.selectable {
                    return Err(Error::bad("move to a folder of the same account"));
                }
            }
        }
        Ok(name.to_string())
    }

    pub async fn create_mail_quick_action(&self, user_id: &str, account_id: &str, input: &QuickActionInput) -> Result<QuickAction> {
        let a = self.mail_account(user_id, account_id).await?;
        let name = self.check_steps(user_id, &a.id, input).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_quick_actions WHERE account_id = ?")
            .bind(&a.id)
            .fetch_one(&self.db)
            .await?;
        if count >= MAX_ACTIONS {
            return Err(Error::bad(format!("at most {MAX_ACTIONS} quick actions per account")));
        }
        let id = new_id();
        sqlx::query("INSERT INTO mail_quick_actions (id, user_id, account_id, name, steps, position, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(&a.id)
            .bind(&name)
            .bind(serde_json::to_string(&input.steps)?)
            .bind(count)
            .bind(now())
            .execute(&self.db)
            .await?;
        self.quick_action(user_id, &id).await
    }

    pub async fn update_mail_quick_action(&self, user_id: &str, id: &str, input: &QuickActionInput) -> Result<QuickAction> {
        let qa = self.quick_action(user_id, id).await?;
        let name = self.check_steps(user_id, &qa.account_id, input).await?;
        sqlx::query("UPDATE mail_quick_actions SET name = ?, steps = ? WHERE id = ?")
            .bind(&name)
            .bind(serde_json::to_string(&input.steps)?)
            .bind(id)
            .execute(&self.db)
            .await?;
        self.quick_action(user_id, id).await
    }

    pub async fn delete_mail_quick_action(&self, user_id: &str, id: &str) -> Result<()> {
        self.quick_action(user_id, id).await?;
        sqlx::query("DELETE FROM mail_quick_actions WHERE id = ?").bind(id).execute(&self.db).await?;
        Ok(())
    }

    /// Runs a quick action on messages of its account, step by step.
    pub async fn run_mail_quick_action(&self, user_id: &str, id: &str, ids: &[String]) -> Result<()> {
        let qa = self.quick_action(user_id, id).await?;
        for mid in ids {
            let (a, _) = self.message_row(user_id, mid).await?;
            if a.id != qa.account_id {
                return Err(Error::bad("this quick action belongs to another account"));
            }
        }
        use super::store::FlagAction;
        for step in &qa.steps {
            match step {
                Step::Read => self.mail_set_flags(user_id, ids, FlagAction::Read).await?,
                Step::Unread => self.mail_set_flags(user_id, ids, FlagAction::Unread).await?,
                Step::Flag => self.mail_set_flags(user_id, ids, FlagAction::Flag).await?,
                Step::Important => self.mail_set_keyword(user_id, ids, super::tags::IMPORTANT, true).await?,
                Step::Tag { keyword } => self.mail_set_keyword(user_id, ids, keyword, true).await?,
                Step::Spam => self.mail_mark_junk(user_id, ids, true).await?,
                Step::Move { folder_id } => self.mail_move(user_id, ids, folder_id).await?,
                Step::Delete => self.mail_delete(user_id, ids).await?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences() {
        let tag = |k: &str| Step::Tag { keyword: k.into() };
        let mv = Step::Move { folder_id: "f".into() };
        assert!(validate(&[Step::Read, tag("$label4"), mv.clone()]).is_ok());
        assert!(validate(&[Step::Delete]).is_ok());
        assert!(validate(&[]).is_err());
        assert!(validate(&[mv.clone(), Step::Read]).is_err(), "a move ends the sequence");
        assert!(validate(&[Step::Delete, mv.clone()]).is_err());
        assert!(validate(&[Step::Spam, Step::Read]).is_err());
        assert!(validate(&[tag("not valid")]).is_err());
        assert!(validate(&vec![Step::Read; 11]).is_err());
        let parsed: Vec<Step> = serde_json::from_str(r#"[{"type":"unread"},{"type":"tag","keyword":"$label2"},{"type":"move","folder_id":"x"}]"#).unwrap();
        assert_eq!(parsed[1], tag("$label2"));
        assert!(serde_json::from_str::<Vec<Step>>(r#"[{"type":"explode"}]"#).is_err());
    }
}
