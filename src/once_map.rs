//! A process-lifetime map whose value for each key is built at most once.
//!
//! The extraction caches (compiled queries per language, stack-graph engines) are first used by
//! every scan worker at the same moment. A check-then-build cache lets each of them miss, build
//! its own copy and race to insert it: N compiles per language at scan start, and where the copy
//! was leaked to get a `'static` borrow, N − 1 unreachable allocations. Here a builder for a key
//! holds that key's gate, so concurrent first users wait for the one build and share its result.
//!
//! A built value is cached whatever it is, a failure included, when the builder returns it as the
//! value (`V = Result<_, _>`): a query that does not compile is compiled once, not once per file.
//! An `Err` from the builder itself is not cached, for failures the next call may not repeat.

use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use ahash::AHashMap;

/// Build-once map. Reads after the first build take one shared read lock.
pub(crate) struct OnceMap<K, V> {
    ready: RwLock<AHashMap<K, V>>,
    building: Mutex<AHashMap<K, Arc<Mutex<()>>>>,
}

impl<K: Hash + Eq + Clone, V: Clone> Default for OnceMap<K, V> {
    fn default() -> Self {
        Self {
            ready: RwLock::new(AHashMap::new()),
            building: Mutex::new(AHashMap::new()),
        }
    }
}

impl<K: Hash + Eq + Clone, V: Clone> OnceMap<K, V> {
    fn ready(&self, key: &K) -> Option<V> {
        self.ready
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    /// The value for `key`, built by `init` if no call has built it yet. Concurrent callers for the
    /// same key run `init` once between them; an `Err` from `init` is returned and not cached.
    pub(crate) fn get_or_try_init<E>(&self, key: K, init: impl FnOnce() -> Result<V, E>) -> Result<V, E> {
        if let Some(value) = self.ready(&key) {
            return Ok(value);
        }
        let gate = Arc::clone(
            self.building
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(key.clone())
                .or_default(),
        );
        // A builder that panicked poisons the gate but cached nothing: the next holder builds.
        let _held = gate.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(value) = self.ready(&key) {
            return Ok(value);
        }
        let value = init()?;
        self.ready
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), value.clone());
        self.building
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        Ok(value)
    }

    /// [`Self::get_or_try_init`] for a builder that cannot fail.
    pub(crate) fn get_or_init(&self, key: K, init: impl FnOnce() -> V) -> V {
        match self.get_or_try_init::<std::convert::Infallible>(key, || Ok(init())) {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    /// Every worker misses at once, as on a scan's first file of a language: one build, one value.
    #[test]
    fn concurrent_first_use_builds_once_and_shares_the_value() {
        const THREADS: usize = 16;
        let map: OnceMap<&'static str, Arc<String>> = OnceMap::default();
        let builds = AtomicUsize::new(0);
        let barrier = Barrier::new(THREADS);
        let values: Vec<Arc<String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        map.get_or_init("python", || {
                            builds.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(50));
                            Arc::new("engine".to_string())
                        })
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("worker")).collect()
        });
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(values.iter().all(|v| Arc::ptr_eq(v, &values[0])));
    }

    #[test]
    fn a_built_failure_is_cached_and_a_builder_error_is_retried() {
        let map: OnceMap<u8, Result<u8, String>> = OnceMap::default();
        let builds = AtomicUsize::new(0);
        let build = || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ()>(Err("does not compile".to_string()))
        };
        assert_eq!(map.get_or_try_init(1, build), Ok(Err("does not compile".to_string())));
        assert_eq!(map.get_or_try_init(1, build), Ok(Err("does not compile".to_string())));
        assert_eq!(builds.load(Ordering::SeqCst), 1, "the failure is built once");

        assert_eq!(
            map.get_or_try_init(2, || Err::<Result<u8, String>, _>("offline")),
            Err("offline")
        );
        assert_eq!(
            map.get_or_try_init(2, || Ok::<_, &str>(Ok(7))),
            Ok(Ok(7)),
            "a builder error is retried"
        );
    }
}
