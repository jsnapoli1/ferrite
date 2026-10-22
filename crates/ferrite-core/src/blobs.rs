//! Content-addressed blob storage. Files are stored once per distinct content under
//! `blobs/<2 hex>/<2 hex>/<blake3>`; nodes and versions reference them with a refcount, so
//! copies, versions that didn't change, and duplicate uploads cost no extra disk.

use crate::{Error, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{OwnedRwLockReadGuard, RwLock};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub struct BlobStore {
    root: PathBuf,
    tmp: PathBuf,
    /// Writers hold a read pin from writing a blob until the reference is committed; garbage
    /// collection takes the write side, so it can never delete a blob that is about to be
    /// referenced.
    gc_lock: Arc<RwLock<()>>,
}

/// Keeps freshly written blobs safe from garbage collection until dropped.
pub type BlobPin = OwnedRwLockReadGuard<()>;

#[derive(Debug, Clone)]
pub struct StoredBlob {
    pub hash: String,
    pub size: i64,
}

impl BlobStore {
    pub fn new(data_dir: &Path) -> std::io::Result<Self> {
        let root = data_dir.join("blobs");
        let tmp = data_dir.join("tmp");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&tmp)?;
        Ok(Self { root, tmp, gc_lock: Arc::new(RwLock::new(())) })
    }

    pub fn path(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[0..2]).join(&hash[2..4]).join(hash)
    }

    pub fn tmp_dir(&self) -> &Path {
        &self.tmp
    }

    /// Pins the store against garbage collection; hold the pin until blob references commit.
    pub async fn pin(&self) -> BlobPin {
        self.gc_lock.clone().read_owned().await
    }

    /// Callers must hold a [`BlobPin`] from before this call until the reference commits.
    ///
    /// Streams `reader` to a temp file while hashing, then moves it into place. Aborts once more
    /// than `limit` bytes arrive so an oversized upload never fills the disk.
    pub async fn write_reader<R: AsyncRead + Unpin>(&self, mut reader: R, limit: u64) -> Result<StoredBlob> {
        let tmp_path = self.tmp.join(crate::util::new_id());
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut size: u64 = 0;
        let result: Result<()> = async {
            loop {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > limit {
                    return Err(Error::bad("upload exceeds the maximum size"));
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n]).await?;
            }
            file.flush().await?;
            file.sync_data().await?;
            Ok(())
        }
        .await;
        drop(file);
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e);
        }
        let hash = hasher.finalize().to_hex().to_string();
        let dest = self.path(&hash);
        if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            tokio::fs::remove_file(&tmp_path).await?;
        } else {
            tokio::fs::create_dir_all(dest.parent().expect("blob path has a parent")).await?;
            tokio::fs::rename(&tmp_path, &dest).await?;
        }
        Ok(StoredBlob { hash, size: size as i64 })
    }

    pub async fn write_bytes(&self, data: &[u8]) -> Result<StoredBlob> {
        self.write_reader(data, u64::MAX).await
    }

    pub async fn read(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(tokio::fs::read(self.path(hash)).await?)
    }

    pub async fn open(&self, hash: &str) -> Result<tokio::fs::File> {
        Ok(tokio::fs::File::open(self.path(hash)).await?)
    }

    /// Records one more reference to a blob inside the caller's transaction.
    pub async fn add_ref(tx: &mut Transaction<'_, Sqlite>, blob: &StoredBlob) -> Result<()> {
        sqlx::query("INSERT INTO blobs (hash, size, refcount) VALUES (?, ?, 1) ON CONFLICT(hash) DO UPDATE SET refcount = refcount + 1")
            .bind(&blob.hash)
            .bind(blob.size)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn add_ref_hash(tx: &mut Transaction<'_, Sqlite>, hash: &str) -> Result<()> {
        sqlx::query("UPDATE blobs SET refcount = refcount + 1 WHERE hash = ?").bind(hash).execute(&mut **tx).await?;
        Ok(())
    }

    pub async fn release(tx: &mut Transaction<'_, Sqlite>, hash: &str) -> Result<()> {
        sqlx::query("UPDATE blobs SET refcount = refcount - 1 WHERE hash = ?").bind(hash).execute(&mut **tx).await?;
        Ok(())
    }

    /// Deletes unreferenced blobs from disk and the index.
    pub async fn collect_garbage(&self, db: &SqlitePool) -> Result<usize> {
        let _exclusive = self.gc_lock.write().await;
        let dead: Vec<String> = sqlx::query_scalar("SELECT hash FROM blobs WHERE refcount <= 0").fetch_all(db).await?;
        for h in &dead {
            match tokio::fs::remove_file(self.path(h)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            sqlx::query("DELETE FROM blobs WHERE hash = ? AND refcount <= 0").bind(h).execute(db).await?;
        }
        Ok(dead.len())
    }
}
