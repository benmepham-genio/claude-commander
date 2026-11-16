//! Per-key locks shared by cache misses. Weak entries do not retain dead keys.
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Mutex as AsyncMutex;

pub(crate) struct Flights<K>(Mutex<HashMap<K, Weak<AsyncMutex<()>>>>);
impl<K> Default for Flights<K> {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}
impl<K: Eq + Hash> Flights<K> {
    pub fn for_key(&self, key: K) -> Arc<AsyncMutex<()>> {
        let mut flights = self.0.lock().expect("cache flights poisoned");
        flights.retain(|_, flight| flight.strong_count() != 0);
        let flight = flights.entry(key).or_default();
        if let Some(lock) = flight.upgrade() {
            return lock;
        }
        let lock = Arc::new(AsyncMutex::new(()));
        *flight = Arc::downgrade(&lock);
        lock
    }
}
