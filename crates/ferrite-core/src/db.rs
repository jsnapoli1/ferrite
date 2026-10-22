use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::path::Path;
use std::time::Duration;

/// Opens (creating if needed) the SQLite database and applies migrations.
///
/// WAL mode lets readers proceed while one writer commits, which is all a single-node
/// collaboration server needs; `synchronous=NORMAL` is durable across application crashes.
pub async fn open(path: &Path) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10))
        .pragma("temp_store", "memory")
        .pragma("cache_size", "-16000");
    let pool = SqlitePoolOptions::new().max_connections(8).min_connections(1).connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
