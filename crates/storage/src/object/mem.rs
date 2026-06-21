//! `MemObjectStore` — an in-process [`ObjectStore`] with strong CAS semantics.
//!
//! Not durable across process exit; its purpose is fast, deterministic tests of
//! the CAS append + fencing logic (two writers racing one log slot, a stale
//! writer fencing itself off) without touching disk. The durability gate uses
//! [`FsObjectStore`](super::FsObjectStore) instead.

use super::store::{ETag, GetResult, ObjectError, ObjectStore};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// In-memory key→(bytes, etag) map behind one lock. The lock makes each
/// conditional op atomic, exactly as an object store's CAS is atomic.
#[derive(Default)]
pub struct MemObjectStore {
    map: Mutex<BTreeMap<String, (Vec<u8>, ETag)>>,
    etag_seq: AtomicU64,
}

impl MemObjectStore {
    pub fn new() -> MemObjectStore {
        MemObjectStore::default()
    }

    fn next_etag(&self) -> ETag {
        format!("e{}", self.etag_seq.fetch_add(1, Ordering::Relaxed))
    }
}

#[async_trait]
impl ObjectStore for MemObjectStore {
    async fn get(&self, key: &str) -> Result<Option<GetResult>, ObjectError> {
        let map = self.map.lock().unwrap();
        Ok(map.get(key).map(|(bytes, etag)| GetResult {
            bytes: bytes.clone(),
            etag: etag.clone(),
        }))
    }

    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        let mut map = self.map.lock().unwrap();
        if map.contains_key(key) {
            return Err(ObjectError::Precondition(key.to_string()));
        }
        let etag = self.next_etag();
        map.insert(key.to_string(), (bytes.to_vec(), etag.clone()));
        Ok(etag)
    }

    async fn put_if_match(
        &self,
        key: &str,
        bytes: &[u8],
        etag: &ETag,
    ) -> Result<ETag, ObjectError> {
        let mut map = self.map.lock().unwrap();
        match map.get(key) {
            Some((_, cur)) if cur == etag => {
                let new = self.next_etag();
                map.insert(key.to_string(), (bytes.to_vec(), new.clone()));
                Ok(new)
            }
            _ => Err(ObjectError::Precondition(key.to_string())),
        }
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<ETag, ObjectError> {
        let mut map = self.map.lock().unwrap();
        let etag = self.next_etag();
        map.insert(key.to_string(), (bytes.to_vec(), etag.clone()));
        Ok(etag)
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        self.map.lock().unwrap().remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, ObjectError> {
        let map = self.map.lock().unwrap();
        Ok(map
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}
