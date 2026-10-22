//! Boards: kanban (stacks of cards) whose cards link to files, mind maps, canvases,
//! conversations and URLs, with labels, assignees, due dates and comments.

use crate::files::perm;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Board {
    pub id: String,
    pub owner_id: String,
    pub title: String,
    pub color: String,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Stack {
    pub id: String,
    pub board_id: String,
    pub title: String,
    pub position: f64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Card {
    pub id: String,
    pub stack_id: String,
    pub board_id: String,
    pub title: String,
    pub description: String,
    pub position: f64,
    pub due_at: Option<DateTime<Utc>>,
    pub done: bool,
    pub archived: bool,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const CARD_COLS: &str = "id, stack_id, board_id, title, description, position, due_at, done, archived, created_by, created_at, updated_at";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Label {
    pub id: String,
    pub board_id: String,
    pub title: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CardLink {
    pub id: String,
    pub card_id: String,
    pub kind: String,
    pub target: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct CardComment {
    pub id: String,
    pub card_id: String,
    pub author_id: String,
    pub author_name: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MemberView {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub can_edit: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CardView {
    #[serde(flatten)]
    pub card: Card,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub links: Vec<CardLink>,
    pub comment_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StackView {
    #[serde(flatten)]
    pub stack: Stack,
    pub cards: Vec<CardView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BoardView {
    #[serde(flatten)]
    pub board: Board,
    pub can_edit: bool,
    pub stacks: Vec<StackView>,
    pub labels: Vec<Label>,
    pub members: Vec<MemberView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CardUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub due_at: Option<Option<DateTime<Utc>>>,
    pub done: Option<bool>,
    pub archived: Option<bool>,
}

impl Core {
    pub async fn board(&self, id: &str) -> Result<Board> {
        Ok(
            sqlx::query_as("SELECT id, owner_id, title, color, archived, created_at FROM boards WHERE id = ?")
                .bind(id)
                .fetch_one(&self.db)
                .await?,
        )
    }

    /// Returns the board and whether the user may edit it; NotFound when they can't see it.
    pub async fn board_access(&self, user_id: &str, board_id: &str) -> Result<(Board, bool)> {
        let board = self.board(board_id).await?;
        if board.owner_id == user_id {
            return Ok((board, true));
        }
        let m: Option<bool> = sqlx::query_scalar("SELECT can_edit FROM board_members WHERE board_id = ? AND user_id = ?")
            .bind(board_id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?;
        match m {
            Some(can_edit) => Ok((board, can_edit)),
            None => Err(Error::NotFound),
        }
    }

    async fn board_for_edit(&self, user_id: &str, board_id: &str) -> Result<Board> {
        match self.board_access(user_id, board_id).await? {
            (b, true) => Ok(b),
            _ => Err(Error::Forbidden),
        }
    }

    pub async fn accessible_board_ids(&self, user_id: &str) -> Result<Vec<String>> {
        Ok(
            sqlx::query_scalar("SELECT id FROM boards WHERE owner_id = ?1 UNION SELECT board_id FROM board_members WHERE user_id = ?1")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    async fn board_audience(&self, board: &Board) -> Result<Vec<String>> {
        let mut users: Vec<String> = sqlx::query_scalar("SELECT user_id FROM board_members WHERE board_id = ?")
            .bind(&board.id)
            .fetch_all(&self.db)
            .await?;
        users.push(board.owner_id.clone());
        Ok(users)
    }

    async fn board_changed(&self, board: &Board, what: &str) -> Result<()> {
        let aud = self.board_audience(board).await?;
        self.hub.publish(
            aud.iter().map(String::as_str),
            &json!({"type": "boards.changed", "board_id": board.id, "what": what}),
        );
        Ok(())
    }

    pub async fn create_board(&self, user_id: &str, title: &str, color: Option<&str>, stacks: Option<&[String]>) -> Result<Board> {
        let title = title.trim();
        if title.is_empty() || title.len() > 200 {
            return Err(Error::bad("board title must be 1-200 characters"));
        }
        let id = new_id();
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO boards (id, owner_id, title, color, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(title)
            .bind(color.unwrap_or("#3b82f6"))
            .bind(now())
            .execute(&mut *tx)
            .await?;
        let defaults = ["To do".to_string(), "Doing".to_string(), "Done".to_string()];
        for (i, s) in stacks.unwrap_or(&defaults).iter().enumerate() {
            sqlx::query("INSERT INTO stacks (id, board_id, title, position) VALUES (?, ?, ?, ?)")
                .bind(new_id())
                .bind(&id)
                .bind(s)
                .bind(i as f64)
                .execute(&mut *tx)
                .await?;
        }
        for (t, c) in [("Urgent", "#ef4444"), ("Idea", "#a855f7"), ("Blocked", "#f59e0b")] {
            sqlx::query("INSERT INTO board_labels (id, board_id, title, color) VALUES (?, ?, ?, ?)")
                .bind(new_id())
                .bind(&id)
                .bind(t)
                .bind(c)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.board(&id).await
    }

    pub async fn list_boards(&self, user_id: &str, include_archived: bool) -> Result<Vec<Board>> {
        Ok(sqlx::query_as(
            "SELECT id, owner_id, title, color, archived, created_at FROM boards
             WHERE (owner_id = ?1 OR id IN (SELECT board_id FROM board_members WHERE user_id = ?1)) AND (archived = 0 OR ?2)
             ORDER BY title COLLATE NOCASE",
        )
        .bind(user_id)
        .bind(include_archived)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn board_view(&self, user_id: &str, board_id: &str) -> Result<BoardView> {
        let (board, can_edit) = self.board_access(user_id, board_id).await?;
        let stacks: Vec<Stack> = sqlx::query_as("SELECT id, board_id, title, position FROM stacks WHERE board_id = ? ORDER BY position")
            .bind(board_id)
            .fetch_all(&self.db)
            .await?;
        let cards: Vec<Card> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {CARD_COLS} FROM cards WHERE board_id = ? AND archived = 0 ORDER BY position"
        )))
        .bind(board_id)
        .fetch_all(&self.db)
        .await?;
        let label_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT cl.card_id, cl.label_id FROM card_labels cl JOIN cards c ON c.id = cl.card_id WHERE c.board_id = ?")
                .bind(board_id)
                .fetch_all(&self.db)
                .await?;
        let assignee_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT ca.card_id, ca.user_id FROM card_assignees ca JOIN cards c ON c.id = ca.card_id WHERE c.board_id = ?")
                .bind(board_id)
                .fetch_all(&self.db)
                .await?;
        let links: Vec<CardLink> =
            sqlx::query_as("SELECT l.id, l.card_id, l.kind, l.target, l.title FROM card_links l JOIN cards c ON c.id = l.card_id WHERE c.board_id = ?")
                .bind(board_id)
                .fetch_all(&self.db)
                .await?;
        let comment_counts: Vec<(String, i64)> =
            sqlx::query_as("SELECT cc.card_id, COUNT(*) FROM card_comments cc JOIN cards c ON c.id = cc.card_id WHERE c.board_id = ? GROUP BY cc.card_id")
                .bind(board_id)
                .fetch_all(&self.db)
                .await?;
        let mut stack_views: Vec<StackView> = stacks.into_iter().map(|s| StackView { stack: s, cards: vec![] }).collect();
        for card in cards {
            let view = CardView {
                labels: label_rows.iter().filter(|(c, _)| *c == card.id).map(|(_, l)| l.clone()).collect(),
                assignees: assignee_rows.iter().filter(|(c, _)| *c == card.id).map(|(_, u)| u.clone()).collect(),
                links: links.iter().filter(|l| l.card_id == card.id).cloned().collect(),
                comment_count: comment_counts.iter().find(|(c, _)| *c == card.id).map(|(_, n)| *n).unwrap_or(0),
                card,
            };
            if let Some(s) = stack_views.iter_mut().find(|s| s.stack.id == view.card.stack_id) {
                s.cards.push(view);
            }
        }
        let labels: Vec<Label> = sqlx::query_as("SELECT id, board_id, title, color FROM board_labels WHERE board_id = ? ORDER BY title")
            .bind(board_id)
            .fetch_all(&self.db)
            .await?;
        let mut members: Vec<MemberView> = sqlx::query_as(
            "SELECT m.user_id, u.username, u.display_name, m.can_edit FROM board_members m JOIN users u ON u.id = m.user_id WHERE m.board_id = ? ORDER BY u.display_name",
        )
        .bind(board_id)
        .fetch_all(&self.db)
        .await?;
        let owner = self.user(&board.owner_id).await?;
        members.insert(
            0,
            MemberView {
                user_id: owner.id,
                username: owner.username,
                display_name: owner.display_name,
                can_edit: true,
            },
        );
        Ok(BoardView {
            board,
            can_edit,
            stacks: stack_views,
            labels,
            members,
        })
    }

    pub async fn update_board(&self, user_id: &str, board_id: &str, title: Option<&str>, color: Option<&str>, archived: Option<bool>) -> Result<Board> {
        let board = self.board_for_edit(user_id, board_id).await?;
        if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
            sqlx::query("UPDATE boards SET title = ? WHERE id = ?")
                .bind(t)
                .bind(board_id)
                .execute(&self.db)
                .await?;
        }
        if let Some(c) = color {
            sqlx::query("UPDATE boards SET color = ? WHERE id = ?")
                .bind(c)
                .bind(board_id)
                .execute(&self.db)
                .await?;
        }
        if let Some(a) = archived {
            sqlx::query("UPDATE boards SET archived = ? WHERE id = ?")
                .bind(a)
                .bind(board_id)
                .execute(&self.db)
                .await?;
        }
        self.board_changed(&board, "board").await?;
        self.board(board_id).await
    }

    pub async fn delete_board(&self, user_id: &str, board_id: &str) -> Result<()> {
        let board = self.board(board_id).await?;
        if board.owner_id != user_id {
            return Err(Error::Forbidden);
        }
        let aud = self.board_audience(&board).await?;
        sqlx::query("DELETE FROM fts WHERE object_kind = 'card' AND scope = ?")
            .bind(board_id)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM boards WHERE id = ?").bind(board_id).execute(&self.db).await?;
        self.hub.publish(
            aud.iter().map(String::as_str),
            &json!({"type": "boards.changed", "board_id": board_id, "what": "deleted"}),
        );
        Ok(())
    }

    pub async fn add_board_member(&self, user_id: &str, board_id: &str, username: &str, can_edit: bool) -> Result<()> {
        let board = self.board(board_id).await?;
        if board.owner_id != user_id {
            return Err(Error::Forbidden);
        }
        let u = self.user_by_name(username).await.map_err(|_| Error::bad("no such user"))?;
        if u.id == board.owner_id {
            return Ok(());
        }
        sqlx::query("INSERT INTO board_members (board_id, user_id, can_edit) VALUES (?, ?, ?) ON CONFLICT(board_id, user_id) DO UPDATE SET can_edit = excluded.can_edit")
            .bind(board_id)
            .bind(&u.id)
            .bind(can_edit)
            .execute(&self.db)
            .await?;
        let me = self.user(user_id).await?;
        self.notify(
            &u.id,
            "board",
            &format!("{} shared the board “{}” with you", me.display_name, board.title),
            "",
            &format!("/boards/{board_id}"),
        )
        .await?;
        self.board_changed(&board, "members").await
    }

    pub async fn remove_board_member(&self, user_id: &str, board_id: &str, member_id: &str) -> Result<()> {
        let board = self.board(board_id).await?;
        if board.owner_id != user_id && member_id != user_id {
            return Err(Error::Forbidden);
        }
        sqlx::query("DELETE FROM board_members WHERE board_id = ? AND user_id = ?")
            .bind(board_id)
            .bind(member_id)
            .execute(&self.db)
            .await?;
        self.board_changed(&board, "members").await
    }

    // ---- stacks --------------------------------------------------------------

    pub async fn create_stack(&self, user_id: &str, board_id: &str, title: &str) -> Result<Stack> {
        let board = self.board_for_edit(user_id, board_id).await?;
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::bad("stack title required"));
        }
        let pos: f64 = sqlx::query_scalar("SELECT CAST(COALESCE(MAX(position), -1) + 1 AS REAL) FROM stacks WHERE board_id = ?")
            .bind(board_id)
            .fetch_one(&self.db)
            .await?;
        let id = new_id();
        sqlx::query("INSERT INTO stacks (id, board_id, title, position) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(board_id)
            .bind(title)
            .bind(pos)
            .execute(&self.db)
            .await?;
        self.board_changed(&board, "stacks").await?;
        Ok(sqlx::query_as("SELECT id, board_id, title, position FROM stacks WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.db)
            .await?)
    }

    async fn stack(&self, id: &str) -> Result<Stack> {
        Ok(sqlx::query_as("SELECT id, board_id, title, position FROM stacks WHERE id = ?")
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn update_stack(&self, user_id: &str, stack_id: &str, title: Option<&str>, index: Option<usize>) -> Result<()> {
        let stack = self.stack(stack_id).await?;
        let board = self.board_for_edit(user_id, &stack.board_id).await?;
        if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
            sqlx::query("UPDATE stacks SET title = ? WHERE id = ?")
                .bind(t)
                .bind(stack_id)
                .execute(&self.db)
                .await?;
        }
        if let Some(idx) = index {
            let mut ids: Vec<String> = sqlx::query_scalar("SELECT id FROM stacks WHERE board_id = ? AND id != ? ORDER BY position")
                .bind(&board.id)
                .bind(stack_id)
                .fetch_all(&self.db)
                .await?;
            ids.insert(idx.min(ids.len()), stack_id.to_string());
            let mut tx = self.db.begin().await?;
            for (i, id) in ids.iter().enumerate() {
                sqlx::query("UPDATE stacks SET position = ? WHERE id = ?")
                    .bind(i as f64)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
        }
        self.board_changed(&board, "stacks").await
    }

    pub async fn delete_stack(&self, user_id: &str, stack_id: &str) -> Result<()> {
        let stack = self.stack(stack_id).await?;
        let board = self.board_for_edit(user_id, &stack.board_id).await?;
        sqlx::query("DELETE FROM fts WHERE object_kind = 'card' AND object_id IN (SELECT id FROM cards WHERE stack_id = ?)")
            .bind(stack_id)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM stacks WHERE id = ?").bind(stack_id).execute(&self.db).await?;
        self.board_changed(&board, "stacks").await
    }

    // ---- cards ---------------------------------------------------------------

    pub async fn card(&self, id: &str) -> Result<Card> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {CARD_COLS} FROM cards WHERE id = ?")))
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    async fn index_card(&self, card: &Card) -> Result<()> {
        sqlx::query("DELETE FROM fts WHERE object_kind = 'card' AND object_id = ?")
            .bind(&card.id)
            .execute(&self.db)
            .await?;
        sqlx::query("INSERT INTO fts (object_kind, object_id, scope, title, body) VALUES ('card', ?, ?, ?, ?)")
            .bind(&card.id)
            .bind(&card.board_id)
            .bind(&card.title)
            .bind(&card.description)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn create_card(&self, user_id: &str, stack_id: &str, title: &str, description: &str, due_at: Option<DateTime<Utc>>) -> Result<Card> {
        let stack = self.stack(stack_id).await?;
        let board = self.board_for_edit(user_id, &stack.board_id).await?;
        let title = title.trim();
        if title.is_empty() || title.len() > 500 {
            return Err(Error::bad("card title must be 1-500 characters"));
        }
        let pos: f64 = sqlx::query_scalar("SELECT CAST(COALESCE(MAX(position), -1) + 1 AS REAL) FROM cards WHERE stack_id = ?")
            .bind(stack_id)
            .fetch_one(&self.db)
            .await?;
        let id = new_id();
        let t = now();
        sqlx::query(
            "INSERT INTO cards (id, stack_id, board_id, title, description, position, due_at, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(stack_id)
        .bind(&board.id)
        .bind(title)
        .bind(description)
        .bind(pos)
        .bind(due_at)
        .bind(user_id)
        .bind(t)
        .bind(t)
        .execute(&self.db)
        .await?;
        let card = self.card(&id).await?;
        self.index_card(&card).await?;
        self.board_changed(&board, "cards").await?;
        Ok(card)
    }

    pub async fn update_card(&self, user_id: &str, card_id: &str, up: CardUpdate) -> Result<Card> {
        let card = self.card(card_id).await?;
        let board = self.board_for_edit(user_id, &card.board_id).await?;
        let mut tx = self.db.begin().await?;
        if let Some(t) = up.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            sqlx::query("UPDATE cards SET title = ? WHERE id = ?")
                .bind(t)
                .bind(card_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(d) = &up.description {
            sqlx::query("UPDATE cards SET description = ? WHERE id = ?")
                .bind(d)
                .bind(card_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(due) = up.due_at {
            sqlx::query("UPDATE cards SET due_at = ? WHERE id = ?")
                .bind(due)
                .bind(card_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(d) = up.done {
            sqlx::query("UPDATE cards SET done = ? WHERE id = ?")
                .bind(d)
                .bind(card_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(a) = up.archived {
            sqlx::query("UPDATE cards SET archived = ? WHERE id = ?")
                .bind(a)
                .bind(card_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE cards SET updated_at = ? WHERE id = ?")
            .bind(now())
            .bind(card_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let card = self.card(card_id).await?;
        self.index_card(&card).await?;
        self.board_changed(&board, "cards").await?;
        Ok(card)
    }

    /// Moves a card to `stack_id` at position `index` (0 = top).
    pub async fn move_card(&self, user_id: &str, card_id: &str, stack_id: &str, index: usize) -> Result<Card> {
        let card = self.card(card_id).await?;
        let board = self.board_for_edit(user_id, &card.board_id).await?;
        let target = self.stack(stack_id).await?;
        if target.board_id != card.board_id {
            return Err(Error::bad("cards can only move within their board"));
        }
        let mut ids: Vec<String> = sqlx::query_scalar("SELECT id FROM cards WHERE stack_id = ? AND id != ? AND archived = 0 ORDER BY position")
            .bind(stack_id)
            .bind(card_id)
            .fetch_all(&self.db)
            .await?;
        ids.insert(index.min(ids.len()), card_id.to_string());
        let mut tx = self.db.begin().await?;
        for (i, id) in ids.iter().enumerate() {
            sqlx::query("UPDATE cards SET stack_id = ?, position = ?, updated_at = ? WHERE id = ?")
                .bind(stack_id)
                .bind(i as f64)
                .bind(now())
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.board_changed(&board, "cards").await?;
        self.card(card_id).await
    }

    pub async fn delete_card(&self, user_id: &str, card_id: &str) -> Result<()> {
        let card = self.card(card_id).await?;
        let board = self.board_for_edit(user_id, &card.board_id).await?;
        sqlx::query("DELETE FROM fts WHERE object_kind = 'card' AND object_id = ?")
            .bind(card_id)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM cards WHERE id = ?").bind(card_id).execute(&self.db).await?;
        self.board_changed(&board, "cards").await
    }

    pub async fn create_label(&self, user_id: &str, board_id: &str, title: &str, color: &str) -> Result<Label> {
        let board = self.board_for_edit(user_id, board_id).await?;
        let id = new_id();
        sqlx::query("INSERT INTO board_labels (id, board_id, title, color) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(board_id)
            .bind(title.trim())
            .bind(color)
            .execute(&self.db)
            .await?;
        self.board_changed(&board, "labels").await?;
        Ok(sqlx::query_as("SELECT id, board_id, title, color FROM board_labels WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn delete_label(&self, user_id: &str, label_id: &str) -> Result<()> {
        let board_id: String = sqlx::query_scalar("SELECT board_id FROM board_labels WHERE id = ?")
            .bind(label_id)
            .fetch_one(&self.db)
            .await?;
        let board = self.board_for_edit(user_id, &board_id).await?;
        sqlx::query("DELETE FROM board_labels WHERE id = ?").bind(label_id).execute(&self.db).await?;
        self.board_changed(&board, "labels").await
    }

    pub async fn set_card_label(&self, user_id: &str, card_id: &str, label_id: &str, on: bool) -> Result<()> {
        let card = self.card(card_id).await?;
        let board = self.board_for_edit(user_id, &card.board_id).await?;
        let lb: String = sqlx::query_scalar("SELECT board_id FROM board_labels WHERE id = ?")
            .bind(label_id)
            .fetch_one(&self.db)
            .await?;
        if lb != card.board_id {
            return Err(Error::bad("label belongs to another board"));
        }
        if on {
            sqlx::query("INSERT OR IGNORE INTO card_labels (card_id, label_id) VALUES (?, ?)")
                .bind(card_id)
                .bind(label_id)
                .execute(&self.db)
                .await?;
        } else {
            sqlx::query("DELETE FROM card_labels WHERE card_id = ? AND label_id = ?")
                .bind(card_id)
                .bind(label_id)
                .execute(&self.db)
                .await?;
        }
        self.board_changed(&board, "cards").await
    }

    pub async fn set_card_assignee(&self, user_id: &str, card_id: &str, assignee: &str, on: bool) -> Result<()> {
        let card = self.card(card_id).await?;
        let board = self.board_for_edit(user_id, &card.board_id).await?;
        let target = match self.user(assignee).await {
            Ok(u) => u,
            Err(_) => self.user_by_name(assignee).await.map_err(|_| Error::bad("no such user"))?,
        };
        if self.board_access(&target.id, &board.id).await.is_err() {
            return Err(Error::bad("assignee must be a board member"));
        }
        if on {
            sqlx::query("INSERT OR IGNORE INTO card_assignees (card_id, user_id) VALUES (?, ?)")
                .bind(card_id)
                .bind(&target.id)
                .execute(&self.db)
                .await?;
            if target.id != user_id {
                let me = self.user(user_id).await?;
                self.notify(
                    &target.id,
                    "board",
                    &format!("{} assigned you “{}”", me.display_name, card.title),
                    &board.title,
                    &format!("/boards/{}?card={card_id}", board.id),
                )
                .await?;
            }
        } else {
            sqlx::query("DELETE FROM card_assignees WHERE card_id = ? AND user_id = ?")
                .bind(card_id)
                .bind(&target.id)
                .execute(&self.db)
                .await?;
        }
        self.board_changed(&board, "cards").await
    }

    /// Links a card to a file/folder (`file`, by id or path), a conversation (`room`, by token)
    /// or a web address (`url`).
    pub async fn add_card_link(&self, user_id: &str, card_id: &str, kind: &str, target: &str, title: Option<&str>) -> Result<CardLink> {
        let card = self.card(card_id).await?;
        let board = self.board_for_edit(user_id, &card.board_id).await?;
        let (target, default_title) = match kind {
            "file" => {
                let node = self.resolve(user_id, target).await?;
                if self.permissions(user_id, &node).await? & perm::READ == 0 {
                    return Err(Error::NotFound);
                }
                (node.id.clone(), node.name.clone())
            }
            "room" => {
                let room = self.room_for(user_id, target).await?;
                let name = self.room_display_name(&room, user_id).await?;
                (room.token, name)
            }
            "url" => {
                if !(target.starts_with("https://") || target.starts_with("http://")) {
                    return Err(Error::bad("URL must start with http:// or https://"));
                }
                (target.to_string(), target.to_string())
            }
            _ => return Err(Error::bad("link kind must be file, room or url")),
        };
        let id = new_id();
        sqlx::query("INSERT INTO card_links (id, card_id, kind, target, title) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(card_id)
            .bind(kind)
            .bind(&target)
            .bind(title.unwrap_or(&default_title))
            .execute(&self.db)
            .await?;
        self.board_changed(&board, "cards").await?;
        Ok(sqlx::query_as("SELECT id, card_id, kind, target, title FROM card_links WHERE id = ?")
            .bind(&id)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn remove_card_link(&self, user_id: &str, link_id: &str) -> Result<()> {
        let card_id: String = sqlx::query_scalar("SELECT card_id FROM card_links WHERE id = ?")
            .bind(link_id)
            .fetch_one(&self.db)
            .await?;
        let card = self.card(&card_id).await?;
        let board = self.board_for_edit(user_id, &card.board_id).await?;
        sqlx::query("DELETE FROM card_links WHERE id = ?").bind(link_id).execute(&self.db).await?;
        self.board_changed(&board, "cards").await
    }

    pub async fn add_card_comment(&self, user_id: &str, card_id: &str, body: &str) -> Result<CardComment> {
        let card = self.card(card_id).await?;
        let (board, _) = self.board_access(user_id, &card.board_id).await?;
        let body = body.trim();
        if body.is_empty() {
            return Err(Error::bad("comment is empty"));
        }
        let id = new_id();
        sqlx::query("INSERT INTO card_comments (id, card_id, author_id, body, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(card_id)
            .bind(user_id)
            .bind(body)
            .bind(now())
            .execute(&self.db)
            .await?;
        self.board_changed(&board, "comments").await?;
        self.card_comments(user_id, card_id)
            .await?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or(Error::NotFound)
    }

    pub async fn card_comments(&self, user_id: &str, card_id: &str) -> Result<Vec<CardComment>> {
        let card = self.card(card_id).await?;
        self.board_access(user_id, &card.board_id).await?;
        Ok(sqlx::query_as(
            "SELECT c.id, c.card_id, c.author_id, COALESCE(u.display_name, 'Assistant') AS author_name, c.body, c.created_at
             FROM card_comments c LEFT JOIN users u ON u.id = c.author_id WHERE c.card_id = ? ORDER BY c.created_at",
        )
        .bind(card_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Open cards assigned to the user across all boards, soonest due first.
    pub async fn my_cards(&self, user_id: &str) -> Result<Vec<Card>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM cards c JOIN card_assignees a ON a.card_id = c.id
             WHERE a.user_id = ? AND c.done = 0 AND c.archived = 0 ORDER BY c.due_at IS NULL, c.due_at, c.updated_at DESC LIMIT 200",
            CARD_COLS.split(", ").map(|c| format!("c.{c}")).collect::<Vec<_>>().join(", ")
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }
}
