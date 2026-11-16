//! Bounded, caller-keyed cache slots. Holding a slot coalesces concurrent work;
//! eviction only removes one idle slot and cannot split an in-flight request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::Mutex as AsyncMutex;
use tw_types::Msg;

type Slot<T> = Arc<AsyncMutex<Option<(Instant, T)>>>;
struct Entry<T> {
    used: Instant,
    slot: Slot<T>,
}

pub(super) struct Cache<T> {
    capacity: usize,
    entries: Mutex<HashMap<String, Entry<T>>>,
}

impl<T> Cache<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: String) -> Result<Slot<T>, Msg> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get_mut(&key) {
            entry.used = Instant::now();
            return Ok(entry.slot.clone());
        }
        if entries.len() >= self.capacity {
            let oldest = entries
                .iter()
                .filter(|(_, e)| Arc::strong_count(&e.slot) == 1)
                .min_by_key(|(_, e)| e.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                return Err(super::error("too many pending web operations"));
            }
        }
        let slot = Arc::new(AsyncMutex::new(None));
        entries.insert(
            key,
            Entry {
                used: Instant::now(),
                slot: slot.clone(),
            },
        );
        Ok(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn eviction_retains_hot_values_and_coalesces_in_flight_work() {
        let cache = Cache::new(2);
        let first = cache.get("first".into()).unwrap();
        *first.lock().await = Some((Instant::now(), "saved"));
        let pending = cache.get("pending".into()).unwrap();
        let guard = pending.lock().await;
        let joined = cache.get("pending".into()).unwrap();
        assert!(Arc::ptr_eq(&pending, &joined));
        assert!(joined.try_lock().is_err());
        assert!(
            cache.get("new".into()).is_err(),
            "active work cannot be evicted"
        );
        drop(guard);
        drop(joined);
        drop(pending);
        drop(first);
        // Touch the completed result: inserting another key must evict the older
        // idle slot, not flush all successfully cached results.
        drop(cache.get("first".into()).unwrap());
        drop(cache.get("new".into()).unwrap());
        let first = cache.get("first".into()).unwrap();
        assert_eq!(first.lock().await.as_ref().unwrap().1, "saved");
        assert_eq!(cache.entries.lock().unwrap().len(), 2);
    }
}
