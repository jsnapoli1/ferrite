use crate::AppState;
use crate::auth::Auth;
use crate::error::ApiResult;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use ferrite_core::boards::CardUpdate;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/boards", get(list).post(create))
        .route("/api/boards/my-cards", get(my_cards))
        .route("/api/boards/{id}", get(view).patch(update).delete(remove))
        .route("/api/boards/{id}/members", post(add_member))
        .route("/api/boards/{id}/members/{user_id}", delete(remove_member))
        .route("/api/boards/{id}/stacks", post(create_stack))
        .route("/api/boards/{id}/labels", post(create_label))
        .route("/api/labels/{id}", delete(delete_label))
        .route("/api/stacks/{id}", patch(update_stack).delete(delete_stack))
        .route("/api/stacks/{id}/cards", post(create_card))
        .route("/api/cards/{id}", get(card).patch(update_card).delete(delete_card))
        .route("/api/cards/{id}/move", post(move_card))
        .route("/api/cards/{id}/labels/{label}", post(add_label).delete(remove_label))
        .route("/api/cards/{id}/assignees", post(assign))
        .route("/api/cards/{id}/links", post(add_link))
        .route("/api/card-links/{id}", delete(remove_link))
        .route("/api/cards/{id}/comments", get(comments).post(comment))
}

fn ok() -> Json<Value> {
    Json(json!({"ok": true}))
}

#[derive(Deserialize)]
struct ListQ {
    #[serde(default)]
    archived: bool,
}

async fn list(State(s): State<AppState>, auth: Auth, Query(q): Query<ListQ>) -> ApiResult<Json<Value>> {
    auth.require_scope("boards")?;
    Ok(Json(json!({"boards": s.core.list_boards(&auth.user.id, q.archived).await?})))
}

#[derive(Deserialize)]
struct NewBoard {
    title: String,
    color: Option<String>,
    stacks: Option<Vec<String>>,
}

async fn create(State(s): State<AppState>, auth: Auth, Json(b): Json<NewBoard>) -> ApiResult<Json<Value>> {
    auth.require_scope("boards")?;
    let board = s.core.create_board(&auth.user.id, &b.title, b.color.as_deref(), b.stacks.as_deref()).await?;
    Ok(Json(json!({"board": board})))
}

async fn my_cards(State(s): State<AppState>, auth: Auth) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"cards": s.core.my_cards(&auth.user.id).await?})))
}

async fn view(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    auth.require_scope("boards")?;
    Ok(Json(json!({"board": s.core.board_view(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct BoardUpdate {
    title: Option<String>,
    color: Option<String>,
    archived: Option<bool>,
}

async fn update(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<BoardUpdate>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"board": s.core.update_board(&auth.user.id, &id, b.title.as_deref(), b.color.as_deref(), b.archived).await?}),
    ))
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_board(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct MemberBody {
    username: String,
    #[serde(default = "yes")]
    can_edit: bool,
}

fn yes() -> bool {
    true
}

async fn add_member(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<MemberBody>) -> ApiResult<Json<Value>> {
    s.core.add_board_member(&auth.user.id, &id, &b.username, b.can_edit).await?;
    Ok(ok())
}

async fn remove_member(State(s): State<AppState>, auth: Auth, Path((id, user_id)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    s.core.remove_board_member(&auth.user.id, &id, &user_id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct TitleBody {
    title: String,
    color: Option<String>,
}

async fn create_stack(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<TitleBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"stack": s.core.create_stack(&auth.user.id, &id, &b.title).await?})))
}

async fn create_label(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<TitleBody>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"label": s.core.create_label(&auth.user.id, &id, &b.title, b.color.as_deref().unwrap_or("#64748b")).await?}),
    ))
}

async fn delete_label(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_label(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct StackUpdate {
    title: Option<String>,
    index: Option<usize>,
}

async fn update_stack(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<StackUpdate>) -> ApiResult<Json<Value>> {
    s.core.update_stack(&auth.user.id, &id, b.title.as_deref(), b.index).await?;
    Ok(ok())
}

async fn delete_stack(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_stack(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct NewCard {
    title: String,
    #[serde(default)]
    description: String,
    due_at: Option<DateTime<Utc>>,
}

async fn create_card(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<NewCard>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"card": s.core.create_card(&auth.user.id, &id, &b.title, &b.description, b.due_at).await?}),
    ))
}

async fn card(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let c = s.core.card(&id).await?;
    let view = s.core.board_view(&auth.user.id, &c.board_id).await?;
    let card = view.stacks.iter().flat_map(|st| st.cards.iter()).find(|x| x.card.id == id).cloned();
    Ok(Json(
        json!({"card": card.map(|c| serde_json::to_value(c).unwrap_or_default()).unwrap_or_else(|| json!(c)), "comments": s.core.card_comments(&auth.user.id, &id).await?}),
    ))
}

async fn update_card(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<CardUpdate>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"card": s.core.update_card(&auth.user.id, &id, b).await?})))
}

async fn delete_card(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.delete_card(&auth.user.id, &id).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct MoveBody {
    stack_id: String,
    index: usize,
}

async fn move_card(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<MoveBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"card": s.core.move_card(&auth.user.id, &id, &b.stack_id, b.index).await?})))
}

async fn add_label(State(s): State<AppState>, auth: Auth, Path((id, label)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    s.core.set_card_label(&auth.user.id, &id, &label, true).await?;
    Ok(ok())
}

async fn remove_label(State(s): State<AppState>, auth: Auth, Path((id, label)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    s.core.set_card_label(&auth.user.id, &id, &label, false).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct AssignBody {
    user: String,
    #[serde(default = "yes")]
    assigned: bool,
}

async fn assign(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<AssignBody>) -> ApiResult<Json<Value>> {
    s.core.set_card_assignee(&auth.user.id, &id, &b.user, b.assigned).await?;
    Ok(ok())
}

#[derive(Deserialize)]
struct LinkBody {
    kind: String,
    target: String,
    title: Option<String>,
}

async fn add_link(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<LinkBody>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"link": s.core.add_card_link(&auth.user.id, &id, &b.kind, &b.target, b.title.as_deref()).await?}),
    ))
}

async fn remove_link(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    s.core.remove_card_link(&auth.user.id, &id).await?;
    Ok(ok())
}

async fn comments(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"comments": s.core.card_comments(&auth.user.id, &id).await?})))
}

#[derive(Deserialize)]
struct CommentBody {
    body: String,
}

async fn comment(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, Json(b): Json<CommentBody>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"comment": s.core.add_card_comment(&auth.user.id, &id, &b.body).await?})))
}
