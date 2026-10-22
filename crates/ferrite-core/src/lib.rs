//! Ferrite core: storage, identity, files, sharing, Talk, boards, live documents, search.
//!
//! All services are methods on [`Core`], a cheap-to-clone handle shared by the HTTP server,
//! the WebDAV layer, the MCP server and the AI assistant, so every surface enforces exactly
//! the same permissions.

pub mod auth;
pub mod blobs;
pub mod boards;
pub mod collab;
pub mod config;
pub mod craft;
pub mod db;
pub mod error;
pub mod events;
pub mod files;
pub mod groupware;
pub mod notify;
pub mod search;
pub mod shares;
pub mod talk;
pub mod util;

pub use config::Config;
pub use error::{Error, Result};

use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct Inner {
    pub db: sqlx::SqlitePool,
    pub config: Arc<Config>,
    pub blobs: blobs::BlobStore,
    pub hub: events::Hub,
    pub talk: talk::TalkState,
    pub collab: collab::CollabManager,
    /// Every new chat message, for bots and integrations (the AI assistant listens here).
    pub message_events: broadcast::Sender<Arc<talk::Message>>,
    /// Server secret used to derive unlock proofs; generated on first start.
    pub secret: String,
}

#[derive(Clone)]
pub struct Core(Arc<Inner>);

impl Deref for Core {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl Core {
    pub async fn open(config: Config) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&config.data_dir)?;
        let db = db::open(&config.data_dir.join("ferrite.db")).await?;
        let blobs = blobs::BlobStore::new(&config.data_dir)?;
        let secret = match sqlx::query_scalar::<_, String>("SELECT value FROM kv WHERE key = 'secret'")
            .fetch_optional(&db)
            .await?
        {
            Some(s) => s,
            None => {
                let s = util::random_token(32);
                sqlx::query("INSERT INTO kv (key, value) VALUES ('secret', ?)").bind(&s).execute(&db).await?;
                s
            }
        };
        let (message_events, _) = broadcast::channel(1024);
        Ok(Core(Arc::new(Inner {
            db,
            config: Arc::new(config),
            blobs,
            hub: events::Hub::default(),
            talk: talk::TalkState::default(),
            collab: collab::CollabManager::default(),
            message_events,
            secret,
        })))
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM kv WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value)
            .execute(&self.db)
            .await?;
        Ok(())
    }
}
