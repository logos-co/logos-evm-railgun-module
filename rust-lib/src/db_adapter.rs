//! Adapter B — a disk-backed [`Database`] for the RAILGUN engine, so synced
//! UTXO/TXID/Merkle/account state survives a module restart (the default
//! `MemoryDatabase` is ephemeral; the engine's own `FilesystemDatabase` is
//! commented out upstream).
//!
//! Each key is one file under the module's per-instance persistence dir
//! (`RustModuleContext.instance_persistence_path`), named by the keccak256 of
//! the key (the engine's keys can exceed the OS filename limit, and the
//! `Database` contract is exact get/set/delete only — no enumeration — so a
//! fixed-length collision-resistant filename is safe). Writes are atomic
//! (temp + rename). The persisted blobs include decrypted note data, so this dir
//! is sensitive — it stays under the instance dir and the keys never land here.

use std::io::ErrorKind;
use std::path::PathBuf;

use alloy::primitives::keccak256;
use railgun::database::{Database, DatabaseError};

pub struct DiskDatabase {
    dir: PathBuf,
}

impl DiskDatabase {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, String> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create db dir {dir:?}: {e}"))?;
        Ok(Self { dir })
    }

    fn path(&self, key: &[u8]) -> PathBuf {
        self.dir.join(hex::encode(keccak256(key)))
    }
}

fn storage(e: std::io::Error) -> DatabaseError {
    DatabaseError::StorageError(e.to_string())
}

#[async_trait::async_trait]
impl Database for DiskDatabase {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DatabaseError> {
        match std::fs::read(self.path(key)) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(storage(e)),
        }
    }

    async fn set(&self, key: &[u8], value: &[u8]) -> Result<(), DatabaseError> {
        let path = self.path(key);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, value).map_err(storage)?;
        std::fs::rename(&tmp, &path).map_err(storage)?;
        Ok(())
    }

    async fn delete(&self, key: &[u8]) -> Result<(), DatabaseError> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(storage(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_get_delete_roundtrip() {
        let dir = std::env::temp_dir().join(format!("railgun-db-{}", std::process::id()));
        let db = DiskDatabase::new(&dir).unwrap();

        assert_eq!(db.get(b"missing").await.unwrap(), None);

        db.set(b"utxo_tree_0", b"\x01\x02\x03").await.unwrap();
        assert_eq!(db.get(b"utxo_tree_0").await.unwrap(), Some(vec![1, 2, 3]));

        // Overwrite is atomic.
        db.set(b"utxo_tree_0", b"\x04").await.unwrap();
        assert_eq!(db.get(b"utxo_tree_0").await.unwrap(), Some(vec![4]));

        db.delete(b"utxo_tree_0").await.unwrap();
        assert_eq!(db.get(b"utxo_tree_0").await.unwrap(), None);
        db.delete(b"utxo_tree_0").await.unwrap(); // delete-missing is ok

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn persists_across_instances() {
        let dir = std::env::temp_dir().join(format!("railgun-db-persist-{}", std::process::id()));
        {
            let db = DiskDatabase::new(&dir).unwrap();
            db.set(b"account", b"state").await.unwrap();
        }
        let db2 = DiskDatabase::new(&dir).unwrap();
        assert_eq!(db2.get(b"account").await.unwrap(), Some(b"state".to_vec()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
