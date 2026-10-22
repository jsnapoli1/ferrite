//! Notifications (per user, pushed live) and the activity stream.

use crate::util::now;
use crate::{Core, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Notification {
    pub id: i64,
    pub user_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub link: String,
    pub read: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Activity {
    pub id: i64,
    pub user_id: String,
    pub actor_id: Option<String>,
    pub verb: String,
    pub object_kind: String,
    pub object_id: String,
    pub summary: String,
    pub created_at: DateTime<Utc>,
}

impl Core {
    pub async fn notify(&self, user_id: &str, kind: &str, title: &str, body: &str, link: &str) -> Result<()> {
        let id: i64 = sqlx::query_scalar("INSERT INTO notifications (user_id, kind, title, body, link, created_at) VALUES (?, ?, ?, ?, ?, ?) RETURNING id")
            .bind(user_id)
            .bind(kind)
            .bind(title)
            .bind(body)
            .bind(link)
            .bind(now())
            .fetch_one(&self.db)
            .await?;
        self.hub.publish(
            [user_id],
            &json!({"type": "notification", "notification": {"id": id, "kind": kind, "title": title, "body": body, "link": link}}),
        );
        self.push_notification(id, user_id, kind, title, body, link);
        Ok(())
    }

    pub async fn notifications(&self, user_id: &str, unread_only: bool, limit: i64) -> Result<Vec<Notification>> {
        Ok(sqlx::query_as(
            "SELECT id, user_id, kind, title, body, link, read, created_at FROM notifications
             WHERE user_id = ? AND (read = 0 OR ? = 0) ORDER BY id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(unread_only)
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }

    /// Marks one notification (or all when `id` is None) as read.
    pub async fn mark_notifications_read(&self, user_id: &str, id: Option<i64>) -> Result<()> {
        match id {
            Some(id) => {
                sqlx::query("UPDATE notifications SET read = 1 WHERE user_id = ? AND id = ?")
                    .bind(user_id)
                    .bind(id)
                    .execute(&self.db)
                    .await?
            }
            None => {
                sqlx::query("UPDATE notifications SET read = 1 WHERE user_id = ?")
                    .bind(user_id)
                    .execute(&self.db)
                    .await?
            }
        };
        Ok(())
    }

    pub async fn record_activity(&self, users: &[String], actor: Option<&str>, verb: &str, object_kind: &str, object_id: &str, summary: &str) -> Result<()> {
        let t = now();
        let mut tx = self.db.begin().await?;
        for u in users {
            sqlx::query("INSERT INTO activity (user_id, actor_id, verb, object_kind, object_id, summary, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(u)
                .bind(actor)
                .bind(verb)
                .bind(object_kind)
                .bind(object_id)
                .bind(summary)
                .bind(t)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn activity(&self, user_id: &str, before: Option<i64>, limit: i64) -> Result<Vec<Activity>> {
        Ok(sqlx::query_as(
            "SELECT id, user_id, actor_id, verb, object_kind, object_id, summary, created_at FROM activity
             WHERE user_id = ? AND id < ? ORDER BY id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(before.unwrap_or(i64::MAX))
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }
}
