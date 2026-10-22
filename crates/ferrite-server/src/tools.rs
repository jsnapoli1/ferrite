//! Ferrite's MCP tool set. The same tools are served to external MCP clients at `/mcp`
//! (and over stdio with `ferrite mcp`), and used by the built-in assistant and Talk bot.
//! Every call runs as the calling user, so permissions are identical to the web UI.

use crate::craft_bridge::CraftBridge;
use crate::external::ExternalMcp;
use chrono::{DateTime, Utc};
use ferrite_core::auth::User;
use ferrite_core::boards::CardUpdate;
use ferrite_core::collab::{DocKind, codec};
use ferrite_core::files::{Node, WriteOpts, perm};
use ferrite_core::shares::NewShare;
use ferrite_core::talk::breakout::Assignment;
use ferrite_core::talk::moderation::ModerationUpdate;
use ferrite_core::talk::{Actor, NewMessage};
use ferrite_core::util::short_token;
use ferrite_core::{Core, Error};
use ferrite_mcp::*;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use yrs::{GetString, Map, Text, Transact};

/// Who is calling: a user and the scopes their credential grants.
#[derive(Clone)]
pub struct UserCtx {
    pub user: User,
    pub scopes: String,
}

impl UserCtx {
    fn allows(&self, scope: &str) -> bool {
        self.scopes.split(',').map(str::trim).any(|s| s == "*" || s == scope)
    }
}

pub struct FerriteTools {
    pub core: Core,
    pub craft: Arc<CraftBridge>,
    pub external: Arc<ExternalMcp>,
    pub semantic: Arc<crate::semantic::Semantic>,
}

type R = Result<ToolResult, Error>;

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

/// Scope a tool belongs to (from its name).
fn scope_of(tool: &str) -> &'static str {
    match tool.split('_').next().unwrap_or("") {
        "files" | "trash" => "files",
        "talk" => "talk",
        "boards" => "boards",
        "docs" | "sheet" | "mindmap" | "canvas" => "docs",
        "craft" => "craft",
        "calendar" | "contacts" => "groupware",
        _ => "read",
    }
}

/// "B3" -> (row 2, col 1).
pub fn parse_a1(cell: &str) -> Option<(usize, usize)> {
    let cell = cell.trim().to_ascii_uppercase();
    let split = cell.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = cell.split_at(split);
    if letters.is_empty() || letters.len() > 3 || !letters.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let mut col = 0usize;
    for c in letters.chars() {
        col = col * 26 + (c as usize - 'A' as usize + 1);
    }
    let row: usize = digits.parse().ok()?;
    if row == 0 {
        return None;
    }
    Some((row - 1, col - 1))
}

fn definitions() -> &'static Vec<Tool> {
    static DEFS: OnceLock<Vec<Tool>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let p = "Path in your files, e.g. /Projects/plan.md (folders shared with you appear at the root), or a file id";
        vec![
            // Files
            Tool::new("files_list", "List folder", "List the files and folders in a folder.", &[("path", "string", "Folder path; / for your root", false)]).read_only(),
            Tool::new("files_info", "File info", "Details of a file or folder: size, type, owner, permissions, lock, versions, and which Craft app or live editor opens it.", &[("path", "string", p, true)]).read_only(),
            Tool::new("files_read", "Read file", "Read a text file (Markdown, CSV, code, JSON, mind maps, canvases). Live documents include unsaved edits from people editing right now.", &[("path", "string", p, true), ("max_bytes", "integer", "Maximum bytes to return (default 200000)", false)]).read_only(),
            Tool::new("files_write", "Write file", "Create or overwrite a text file; parent folders are created. The previous content is kept as a version.", &[("path", "string", p, true), ("content", "string", "Full new content", true)]),
            Tool::new("files_mkdir", "Create folder", "Create a folder (and any missing parents).", &[("path", "string", "Folder path", true)]),
            Tool::new("files_move", "Move or rename", "Move or rename a file or folder.", &[("path", "string", p, true), ("to", "string", "New full path, e.g. /Archive/plan.md", true)]),
            Tool::new("files_copy", "Copy", "Copy a file or folder.", &[("path", "string", p, true), ("to", "string", "Full path of the copy", true)]),
            Tool::new("files_delete", "Delete", "Move a file or folder to the trash (restorable for 30 days).", &[("path", "string", p, true)]).destructive(),
            Tool::new("files_search", "Search files", "Full text search over file names and the text of documents you can access.", &[("query", "string", "Words to search for", true)]).read_only(),
            Tool::new("files_recent", "Recent files", "Your most recently changed files.", &[]).read_only(),
            Tool::new("files_versions", "List versions", "Earlier versions of a file.", &[("path", "string", p, true)]).read_only(),
            Tool::new("files_restore_version", "Restore version", "Make an earlier version current again (the current content becomes a version).", &[("path", "string", p, true), ("version_id", "string", "Id from files_versions", true)]),
            Tool::new("files_share", "Share", "Share a file or folder with a user, a group, a conversation (room token), or as a public link.", &[
                ("path", "string", p, true),
                ("kind", "string", "user, group, room or link", true),
                ("target", "string", "Username, group name or room token (not for links)", false),
                ("permission", "string", "read (default) or edit", false),
                ("password", "string", "Password for public links", false),
                ("expires_in_days", "integer", "Days until the share expires", false),
            ]),
            Tool::new("files_shares", "List shares", "Who a file or folder is shared with, and its public links.", &[("path", "string", p, true)]).read_only(),
            Tool::new("trash_list", "List trash", "Items in your trash.", &[]).read_only(),
            Tool::new("trash_restore", "Restore from trash", "Restore an item from the trash.", &[("id", "string", "Id from trash_list", true)]),
            // Talk
            Tool::new("talk_rooms", "List conversations", "Your conversations with unread counts.", &[]).read_only(),
            Tool::new("talk_read", "Read conversation", "Read recent messages of a conversation.", &[
                ("room", "string", "Room token or conversation name", true),
                ("limit", "integer", "Messages to return (default 50, max 200)", false),
                ("before", "integer", "Only messages with a smaller id (paging back)", false),
            ]).read_only(),
            Tool::new("talk_post", "Post message", "Post a message in a conversation. Mention people with @username.", &[("room", "string", "Room token or name", true), ("text", "string", "Message (Markdown)", true), ("reply_to", "integer", "Message id to reply to", false)]),
            Tool::new("talk_create_room", "Create conversation", "Create a group conversation.", &[("name", "string", "Name", true), ("public", "boolean", "Allow anyone with the link (and guests) to join", false), ("invite", "string[]", "Usernames to add", false)]),
            Tool::new("talk_direct", "Direct message", "Open (or create) a one-to-one conversation and optionally send a message.", &[("username", "string", "Who to message", true), ("text", "string", "Message to send", false)]),
            Tool::new("talk_share_file", "Share file in chat", "Share a file into a conversation; all participants can open it.", &[("room", "string", "Room token or name", true), ("path", "string", p, true), ("caption", "string", "Optional message", false)]),
            Tool::new("talk_react", "React", "Add an emoji reaction to a message.", &[("message_id", "integer", "Message id", true), ("emoji", "string", "Emoji", true)]),
            Tool::new("talk_poll", "Create poll", "Post a poll in a conversation.", &[("room", "string", "Room token or name", true), ("question", "string", "Question", true), ("options", "string[]", "2-20 answers", true), ("multiple", "boolean", "Allow several answers", false)]),
            Tool::new("talk_search", "Search messages", "Full text search over messages in your conversations.", &[("query", "string", "Words to search for", true)]).read_only(),
            Tool::new("talk_lobby", "Lobby", "Turn the lobby of a group or public conversation you moderate on or off. While it is on, only moderators can read, write and call; everyone else waits until it opens.", &[
                ("room", "string", "Room token or name", true),
                ("enabled", "boolean", "true to hold people in the lobby, false to let everyone in", true),
                ("opens_at", "string", "With enabled: when the lobby opens by itself (RFC 3339, e.g. 2026-11-05T14:00:00Z)", false),
            ]),
            Tool::new("talk_breakouts", "Breakout rooms", "The breakout rooms of a conversation: mode, whether they run, and who is in which room.", &[("room", "string", "Room token or name", true)]).read_only(),
            Tool::new("talk_breakouts_start", "Start breakout rooms", "Split a conversation you moderate into breakout rooms and start them: participants move into their room's chat and call. Replaces earlier breakout rooms and their chats.", &[
                ("room", "string", "Room token or name", true),
                ("count", "integer", "Number of breakout rooms (1-20)", true),
                ("mode", "string", "auto (spread people evenly, default), manual (use `assignments`) or free (participants pick a room)", false),
                ("assignments", "object", "manual mode: username -> breakout room number (1-based), e.g. {\"bob\": 1, \"carol\": 2}", false),
                ("minutes", "integer", "Stop automatically after this many minutes", false),
            ]),
            Tool::new("talk_breakouts_stop", "Stop breakout rooms", "Stop the running breakout rooms of a conversation and bring everyone back to the main room. Their chats stay readable.", &[("room", "string", "Room token or name", true)]),
            Tool::new("talk_breakouts_broadcast", "Message all breakout rooms", "Post a message into every running breakout room of a conversation you moderate.", &[("room", "string", "Room token or name", true), ("text", "string", "Message (Markdown)", true)]),
            // Boards
            Tool::new("boards_list", "List boards", "Your kanban boards.", &[]).read_only(),
            Tool::new("boards_get", "Get board", "A board with its stacks (columns), cards, labels and members.", &[("board", "string", "Board id or title", true)]).read_only(),
            Tool::new("boards_create", "Create board", "Create a kanban board.", &[("title", "string", "Title", true), ("stacks", "string[]", "Column names (default To do, Doing, Done)", false)]),
            Tool::new("boards_add_card", "Add card", "Add a card to a board column.", &[
                ("board", "string", "Board id or title", true),
                ("stack", "string", "Column id or title", true),
                ("title", "string", "Card title", true),
                ("description", "string", "Markdown description", false),
                ("due", "string", "Due date-time (RFC 3339)", false),
                ("labels", "string[]", "Label titles", false),
                ("assignees", "string[]", "Usernames", false),
            ]),
            Tool::new("boards_update_card", "Update card", "Change a card's title, description, due date, or done/archived state.", &[
                ("card_id", "string", "Card id", true),
                ("title", "string", "New title", false),
                ("description", "string", "New description", false),
                ("due", "string", "Due date-time (RFC 3339); empty string clears it", false),
                ("done", "boolean", "Mark done", false),
                ("archived", "boolean", "Archive", false),
            ]),
            Tool::new("boards_move_card", "Move card", "Move a card to a column (and position).", &[("card_id", "string", "Card id", true), ("stack", "string", "Column id or title", true), ("position", "integer", "0 = top (default: bottom)", false)]),
            Tool::new("boards_assign", "Assign card", "Assign or unassign a board member.", &[("card_id", "string", "Card id", true), ("username", "string", "Username", true), ("remove", "boolean", "Unassign instead", false)]),
            Tool::new("boards_comment", "Comment on card", "Add a comment to a card.", &[("card_id", "string", "Card id", true), ("text", "string", "Comment", true)]),
            Tool::new("boards_link", "Link to card", "Attach a file, mind map, canvas, conversation or URL to a card.", &[("card_id", "string", "Card id", true), ("kind", "string", "file, room or url", true), ("target", "string", "Path, room token or URL", true)]),
            Tool::new("boards_my_cards", "My cards", "Open cards assigned to you across all boards, soonest due first.", &[]).read_only(),
            // Live documents
            Tool::new("docs_create", "Create document", "Create a live collaborative document: text (Markdown), sheet (CSV), mindmap or canvas (a board to arrange notes, files and mind maps).", &[
                ("folder", "string", "Folder path (default /)", false),
                ("title", "string", "Title (file name without extension)", true),
                ("kind", "string", "text, sheet, mindmap or canvas", true),
            ]),
            Tool::new("docs_read", "Read document", "Read a live document in a structured form: text, sheet cells, mind map outline with node ids, or canvas items.", &[("path", "string", p, true)]).read_only(),
            Tool::new("docs_append", "Append text", "Append text to a live text document; people editing see it immediately.", &[("path", "string", p, true), ("text", "string", "Text to append", true)]),
            Tool::new("docs_replace", "Replace text", "Replace text in a live text document.", &[("path", "string", p, true), ("find", "string", "Exact text to find", true), ("replace", "string", "Replacement", true), ("all", "boolean", "Replace every occurrence (default first only)", false)]),
            Tool::new("sheet_set", "Set cells", "Set cells of a live sheet (CSV). Values starting with = are formulas (SUM, AVERAGE, MIN, MAX, COUNT over ranges like A1:A9).", &[("path", "string", p, true), ("cells", "object", "Map of A1-style cell to value, e.g. {\"A1\": \"Name\", \"B2\": \"=SUM(B3:B9)\"}", true)]),
            Tool::new("mindmap_add", "Add mind map nodes", "Add nodes (with nested children) under a node of a live mind map.", &[
                ("path", "string", p, true),
                ("parent", "string", "Parent node id or exact text (default: the central node)", false),
                ("nodes", "array", "Nodes like {\"text\": \"Idea\", \"color\": \"#f59e0b\", \"link\": \"/Docs/a.md\", \"children\": [...]}", true),
            ]),
            Tool::new("mindmap_update", "Edit mind map node", "Change, move or delete a mind map node.", &[
                ("path", "string", p, true),
                ("node_id", "string", "Node id (from docs_read)", true),
                ("text", "string", "New text", false),
                ("color", "string", "CSS color", false),
                ("link", "string", "File path or URL", false),
                ("parent", "string", "New parent node id", false),
                ("delete", "boolean", "Delete the node and its descendants", false),
            ]),
            Tool::new("canvas_add", "Add canvas items", "Place items on a live canvas: notes, files, mind maps, boards, links or frames. Files and mind maps are referenced by path, boards by id.", &[
                ("path", "string", p, true),
                ("items", "array", "Items like {\"type\": \"note\", \"text\": \"…\", \"x\": 0, \"y\": 0, \"color\": \"#fde68a\"} or {\"type\": \"file\", \"ref\": \"/Docs/spec.pdf\"}", true),
            ]),
            Tool::new("canvas_connect", "Connect canvas items", "Draw an arrow between two canvas items.", &[("path", "string", p, true), ("from", "string", "Item id", true), ("to", "string", "Item id", true), ("label", "string", "Arrow label", false)]),
            // Calendar and contacts
            Tool::new("calendar_events", "Calendar events", "Events and tasks in your calendars between two times (default: the next 7 days).", &[
                ("from", "string", "Start (RFC 3339 or YYYY-MM-DD); default now", false),
                ("to", "string", "End (RFC 3339 or YYYY-MM-DD); default from + 7 days", false),
            ]).read_only(),
            Tool::new("calendar_create_event", "Create event", "Add an event to your calendar (it syncs to phones over CalDAV).", &[
                ("summary", "string", "Title", true),
                ("start", "string", "Start (RFC 3339, e.g. 2026-11-05T14:00:00Z, or YYYY-MM-DD for all-day)", true),
                ("end", "string", "End (default: 1 hour later, or the next day for all-day)", false),
                ("description", "string", "Notes", false),
                ("location", "string", "Location", false),
                ("calendar", "string", "Calendar name (default: the first calendar)", false),
            ]),
            Tool::new("calendar_update_event", "Update event", "Change an event's title, time, description or location; other details (alarms, attendees, recurrence) are kept.", &[
                ("event_id", "string", "Event id from calendar_events", true),
                ("summary", "string", "New title", false),
                ("start", "string", "New start", false),
                ("end", "string", "New end", false),
                ("description", "string", "New notes", false),
                ("location", "string", "New location", false),
            ]),
            Tool::new("calendar_delete_event", "Delete event", "Delete an event or task.", &[("event_id", "string", "Event id", true)]).destructive(),
            Tool::new("contacts_search", "Search contacts", "Find contacts by name, email, phone or organization.", &[("query", "string", "Search text (empty for all)", false)]).read_only(),
            Tool::new("contacts_create", "Create contact", "Add a contact to your address book (syncs over CardDAV).", &[
                ("name", "string", "Full name", true),
                ("emails", "string[]", "Email addresses", false),
                ("phones", "string[]", "Phone numbers", false),
                ("org", "string", "Organization", false),
                ("note", "string", "Note", false),
                ("addressbook", "string", "Address book uri (default: your first)", false),
            ]),
            Tool::new("contacts_update", "Update contact", "Replace a contact's name, emails, phones, organization and note (other vCard fields are kept).", &[
                ("contact_id", "string", "Contact id from contacts_search", true),
                ("name", "string", "Full name", true),
                ("emails", "string[]", "Email addresses (replaces the existing ones)", false),
                ("phones", "string[]", "Phone numbers (replaces the existing ones)", false),
                ("org", "string", "Organization", false),
                ("note", "string", "Note", false),
            ]),
            // Everything else
            Tool::new("search", "Search", "Search everything you can access: files, chat messages, cards, calendar events and contacts. Matches words, and also meaning when the server has an embedding model (so a paraphrase finds the original).", &[("query", "string", "Words or a description of what you are looking for", true), ("kinds", "string[]", "Limit to file, message, card, event and/or contact", false)]).read_only(),
            Tool::new("notifications", "Notifications", "Your unread notifications.", &[]).read_only(),
            Tool::new("activity", "Activity", "Recent activity in your account.", &[]).read_only(),
            Tool::new("users_search", "Find people", "Find users by name (for sharing, mentions and assignments).", &[("query", "string", "Name or username", true)]).read_only(),
            // Craft apps
            Tool::new("craft_apps", "Craft apps", "The ArtCraft Crafting Apps (WordCraft, GridCraft, DeckCraft, VectorCraft, PhotoCraft, PdfCraft, DesignCraft, EffectCraft, CADCraft, SoundCraft, FigCraft, FilmCraft, LightCraft), what they open, and which are installed on this server.", &[]).read_only(),
            Tool::new("craft_open", "Open in Craft app", "Open a document in its Craft app on the server and get that app's own tools. The file is locked for others until craft_close.", &[("path", "string", p, true), ("app", "string", "App id to use instead of the default (e.g. vectorcraft)", false)]),
            Tool::new("craft_call", "Use Craft app tool", "Call a tool of an open Craft session.", &[("session", "string", "Session id from craft_open", true), ("tool", "string", "Tool name", true), ("arguments", "object", "Tool arguments", false)]),
            Tool::new("craft_close", "Close Craft session", "Close a Craft session; changed and new files are saved back as new versions.", &[("session", "string", "Session id", true), ("save", "boolean", "Save changes (default true)", false)]),
        ]
    })
}

impl FerriteTools {
    pub fn list(&self, ctx: &UserCtx) -> Vec<Tool> {
        definitions()
            .iter()
            .filter(|t| ctx.allows(scope_of(&t.name)) || scope_of(&t.name) == "read")
            .cloned()
            .collect()
    }

    pub async fn call(&self, ctx: &UserCtx, name: &str, args: Value) -> Result<ToolResult, RpcError> {
        if !definitions().iter().any(|t| t.name == name) {
            return Err(RpcError::invalid_params(format!("Unknown tool: {name}")));
        }
        let scope = scope_of(name);
        if scope != "read" && !ctx.allows(scope) {
            return Ok(ToolResult::error(format!("This credential lacks the `{scope}` scope.")));
        }
        Ok(match self.dispatch(ctx, name, &args).await {
            Ok(r) => r,
            Err(e) => ToolResult::error(match e {
                Error::Internal(inner) => {
                    tracing::error!(tool = name, error = ?inner, "tool failed");
                    "internal error".to_string()
                }
                Error::NotFound => "Not found (or you don't have access).".to_string(),
                other => other.to_string(),
            }),
        })
    }

    async fn node(&self, ctx: &UserCtx, path: &str) -> Result<Node, Error> {
        self.core.resolve(&ctx.user.id, path).await
    }

    async fn describe_node(&self, ctx: &UserCtx, n: &Node) -> Result<Value, Error> {
        let p = self.core.permissions(&ctx.user.id, n).await?;
        let path = self.core.display_path(&ctx.user.id, n).await?;
        let entry = self.core.entry(&ctx.user.id, n.clone(), p).await?;
        Ok(json!({
            "id": n.id, "path": path, "name": n.name, "kind": n.kind, "size": n.size, "mime": n.mime,
            "modified": n.modified_at, "etag": n.etag,
            "can_edit": p & perm::UPDATE != 0, "can_share": p & perm::SHARE != 0,
            "shared_by": entry.shared_by, "locked_by_app": entry.lock.as_ref().map(|l| l.owner_app.clone()),
            "craft_app": entry.craft_app, "live_editor": entry.live_editor,
        }))
    }

    async fn room(&self, ctx: &UserCtx, room: &str) -> Result<ferrite_core::talk::Room, Error> {
        if let Ok(r) = self.core.room_for(&ctx.user.id, room).await {
            return Ok(r);
        }
        let rooms = self.core.list_rooms(&ctx.user.id).await?;
        rooms
            .into_iter()
            .find(|r| r.display_name.eq_ignore_ascii_case(room) || r.room.name.eq_ignore_ascii_case(room))
            .map(|r| r.room)
            .ok_or(Error::NotFound)
    }

    async fn board_id(&self, ctx: &UserCtx, board: &str) -> Result<String, Error> {
        let boards = self.core.list_boards(&ctx.user.id, true).await?;
        boards
            .into_iter()
            .find(|b| b.id == board || b.title.eq_ignore_ascii_case(board))
            .map(|b| b.id)
            .ok_or(Error::NotFound)
    }

    async fn stack_id(&self, ctx: &UserCtx, board_id: &str, stack: &str) -> Result<String, Error> {
        let view = self.core.board_view(&ctx.user.id, board_id).await?;
        view.stacks
            .into_iter()
            .find(|s| s.stack.id == stack || s.stack.title.eq_ignore_ascii_case(stack))
            .map(|s| s.stack.id)
            .ok_or_else(|| Error::bad(format!("no column named {stack:?} on this board")))
    }

    fn me(ctx: &UserCtx) -> Actor {
        Actor::user(&ctx.user.id, &ctx.user.display_name)
    }

    async fn live(&self, ctx: &UserCtx, path: &str, want: DocKind) -> Result<Node, Error> {
        let n = self.node(ctx, path).await?;
        match DocKind::for_name(&n.name) {
            Some(k) if k == want => Ok(n),
            _ => Err(Error::bad(format!("{} is not a {} document", n.name, want.as_str()))),
        }
    }

    async fn dispatch(&self, ctx: &UserCtx, name: &str, a: &Value) -> R {
        let core = &self.core;
        let uid = ctx.user.id.as_str();
        match name {
            "files_list" => {
                let dir = self.node(ctx, opt(a, "path").unwrap_or("/")).await?;
                let entries = core.list_dir(uid, &dir.id).await?;
                let list: Vec<Value> = entries
                    .iter()
                    .map(|e| json!({"id": e.node.id, "name": e.node.name, "kind": e.node.kind, "size": e.node.size, "modified": e.node.modified_at, "shared_by": e.shared_by, "craft_app": e.craft_app, "live_editor": e.live_editor}))
                    .collect();
                Ok(ToolResult::json(json!({"path": core.display_path(uid, &dir).await?, "entries": list})))
            }
            "files_info" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                Ok(ToolResult::json(self.describe_node(ctx, &n).await?))
            }
            "files_read" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                if n.is_dir() {
                    return Err(Error::bad("that is a folder; use files_list"));
                }
                let max = a.get("max_bytes").and_then(Value::as_u64).unwrap_or(200_000) as usize;
                let data = if DocKind::for_name(&n.name).is_some() {
                    core.collab_content(uid, &n.id).await?
                } else {
                    core.read_file(uid, &n.id).await?.1
                };
                match String::from_utf8(data) {
                    Ok(text) => {
                        let truncated = text.len() > max;
                        let mut end = max.min(text.len());
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        let mut out = text[..end].to_string();
                        if truncated {
                            out.push_str(&format!("\n…[truncated; {} bytes total]", text.len()));
                        }
                        Ok(ToolResult::text(out))
                    }
                    Err(_) => {
                        let hint = match ferrite_core::craft::app_for_name(&n.name) {
                            Some(app) => format!(" It is a {} document; use craft_open to work inside it.", app.name),
                            None => String::new(),
                        };
                        Ok(ToolResult::text(format!("{} is a binary file ({}, {} bytes).{hint}", n.name, n.mime, n.size)))
                    }
                }
            }
            "files_write" => {
                let path = s(a, "path")?;
                let content = a.get("content").and_then(Value::as_str).ok_or_else(|| Error::bad("`content` is required"))?;
                let n = core.write_path(uid, path, content.as_bytes(), WriteOpts::user()).await?;
                Ok(ToolResult::json(self.describe_node(ctx, &n).await?))
            }
            "files_mkdir" => {
                let n = core.mkdir_p(uid, s(a, "path")?).await?;
                Ok(ToolResult::json(self.describe_node(ctx, &n).await?))
            }
            "files_move" | "files_copy" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                let (parent, new_name) = core.resolve_parent(uid, s(a, "to")?).await?;
                let out = if name == "files_move" {
                    core.move_node(uid, &n.id, &parent.id, &new_name, false).await?
                } else {
                    core.copy_node(uid, &n.id, &parent.id, &new_name).await?
                };
                Ok(ToolResult::json(self.describe_node(ctx, &out).await?))
            }
            "files_delete" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                core.delete(uid, &n.id).await?;
                Ok(ToolResult::text(format!("Moved {} to the trash.", n.name)))
            }
            "files_search" => {
                let hits = core.search(uid, s(a, "query")?, Some(&["file"]), 25).await?;
                let mut out = Vec::new();
                for h in hits {
                    if let Ok(n) = core.node(&h.id).await {
                        out.push(json!({"id": n.id, "path": core.display_path(uid, &n).await?, "snippet": h.snippet}));
                    }
                }
                Ok(ToolResult::json(json!({"results": out})))
            }
            "files_recent" => {
                let mut out = Vec::new();
                for n in core.recent_files(uid, 20).await? {
                    out.push(json!({"id": n.id, "path": core.display_path(uid, &n).await?, "modified": n.modified_at, "size": n.size}));
                }
                Ok(ToolResult::json(json!({"files": out})))
            }
            "files_versions" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                Ok(ToolResult::json(json!({"versions": core.list_versions(uid, &n.id).await?})))
            }
            "files_restore_version" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                core.restore_version(uid, &n.id, s(a, "version_id")?).await?;
                Ok(ToolResult::text("Restored."))
            }
            "files_share" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                let edit = opt(a, "permission") == Some("edit");
                let permissions = if edit { perm::ALL } else { perm::READ };
                let expires_at = a
                    .get("expires_in_days")
                    .and_then(Value::as_i64)
                    .map(|d| Utc::now() + chrono::Duration::days(d.clamp(1, 3650)));
                let v = core
                    .create_share(
                        uid,
                        &n.id,
                        NewShare {
                            kind: s(a, "kind")?.to_string(),
                            target: opt(a, "target").map(String::from),
                            permissions: Some(permissions),
                            password: opt(a, "password").map(String::from),
                            expires_at,
                            label: None,
                        },
                    )
                    .await?;
                Ok(ToolResult::json(serde_json::to_value(v).unwrap_or_default()))
            }
            "files_shares" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                Ok(ToolResult::json(json!({"shares": core.shares_of_node(uid, &n.id).await?})))
            }
            "trash_list" => {
                let items: Vec<Value> = core
                    .list_trash(uid)
                    .await?
                    .into_iter()
                    .map(|n| json!({"id": n.id, "name": n.name, "kind": n.kind, "deleted": n.trashed_at}))
                    .collect();
                Ok(ToolResult::json(json!({"items": items})))
            }
            "trash_restore" => {
                let n = core.restore(uid, s(a, "id")?).await?;
                Ok(ToolResult::text(format!("Restored to {}", core.display_path(uid, &n).await?)))
            }

            "talk_rooms" => {
                let rooms: Vec<Value> = core
                    .list_rooms(uid)
                    .await?
                    .into_iter()
                    .map(|r| json!({"token": r.room.token, "name": r.display_name, "kind": r.room.kind, "unread": r.unread, "participants": r.participant_count, "last_activity": r.room.last_activity_at}))
                    .collect();
                Ok(ToolResult::json(json!({"rooms": rooms})))
            }
            "talk_read" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                let limit = a.get("limit").and_then(Value::as_i64).unwrap_or(50);
                let msgs = core
                    .list_messages(&Self::me(ctx), &room.token, a.get("before").and_then(Value::as_i64), None, limit)
                    .await?;
                let out: Vec<Value> = msgs
                    .iter()
                    .filter(|m| !m.message.deleted)
                    .map(|m| {
                        let mut v = json!({"id": m.message.id, "from": m.message.actor_name, "at": m.message.created_at, "text": m.message.body, "kind": m.message.kind});
                        if let Some(p) = &m.parent {
                            v["reply_to"] = json!({"id": p.id, "from": p.actor_name});
                        }
                        if !m.reactions.is_empty() {
                            v["reactions"] = json!(m.reactions.iter().map(|(e, who)| (e.clone(), who.len())).collect::<std::collections::BTreeMap<_, _>>());
                        }
                        if m.message.kind == "file" || m.message.kind == "poll" {
                            v["meta"] = m.message.meta.0.clone();
                        }
                        v
                    })
                    .collect();
                Ok(ToolResult::json(json!({"room": room.name, "token": room.token, "messages": out})))
            }
            "talk_post" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                let m = core
                    .post_message(
                        &Self::me(ctx),
                        &room.token,
                        NewMessage {
                            body: s(a, "text")?.to_string(),
                            reply_to: a.get("reply_to").and_then(Value::as_i64),
                            ..Default::default()
                        },
                    )
                    .await?;
                Ok(ToolResult::json(json!({"posted": m.message.id, "room": room.token})))
            }
            "talk_create_room" => {
                let public = a.get("public").and_then(Value::as_bool).unwrap_or(false);
                let r = core
                    .create_room(uid, if public { "public" } else { "group" }, s(a, "name")?, &strings(a, "invite"))
                    .await?;
                Ok(ToolResult::json(json!({"token": r.token, "name": r.name, "kind": r.kind})))
            }
            "talk_direct" => {
                let r = core.direct_room(uid, s(a, "username")?).await?;
                if let Some(text) = opt(a, "text") {
                    core.post_message(
                        &Self::me(ctx),
                        &r.token,
                        NewMessage {
                            body: text.to_string(),
                            ..Default::default()
                        },
                    )
                    .await?;
                }
                Ok(ToolResult::json(json!({"token": r.token})))
            }
            "talk_share_file" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                let n = self.node(ctx, s(a, "path")?).await?;
                let m = core.share_file_to_room(uid, &room.token, &n.id, opt(a, "caption").unwrap_or("")).await?;
                Ok(ToolResult::json(json!({"posted": m.message.id})))
            }
            "talk_react" => {
                let id = a
                    .get("message_id")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| Error::bad("`message_id` is required"))?;
                core.react(&Self::me(ctx), id, s(a, "emoji")?, true).await?;
                Ok(ToolResult::text("Reacted."))
            }
            "talk_poll" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                let m = core
                    .create_poll(
                        &Self::me(ctx),
                        &room.token,
                        s(a, "question")?,
                        &strings(a, "options"),
                        a.get("multiple").and_then(Value::as_bool).unwrap_or(false),
                    )
                    .await?;
                Ok(ToolResult::json(json!({"posted": m.message.id})))
            }
            "talk_lobby" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                let enabled = a.get("enabled").and_then(Value::as_bool).ok_or_else(|| Error::bad("`enabled` is required"))?;
                let timer = match opt(a, "opens_at") {
                    Some(t) if enabled => Some(
                        DateTime::parse_from_rfc3339(t)
                            .map_err(|_| Error::bad("opens_at must be RFC 3339, e.g. 2026-11-05T14:00:00Z"))?
                            .with_timezone(&Utc),
                    ),
                    _ => None,
                };
                let r = core
                    .update_moderation(
                        uid,
                        &room.token,
                        ModerationUpdate {
                            lobby: Some(enabled),
                            lobby_timer: Some(timer),
                            ..Default::default()
                        },
                    )
                    .await?;
                let waiting: Vec<String> = core.waiting_in_lobby(&r.id).into_iter().map(|w| w.name).collect();
                Ok(ToolResult::json(
                    json!({"room": r.token, "lobby": r.lobby_active(), "opens_at": r.lobby_timer, "waiting": waiting}),
                ))
            }
            "talk_breakouts" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                Ok(ToolResult::json(
                    serde_json::to_value(core.breakouts(&Self::me(ctx), &room.token).await?).unwrap_or_default(),
                ))
            }
            "talk_breakouts_start" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                let count = a.get("count").and_then(Value::as_u64).ok_or_else(|| Error::bad("`count` is required"))? as usize;
                let mode = opt(a, "mode").unwrap_or("auto");
                let view = core.configure_breakouts(uid, &room.token, mode, count).await?;
                if let Some(map) = a.get("assignments").and_then(Value::as_object) {
                    let mut list = Vec::new();
                    for (username, n) in map {
                        let user = core.user_by_name(username).await.map_err(|_| Error::bad(format!("no user {username}")))?;
                        let n = n.as_u64().filter(|n| *n >= 1 && (*n as usize) <= view.rooms.len());
                        let n = n.ok_or_else(|| Error::bad(format!("{username}: room number must be 1-{}", view.rooms.len())))?;
                        list.push(Assignment {
                            kind: "user".into(),
                            id: user.id,
                            room: Some(view.rooms[n as usize - 1].token.clone()),
                        });
                    }
                    core.assign_breakouts(uid, &room.token, &list).await?;
                }
                let minutes = a.get("minutes").and_then(Value::as_i64);
                let view = core.start_breakouts(uid, &room.token, minutes).await?;
                Ok(ToolResult::json(serde_json::to_value(view).unwrap_or_default()))
            }
            "talk_breakouts_stop" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                core.stop_breakouts(Some(uid), &room.token).await?;
                Ok(ToolResult::text("Stopped the breakout rooms; everyone is back in the main conversation."))
            }
            "talk_breakouts_broadcast" => {
                let room = self.room(ctx, s(a, "room")?).await?;
                let n = core.broadcast_to_breakouts(uid, &room.token, s(a, "text")?).await?;
                Ok(ToolResult::text(format!("Posted in {n} breakout {}.", if n == 1 { "room" } else { "rooms" })))
            }
            "talk_search" => {
                let hits = core.search(uid, s(a, "query")?, Some(&["message"]), 25).await?;
                Ok(ToolResult::json(
                    json!({"results": hits.iter().map(|h| json!({"message_id": h.id, "from": h.title, "room": h.context, "snippet": h.snippet})).collect::<Vec<_>>()}),
                ))
            }

            "boards_list" => {
                let b: Vec<Value> = core
                    .list_boards(uid, false)
                    .await?
                    .into_iter()
                    .map(|b| json!({"id": b.id, "title": b.title}))
                    .collect();
                Ok(ToolResult::json(json!({"boards": b})))
            }
            "boards_get" => {
                let id = self.board_id(ctx, s(a, "board")?).await?;
                let v = core.board_view(uid, &id).await?;
                let label_name = |lid: &str| v.labels.iter().find(|l| l.id == lid).map(|l| l.title.clone()).unwrap_or_default();
                let member_name = |u: &str| v.members.iter().find(|m| m.user_id == u).map(|m| m.username.clone()).unwrap_or_default();
                let stacks: Vec<Value> = v
                    .stacks
                    .iter()
                    .map(|st| {
                        json!({"id": st.stack.id, "title": st.stack.title, "cards": st.cards.iter().map(|c| json!({
                            "id": c.card.id, "title": c.card.title, "description": c.card.description, "due": c.card.due_at, "done": c.card.done,
                            "labels": c.labels.iter().map(|l| label_name(l)).collect::<Vec<_>>(),
                            "assignees": c.assignees.iter().map(|u| member_name(u)).collect::<Vec<_>>(),
                            "links": c.links.iter().map(|l| json!({"kind": l.kind, "title": l.title, "target": l.target})).collect::<Vec<_>>(),
                            "comments": c.comment_count,
                        })).collect::<Vec<_>>()})
                    })
                    .collect();
                Ok(ToolResult::json(
                    json!({"id": v.board.id, "title": v.board.title, "can_edit": v.can_edit, "stacks": stacks,
                    "labels": v.labels.iter().map(|l| &l.title).collect::<Vec<_>>(), "members": v.members.iter().map(|m| &m.username).collect::<Vec<_>>()}),
                ))
            }
            "boards_create" => {
                let stacks = strings(a, "stacks");
                let b = core
                    .create_board(uid, s(a, "title")?, None, if stacks.is_empty() { None } else { Some(&stacks) })
                    .await?;
                Ok(ToolResult::json(json!({"id": b.id, "title": b.title})))
            }
            "boards_add_card" => {
                let board = self.board_id(ctx, s(a, "board")?).await?;
                let stack = self.stack_id(ctx, &board, s(a, "stack")?).await?;
                let due = parse_due(opt(a, "due"))?;
                let card = core.create_card(uid, &stack, s(a, "title")?, opt(a, "description").unwrap_or(""), due).await?;
                let view = core.board_view(uid, &board).await?;
                for l in strings(a, "labels") {
                    if let Some(label) = view.labels.iter().find(|x| x.title.eq_ignore_ascii_case(&l)) {
                        core.set_card_label(uid, &card.id, &label.id, true).await?;
                    }
                }
                for u in strings(a, "assignees") {
                    core.set_card_assignee(uid, &card.id, &u, true).await?;
                }
                Ok(ToolResult::json(json!({"card_id": card.id})))
            }
            "boards_update_card" => {
                let due = match a.get("due").and_then(Value::as_str) {
                    Some("") => Some(None),
                    Some(d) => Some(parse_due(Some(d))?),
                    None => None,
                };
                let up = CardUpdate {
                    title: opt(a, "title").map(String::from),
                    description: a.get("description").and_then(Value::as_str).map(String::from),
                    due_at: due,
                    done: a.get("done").and_then(Value::as_bool),
                    archived: a.get("archived").and_then(Value::as_bool),
                };
                let c = core.update_card(uid, s(a, "card_id")?, up).await?;
                Ok(ToolResult::json(json!({"card_id": c.id, "title": c.title, "done": c.done})))
            }
            "boards_move_card" => {
                let card = core.card(s(a, "card_id")?).await?;
                let stack = self.stack_id(ctx, &card.board_id, s(a, "stack")?).await?;
                let pos = a.get("position").and_then(Value::as_u64).map(|p| p as usize).unwrap_or(usize::MAX);
                core.move_card(uid, &card.id, &stack, pos).await?;
                Ok(ToolResult::text("Moved."))
            }
            "boards_assign" => {
                let on = !a.get("remove").and_then(Value::as_bool).unwrap_or(false);
                core.set_card_assignee(uid, s(a, "card_id")?, s(a, "username")?, on).await?;
                Ok(ToolResult::text(if on { "Assigned." } else { "Unassigned." }))
            }
            "boards_comment" => {
                let c = core.add_card_comment(uid, s(a, "card_id")?, s(a, "text")?).await?;
                Ok(ToolResult::json(json!({"comment_id": c.id})))
            }
            "boards_link" => {
                let l = core.add_card_link(uid, s(a, "card_id")?, s(a, "kind")?, s(a, "target")?, None).await?;
                Ok(ToolResult::json(json!({"link_id": l.id, "title": l.title})))
            }
            "boards_my_cards" => {
                let cards: Vec<Value> = core
                    .my_cards(uid)
                    .await?
                    .into_iter()
                    .map(|c| json!({"id": c.id, "board_id": c.board_id, "title": c.title, "due": c.due_at}))
                    .collect();
                Ok(ToolResult::json(json!({"cards": cards})))
            }

            "docs_create" => {
                let kind = DocKind::parse(s(a, "kind")?).ok_or_else(|| Error::bad("kind must be text, sheet, mindmap or canvas"))?;
                let folder = core.mkdir_p(uid, opt(a, "folder").unwrap_or("/")).await?;
                let n = core.create_document(uid, &folder.id, s(a, "title")?, kind).await?;
                Ok(ToolResult::json(self.describe_node(ctx, &n).await?))
            }
            "docs_read" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                let kind = DocKind::for_name(&n.name).ok_or_else(|| Error::bad("not a live document; use files_read"))?;
                let data = core.collab_content(uid, &n.id).await?;
                match kind {
                    DocKind::Text => Ok(ToolResult::text(String::from_utf8_lossy(&data).into_owned())),
                    DocKind::Sheet => Ok(ToolResult::json(json!({"csv": String::from_utf8_lossy(&data), "note": "Row 1 is A1, B1, …"}))),
                    DocKind::Mindmap => {
                        let v: Value = serde_json::from_slice(&data).unwrap_or(json!({}));
                        Ok(ToolResult::json(json!({"outline": codec::mindmap_outline(&v), "nodes": v["nodes"]})))
                    }
                    DocKind::Canvas => {
                        let v: Value = serde_json::from_slice(&data).unwrap_or(json!({}));
                        Ok(ToolResult::json(json!({"items": v["items"], "edges": v["edges"]})))
                    }
                }
            }
            "docs_append" => {
                let n = self.live(ctx, s(a, "path")?, DocKind::Text).await?;
                let text = s(a, "text")?.to_string();
                core.collab_edit(uid, &n.id, move |d| {
                    let t = d.get_or_insert_text("content");
                    let mut txn = d.transact_mut();
                    let cur = t.get_string(&txn);
                    let sep = if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" };
                    let len = t.len(&txn);
                    t.insert(&mut txn, len, &format!("{sep}{text}"));
                    Ok(())
                })
                .await?;
                Ok(ToolResult::text("Appended."))
            }
            "docs_replace" => {
                let n = self.live(ctx, s(a, "path")?, DocKind::Text).await?;
                let find = s(a, "find")?.to_string();
                let replace = a.get("replace").and_then(Value::as_str).unwrap_or("").to_string();
                let all = a.get("all").and_then(Value::as_bool).unwrap_or(false);
                let count = core
                    .collab_edit(uid, &n.id, move |d| {
                        let t = d.get_or_insert_text("content");
                        let mut txn = d.transact_mut();
                        let mut count = 0;
                        let mut from = 0usize;
                        loop {
                            let cur = t.get_string(&txn);
                            let Some(pos) = cur[from..].find(&find).map(|p| p + from) else {
                                break;
                            };
                            // Documents use yrs' default OffsetKind::Bytes: offsets are UTF-8 byte positions.
                            t.remove_range(&mut txn, pos as u32, find.len() as u32);
                            t.insert(&mut txn, pos as u32, &replace);
                            count += 1;
                            from = pos + replace.len();
                            if !all {
                                break;
                            }
                        }
                        Ok(count)
                    })
                    .await?;
                if count == 0 {
                    return Err(Error::bad("text not found"));
                }
                Ok(ToolResult::text(format!("Replaced {count} occurrence(s).")))
            }
            "sheet_set" => {
                let n = self.live(ctx, s(a, "path")?, DocKind::Sheet).await?;
                let cells = a
                    .get("cells")
                    .and_then(Value::as_object)
                    .ok_or_else(|| Error::bad("`cells` must be an object like {\"A1\": \"x\"}"))?
                    .clone();
                let mut parsed = Vec::new();
                for (k, v) in &cells {
                    let (r, c) = parse_a1(k).ok_or_else(|| Error::bad(format!("bad cell reference {k}")))?;
                    let val = match v {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    parsed.push((format!("{r},{c}"), val));
                }
                let count = parsed.len();
                core.collab_edit(uid, &n.id, move |d| {
                    let m = d.get_or_insert_map("cells");
                    let mut txn = d.transact_mut();
                    for (k, v) in parsed {
                        if v.is_empty() {
                            m.remove(&mut txn, &k);
                        } else {
                            m.insert(&mut txn, k, v);
                        }
                    }
                    Ok(())
                })
                .await?;
                Ok(ToolResult::text(format!("Set {count} cell(s).")))
            }
            "mindmap_add" => {
                let n = self.live(ctx, s(a, "path")?, DocKind::Mindmap).await?;
                let parent_hint = opt(a, "parent").map(String::from);
                let nodes = a
                    .get("nodes")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| Error::bad("`nodes` must be an array"))?;
                let created = core
                    .collab_edit(uid, &n.id, move |d| {
                        let map = d.get_or_insert_map("nodes");
                        let mut txn = d.transact_mut();
                        let existing = codec::read_objects(&txn, &map);
                        let root = existing
                            .iter()
                            .find(|o| o["parent"].as_str().unwrap_or("").is_empty())
                            .and_then(|o| o["id"].as_str())
                            .unwrap_or("root")
                            .to_string();
                        let parent = match parent_hint {
                            Some(h) => existing
                                .iter()
                                .find(|o| o["id"] == h.as_str() || o["text"].as_str().is_some_and(|t| t.eq_ignore_ascii_case(&h)))
                                .and_then(|o| o["id"].as_str().map(String::from))
                                .ok_or_else(|| Error::bad(format!("no node {h:?}")))?,
                            None => root,
                        };
                        let mut created = Vec::new();
                        add_mind_nodes(&mut txn, &map, &existing, &parent, &nodes, &mut created, 0)?;
                        Ok(created)
                    })
                    .await?;
                Ok(ToolResult::json(json!({"created": created})))
            }
            "mindmap_update" => {
                let n = self.live(ctx, s(a, "path")?, DocKind::Mindmap).await?;
                let id = s(a, "node_id")?.to_string();
                let a2 = a.clone();
                core.collab_edit(uid, &n.id, move |d| {
                    let map = d.get_or_insert_map("nodes");
                    let mut txn = d.transact_mut();
                    let existing = codec::read_objects(&txn, &map);
                    if !existing.iter().any(|o| o["id"] == id.as_str()) {
                        return Err(Error::bad(format!("no node {id}")));
                    }
                    if a2.get("delete").and_then(Value::as_bool).unwrap_or(false) {
                        let mut doomed = vec![id.clone()];
                        let mut i = 0;
                        while i < doomed.len() {
                            let p = doomed[i].clone();
                            doomed.extend(
                                existing
                                    .iter()
                                    .filter(|o| o["parent"] == p.as_str())
                                    .filter_map(|o| o["id"].as_str().map(String::from)),
                            );
                            i += 1;
                        }
                        for x in doomed {
                            map.remove(&mut txn, &x);
                        }
                        return Ok(());
                    }
                    let Some(yrs::Out::YMap(node)) = map.get(&txn, &id) else {
                        return Err(Error::bad("node is not editable"));
                    };
                    for k in ["text", "color", "link", "parent"] {
                        if let Some(v) = a2.get(k).and_then(Value::as_str) {
                            if k == "parent" && (v == id || !existing.iter().any(|o| o["id"] == v)) {
                                return Err(Error::bad("invalid parent"));
                            }
                            node.insert(&mut txn, k, v);
                        }
                    }
                    Ok(())
                })
                .await?;
                Ok(ToolResult::text("Updated."))
            }
            "canvas_add" => {
                let n = self.live(ctx, s(a, "path")?, DocKind::Canvas).await?;
                let items = a
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| Error::bad("`items` must be an array"))?;
                // Resolve references before entering the (synchronous) document edit.
                let mut prepared = Vec::new();
                for (i, it) in items.iter().enumerate() {
                    let mut obj = it.as_object().cloned().ok_or_else(|| Error::bad("each item must be an object"))?;
                    let ty = obj.get("type").and_then(Value::as_str).unwrap_or("note").to_string();
                    if !matches!(ty.as_str(), "note" | "file" | "mindmap" | "board" | "link" | "frame" | "text") {
                        return Err(Error::bad(format!("unknown item type {ty}")));
                    }
                    if let Some(r) = obj.get("ref").and_then(Value::as_str).map(String::from) {
                        match ty.as_str() {
                            "file" | "mindmap" => {
                                let target = self.node(ctx, &r).await?;
                                obj.insert("ref".into(), json!(target.id));
                                obj.entry("text").or_insert(json!(target.name));
                            }
                            "board" => {
                                let b = self.board_id(ctx, &r).await?;
                                obj.insert("ref".into(), json!(b));
                            }
                            _ => {}
                        }
                    }
                    obj.insert("type".into(), json!(ty));
                    obj.entry("x").or_insert(json!(40 + (i as i64 % 4) * 260));
                    obj.entry("y").or_insert(json!(40 + (i as i64 / 4) * 200));
                    obj.entry("w").or_insert(json!(220));
                    obj.entry("h").or_insert(json!(140));
                    obj.remove("id");
                    prepared.push(obj);
                }
                let ids = core
                    .collab_edit(uid, &n.id, move |d| {
                        let map = d.get_or_insert_map("items");
                        let mut txn = d.transact_mut();
                        let mut ids = Vec::new();
                        for obj in prepared {
                            let id = format!("i{}", short_token(8));
                            codec::put_object(&mut txn, &map, &id, &obj);
                            ids.push(id);
                        }
                        Ok(ids)
                    })
                    .await?;
                Ok(ToolResult::json(json!({"created": ids})))
            }
            "canvas_connect" => {
                let n = self.live(ctx, s(a, "path")?, DocKind::Canvas).await?;
                let (from, to) = (s(a, "from")?.to_string(), s(a, "to")?.to_string());
                let label = opt(a, "label").unwrap_or("").to_string();
                let id = core
                    .collab_edit(uid, &n.id, move |d| {
                        let items = d.get_or_insert_map("items");
                        let edges = d.get_or_insert_map("edges");
                        let mut txn = d.transact_mut();
                        if items.get(&txn, &from).is_none() || items.get(&txn, &to).is_none() {
                            return Err(Error::bad("both items must exist on the canvas"));
                        }
                        let id = format!("e{}", short_token(8));
                        let obj = json!({"from": from, "to": to, "label": label});
                        codec::put_object(&mut txn, &edges, &id, obj.as_object().expect("object literal"));
                        Ok(id)
                    })
                    .await?;
                Ok(ToolResult::json(json!({"edge_id": id})))
            }

            "calendar_events" => {
                let from = match opt(a, "from") {
                    Some(f) => parse_when(f)?.0,
                    None => Utc::now(),
                };
                let to = match opt(a, "to") {
                    Some(t) => parse_when(t)?.0,
                    None => from + chrono::Duration::days(7),
                };
                Ok(ToolResult::json(
                    json!({"from": from, "to": to, "events": core.events_between(uid, from, to).await?}),
                ))
            }
            "calendar_create_event" => {
                let (start, all_day) = parse_when(s(a, "start")?)?;
                let end = opt(a, "end").map(parse_when).transpose()?.map(|e| e.0);
                let e = ferrite_core::groupware::EventInput {
                    summary: s(a, "summary")?.to_string(),
                    start,
                    end,
                    all_day,
                    description: opt(a, "description").map(String::from),
                    location: opt(a, "location").map(String::from),
                    calendar: opt(a, "calendar").map(String::from),
                };
                Ok(ToolResult::json(json!({"event": core.create_event(uid, &e).await?})))
            }
            "calendar_update_event" => {
                let id = s(a, "event_id")?;
                let far = Utc::now() + chrono::Duration::days(365 * 50);
                let current = core
                    .events_between(uid, chrono::DateTime::<Utc>::MIN_UTC, far)
                    .await?
                    .into_iter()
                    .find(|e| e.id == id)
                    .ok_or(Error::NotFound)?;
                let (start, all_day) = match opt(a, "start") {
                    Some(t) => parse_when(t)?,
                    None => (current.starts_at.ok_or_else(|| Error::bad("event has no start"))?, current.all_day),
                };
                let end = match opt(a, "end") {
                    Some(t) => Some(parse_when(t)?.0),
                    // Keep the duration when only the start moves.
                    None => match (current.starts_at, current.ends_at) {
                        (Some(s0), Some(e0)) => Some(start + (e0 - s0)),
                        _ => None,
                    },
                };
                let e = ferrite_core::groupware::EventInput {
                    summary: opt(a, "summary").map(String::from).unwrap_or(current.summary),
                    start,
                    end,
                    all_day,
                    description: Some(opt(a, "description").map(String::from).unwrap_or(current.description)),
                    location: Some(opt(a, "location").map(String::from).unwrap_or(current.location)),
                    calendar: None,
                };
                Ok(ToolResult::json(json!({"event": core.update_event(uid, id, &e).await?})))
            }
            "calendar_delete_event" => {
                core.delete_event(uid, s(a, "event_id")?).await?;
                Ok(ToolResult::text("Deleted."))
            }
            "contacts_search" => Ok(ToolResult::json(json!({"contacts": core.contacts(uid, opt(a, "query")).await?}))),
            "contacts_create" => {
                let c = ferrite_core::groupware::ContactInput {
                    name: s(a, "name")?.to_string(),
                    emails: strings(a, "emails"),
                    phones: strings(a, "phones"),
                    org: opt(a, "org").map(String::from),
                    note: opt(a, "note").map(String::from),
                    addressbook: opt(a, "addressbook").map(String::from),
                };
                Ok(ToolResult::json(json!({"contact": core.create_contact(uid, &c).await?})))
            }
            "contacts_update" => {
                let c = ferrite_core::groupware::ContactInput {
                    name: s(a, "name")?.to_string(),
                    emails: strings(a, "emails"),
                    phones: strings(a, "phones"),
                    org: opt(a, "org").map(String::from),
                    note: opt(a, "note").map(String::from),
                    addressbook: None,
                };
                Ok(ToolResult::json(json!({"contact": core.update_contact(uid, s(a, "contact_id")?, &c).await?})))
            }
            "search" => {
                let kinds = strings(a, "kinds");
                let kinds_ref: Vec<&str> = kinds.iter().map(String::as_str).collect();
                let (hits, semantic) = self
                    .semantic
                    .search(uid, s(a, "query")?, if kinds.is_empty() { None } else { Some(&kinds_ref) }, 30)
                    .await?;
                Ok(ToolResult::json(json!({"results": hits, "semantic": semantic})))
            }
            "notifications" => Ok(ToolResult::json(json!({"notifications": core.notifications(uid, true, 50).await?}))),
            "activity" => Ok(ToolResult::json(json!({"activity": core.activity(uid, None, 50).await?}))),
            "users_search" => {
                let users: Vec<Value> = core
                    .list_users(Some(s(a, "query")?), 20)
                    .await?
                    .into_iter()
                    .filter(|u| !u.disabled)
                    .map(|u| json!({"username": u.username, "name": u.display_name}))
                    .collect();
                Ok(ToolResult::json(json!({"users": users})))
            }

            "craft_apps" => Ok(ToolResult::json(json!({"apps": self.craft.availability()}))),
            "craft_open" => {
                let n = self.node(ctx, s(a, "path")?).await?;
                let (session, tools) = self.craft.open(uid, &n.id, opt(a, "app")).await?;
                Ok(ToolResult::json(CraftBridge::describe(&session, &tools)))
            }
            "craft_call" => {
                let args = a.get("arguments").cloned().unwrap_or_else(|| json!({}));
                Ok(self.craft.call(uid, s(a, "session")?, s(a, "tool")?, args).await?)
            }
            "craft_close" => {
                let save = a.get("save").and_then(Value::as_bool).unwrap_or(true);
                let written = self.craft.close(uid, s(a, "session")?, save).await?;
                Ok(ToolResult::json(json!({"saved": written})))
            }
            other => Err(Error::bad(format!("unknown tool {other}"))),
        }
    }
}

/// Parses RFC 3339 date-times or YYYY-MM-DD dates (the latter as all-day).
fn parse_when(v: &str) -> Result<(DateTime<Utc>, bool), Error> {
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Ok((t.with_timezone(&Utc), false));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(|t| (t.and_utc(), true)).ok_or_else(|| Error::bad("invalid date"));
    }
    Err(Error::bad(format!("{v:?} is not RFC 3339 (2026-11-05T14:00:00Z) or YYYY-MM-DD")))
}

fn parse_due(s: Option<&str>) -> Result<Option<DateTime<Utc>>, Error> {
    match s {
        None => Ok(None),
        Some(d) => {
            if let Ok(t) = DateTime::parse_from_rfc3339(d) {
                return Ok(Some(t.with_timezone(&Utc)));
            }
            if let Ok(date) = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d") {
                return Ok(date.and_hms_opt(17, 0, 0).map(|t| t.and_utc()));
            }
            Err(Error::bad("due must be RFC 3339 (2026-11-02T17:00:00Z) or YYYY-MM-DD"))
        }
    }
}

fn add_mind_nodes(
    txn: &mut yrs::TransactionMut,
    map: &yrs::MapRef,
    existing: &[Value],
    parent: &str,
    nodes: &[Value],
    created: &mut Vec<Value>,
    depth: usize,
) -> Result<(), Error> {
    if depth > 16 {
        return Err(Error::bad("mind map nesting is too deep"));
    }
    let mut order = existing
        .iter()
        .filter(|o| o["parent"] == parent)
        .filter_map(|o| o["order"].as_f64())
        .fold(0.0f64, f64::max);
    for n in nodes {
        let text = n.get("text").and_then(Value::as_str).or_else(|| n.as_str()).unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        order += 1.0;
        let id = format!("n{}", short_token(8));
        let mut obj = serde_json::Map::new();
        obj.insert("parent".into(), json!(parent));
        obj.insert("text".into(), json!(text));
        obj.insert("order".into(), json!(order));
        for k in ["color", "link", "note"] {
            if let Some(v) = n.get(k).and_then(Value::as_str) {
                obj.insert(k.into(), json!(v));
            }
        }
        codec::put_object(txn, map, &id, &obj);
        created.push(json!({"id": id, "text": text, "parent": parent}));
        if let Some(children) = n.get("children").and_then(Value::as_array) {
            add_mind_nodes(txn, map, &[], &id, children, created, depth + 1)?;
        }
    }
    Ok(())
}

/// MCP handler: Ferrite's tools, resources and prompts for an authenticated user.
pub struct McpHandler {
    pub tools: Arc<FerriteTools>,
}

impl Handler for McpHandler {
    type Ctx = UserCtx;

    fn server_info(&self) -> Implementation {
        Implementation {
            name: "ferrite".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("Ferrite".into()),
        }
    }

    fn instructions(&self) -> Option<String> {
        Some(
            "Ferrite is a self-hosted collaboration server: files and sharing, Talk (chat and calls), kanban boards, calendars and contacts, \
             live documents (Markdown text, CSV sheets, mind maps, canvases that arrange files and mind maps), and the \
             ArtCraft Crafting Apps for Office/Adobe-style documents. Paths are relative to the user's home. \
             Prefer docs_* tools for live documents so people editing see changes instantly."
                .into(),
        )
    }

    async fn list_tools(&self, ctx: &UserCtx) -> Vec<Tool> {
        self.tools.list(ctx)
    }

    async fn call_tool(&self, ctx: &UserCtx, name: &str, args: Value) -> Result<ToolResult, RpcError> {
        self.tools.call(ctx, name, args).await
    }

    async fn list_resources(&self, ctx: &UserCtx) -> Vec<Resource> {
        let core = &self.tools.core;
        let mut out = Vec::new();
        if let Ok(files) = core.recent_files(&ctx.user.id, 50).await {
            for n in files {
                if let Ok(path) = core.display_path(&ctx.user.id, &n).await {
                    out.push(Resource {
                        uri: format!("ferrite://files{path}"),
                        name: n.name.clone(),
                        title: None,
                        description: Some(format!("Modified {}", n.modified_at.format("%Y-%m-%d %H:%M"))),
                        mime_type: Some(n.mime.clone()),
                        size: Some(n.size),
                    });
                }
            }
        }
        if let Ok(rooms) = core.list_rooms(&ctx.user.id).await {
            for r in rooms.into_iter().take(30) {
                out.push(Resource {
                    uri: format!("ferrite://talk/{}", r.room.token),
                    name: r.display_name,
                    title: None,
                    description: Some("Conversation transcript".into()),
                    mime_type: Some("text/plain".into()),
                    size: None,
                });
            }
        }
        out
    }

    fn resource_templates(&self) -> Vec<ResourceTemplate> {
        vec![
            ResourceTemplate {
                uri_template: "ferrite://files/{+path}".into(),
                name: "Files".into(),
                title: Some("Ferrite files".into()),
                description: Some("Any file you can access".into()),
                mime_type: None,
            },
            ResourceTemplate {
                uri_template: "ferrite://talk/{room}".into(),
                name: "Conversations".into(),
                title: Some("Talk transcripts".into()),
                description: Some("Recent messages of a conversation".into()),
                mime_type: Some("text/plain".into()),
            },
        ]
    }

    async fn read_resource(&self, ctx: &UserCtx, uri: &str) -> Result<Vec<ResourceContents>, RpcError> {
        let core = &self.tools.core;
        let not_found = || RpcError::invalid_params("Resource not found").with_data(json!({"uri": uri}));
        if let Some(path) = uri.strip_prefix("ferrite://files") {
            let path = percent_encoding::percent_decode_str(path).decode_utf8_lossy().into_owned();
            let n = core.resolve_path(&ctx.user.id, &path).await.map_err(|_| not_found())?;
            if n.is_dir() {
                let entries = core.list_dir(&ctx.user.id, &n.id).await.map_err(|_| not_found())?;
                let text = entries
                    .iter()
                    .map(|e| format!("{}{}", e.node.name, if e.node.is_dir() { "/" } else { "" }))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Ok(vec![ResourceContents {
                    uri: uri.into(),
                    mime_type: Some("inode/directory".into()),
                    text: Some(text),
                    blob: None,
                }]);
            }
            let data = if DocKind::for_name(&n.name).is_some() {
                core.collab_content(&ctx.user.id, &n.id).await
            } else {
                core.read_file(&ctx.user.id, &n.id).await.map(|r| r.1)
            }
            .map_err(|_| not_found())?;
            return Ok(vec![match String::from_utf8(data) {
                Ok(text) => ResourceContents {
                    uri: uri.into(),
                    mime_type: Some(n.mime),
                    text: Some(text),
                    blob: None,
                },
                Err(e) => {
                    use base64::Engine;
                    ResourceContents {
                        uri: uri.into(),
                        mime_type: Some(n.mime),
                        text: None,
                        blob: Some(base64::engine::general_purpose::STANDARD.encode(e.into_bytes())),
                    }
                }
            }]);
        }
        if let Some(token) = uri.strip_prefix("ferrite://talk/") {
            let actor = Actor::user(&ctx.user.id, &ctx.user.display_name);
            let msgs = core.list_messages(&actor, token, None, None, 100).await.map_err(|_| not_found())?;
            let text = msgs
                .iter()
                .filter(|m| !m.message.deleted)
                .map(|m| {
                    format!(
                        "[{}] {}: {}",
                        m.message.created_at.format("%Y-%m-%d %H:%M"),
                        m.message.actor_name,
                        m.message.body
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(vec![ResourceContents {
                uri: uri.into(),
                mime_type: Some("text/plain".into()),
                text: Some(text),
                blob: None,
            }]);
        }
        Err(not_found())
    }

    fn list_prompts(&self) -> Vec<Prompt> {
        let arg = |n: &str, d: &str, r: bool| PromptArgument {
            name: n.into(),
            description: Some(d.into()),
            required: r,
        };
        vec![
            Prompt {
                name: "summarize_conversation".into(),
                title: Some("Summarize a conversation".into()),
                description: Some("Summarize a Talk conversation with decisions and action items".into()),
                arguments: vec![arg("room", "Room token or name", true)],
            },
            Prompt {
                name: "board_standup".into(),
                title: Some("Board stand-up".into()),
                description: Some("Status report of a kanban board: done, in progress, blocked, overdue".into()),
                arguments: vec![arg("board", "Board id or title", true)],
            },
            Prompt {
                name: "mindmap_from_document".into(),
                title: Some("Mind map from a document".into()),
                description: Some("Turn a document into a new mind map next to it".into()),
                arguments: vec![arg("path", "Document path", true)],
            },
        ]
    }

    async fn get_prompt(&self, _ctx: &UserCtx, name: &str, args: Value) -> Result<Value, RpcError> {
        let get = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| RpcError::invalid_params(format!("missing argument {k}")))
        };
        let text = match name {
            "summarize_conversation" => format!(
                "Use talk_read to read the conversation \"{}\" (page back with `before` if needed), then summarize it: key points, decisions, open questions, and action items with owners.",
                get("room")?
            ),
            "board_standup" => format!(
                "Use boards_get on the board \"{}\" and write a short stand-up: done, in progress, blocked, overdue (due dates before today), and unassigned work.",
                get("board")?
            ),
            "mindmap_from_document" => format!(
                "Read \"{}\" with files_read, then create a mind map next to it with docs_create (kind mindmap) and add the document's structure with mindmap_add: main themes as branches, key points as children.",
                get("path")?
            ),
            other => return Err(RpcError::invalid_params(format!("Unknown prompt: {other}"))),
        };
        Ok(json!({"messages": [{"role": "user", "content": {"type": "text", "text": text}}]}))
    }
}

/// The tool set as the assistant sees it: Ferrite's tools plus connected MCP servers.
pub struct UserToolBox {
    pub tools: Arc<FerriteTools>,
    pub ctx: UserCtx,
}

impl ferrite_ai::ToolBox for UserToolBox {
    async fn tools(&self) -> Vec<Tool> {
        let mut t = self.tools.list(&self.ctx);
        t.extend(self.tools.external.tools(&self.ctx.user.id).await);
        t
    }

    async fn call(&self, name: &str, args: Value) -> ToolResult {
        if name.starts_with("ext_") {
            return self
                .tools
                .external
                .call(&self.ctx.user.id, name, args)
                .await
                .unwrap_or_else(|| ToolResult::error("unknown external tool"));
        }
        match self.tools.call(&self.ctx, name, args).await {
            Ok(r) => r,
            Err(e) => ToolResult::error(e.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_a1;

    #[test]
    fn a1_references() {
        assert_eq!(parse_a1("A1"), Some((0, 0)));
        assert_eq!(parse_a1("b3"), Some((2, 1)));
        assert_eq!(parse_a1("AA10"), Some((9, 26)));
        assert_eq!(parse_a1("A0"), None);
        assert_eq!(parse_a1("1A"), None);
    }
}
