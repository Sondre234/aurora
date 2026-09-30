//! Byte-budgeted LRU cache with hit-rate metrics.
//!
//! Every cache in this crate follows the cache discipline of `docs/performance.md`:
//! it has an owner, a declared invalidation trigger, a byte budget enforced by LRU
//! eviction and a `tracing` metric (target `perf`). The metric line is emitted at most
//! once every 5 s and only while the cache is being used, so an idle process logs nothing.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::time::{Duration, Instant};

const REPORT_EVERY: Duration = Duration::from_secs(5);

/// Snapshot of a cache's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub budget: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    /// Inserts refused because one entry alone exceeds the budget.
    pub rejected: u64,
}

impl CacheStats {
    /// Hit rate in 0..=1 (0 when nothing was looked up yet).
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

struct Entry<V> {
    value: V,
    bytes: usize,
    tick: u64,
}

/// LRU map whose size is measured in bytes supplied by the caller at insert time.
pub struct LruCache<K, V> {
    name: &'static str,
    budget: usize,
    bytes: usize,
    tick: u64,
    map: HashMap<K, Entry<V>>,
    order: BTreeMap<u64, K>,
    stats: CacheStats,
    dirty: bool,
    last_report: Instant,
}

impl<K: Hash + Eq + Clone, V: Clone> LruCache<K, V> {
    pub fn new(name: &'static str, budget: usize) -> Self {
        Self {
            name,
            budget,
            bytes: 0,
            tick: 0,
            map: HashMap::new(),
            order: BTreeMap::new(),
            stats: CacheStats {
                budget,
                ..Default::default()
            },
            dirty: false,
            last_report: Instant::now(),
        }
    }

    /// Look up and mark most recently used. Counts a hit or a miss.
    pub fn get(&mut self, k: &K) -> Option<V> {
        self.tick += 1;
        let tick = self.tick;
        let out = match self.map.get_mut(k) {
            Some(e) => {
                self.order.remove(&e.tick);
                e.tick = tick;
                self.order.insert(tick, k.clone());
                self.stats.hits += 1;
                Some(e.value.clone())
            }
            None => {
                self.stats.misses += 1;
                None
            }
        };
        self.dirty = true;
        self.report(false);
        out
    }

    /// Insert (replacing any previous value) and evict least recently used entries until
    /// the budget holds. An entry larger than the whole budget is not stored.
    pub fn insert(&mut self, k: K, v: V, bytes: usize) {
        self.remove(&k);
        if bytes > self.budget {
            self.stats.rejected += 1;
            self.dirty = true;
            self.report(false);
            return;
        }
        self.tick += 1;
        self.order.insert(self.tick, k.clone());
        self.map.insert(
            k,
            Entry {
                value: v,
                bytes,
                tick: self.tick,
            },
        );
        self.bytes += bytes;
        self.evict();
        self.dirty = true;
        self.report(false);
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let e = self.map.remove(k)?;
        self.order.remove(&e.tick);
        self.bytes -= e.bytes;
        Some(e.value)
    }

    /// Drop every entry (invalidation). Counters other than size are kept.
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
        self.dirty = true;
        self.report(true);
    }

    /// Change the budget, evicting immediately if it shrank.
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.evict();
    }

    pub fn contains(&self, k: &K) -> bool {
        self.map.contains_key(k)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.map.len(),
            bytes: self.bytes,
            budget: self.budget,
            ..self.stats
        }
    }

    fn evict(&mut self) {
        while self.bytes > self.budget {
            let Some((_, k)) = self.order.pop_first() else {
                break;
            };
            if let Some(e) = self.map.remove(&k) {
                self.bytes -= e.bytes;
                self.stats.evictions += 1;
            }
        }
    }

    /// Emit the metric line if the cache saw activity and 5 s passed (or `force`).
    fn report(&mut self, force: bool) {
        if !self.dirty || (!force && self.last_report.elapsed() < REPORT_EVERY) {
            return;
        }
        self.dirty = false;
        self.last_report = Instant::now();
        let s = self.stats();
        tracing::info!(
            target: "perf",
            "ui cache {}: entries={} bytes={} budget={} hit_rate={:.3} hits={} misses={} evictions={}",
            self.name, s.entries, s.bytes, s.budget, s.hit_rate(), s.hits, s.misses, s.evictions
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_oldest_within_budget() {
        let mut c = LruCache::new("t", 100);
        c.insert(1, "a", 40);
        c.insert(2, "b", 40);
        assert_eq!(c.get(&1), Some("a")); // 1 is now newest
        c.insert(3, "c", 40); // over budget: evict 2
        assert!(c.contains(&1) && c.contains(&3) && !c.contains(&2));
        let s = c.stats();
        assert_eq!((s.bytes, s.entries, s.evictions), (80, 2, 1));
    }

    #[test]
    fn oversize_rejected_and_replace_adjusts_bytes() {
        let mut c = LruCache::new("t", 50);
        c.insert(1, 1, 60);
        assert_eq!(c.stats().rejected, 1);
        c.insert(1, 1, 10);
        c.insert(1, 2, 30);
        assert_eq!(c.stats().bytes, 30);
        assert_eq!(c.get(&1), Some(2));
    }

    #[test]
    fn hit_rate_and_shrink() {
        let mut c = LruCache::new("t", 100);
        c.insert(1, (), 50);
        c.get(&1);
        c.get(&2);
        assert!((c.stats().hit_rate() - 0.5).abs() < 1e-9);
        c.set_budget(10);
        assert!(c.is_empty());
        c.insert(1, (), 5);
        c.clear();
        assert_eq!(c.stats().bytes, 0);
    }
}
