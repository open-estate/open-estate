//! Deduplicated candidate store: priority scheduling and negative caching of dead proxies.
//!
//! Public lists overlap heavily and most entries are dead, so re-testing everything each round
//! wastes nearly all of the work. Each candidate keeps:
//! * which sources listed it in the current *generation* (one generation per source sweep) and a
//!   priority `score` = Σ yield of those sources (a proxy listed by several productive lists is far
//!   more likely to be alive than one listed by a single noisy list);
//! * its check history: dead candidates are re-checked only after an exponential backoff, alive
//!   ones get a priority boost when they come back after an eviction.

use super::extract::ExtractedProxy;
use super::{ProxyEndpoint, ProxyProtocol};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

pub const MAX_SOURCE_REFS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandKey {
    pub protocol: ProxyProtocol,
    pub ip: IpAddr,
    pub port: u16,
}

impl CandKey {
    pub fn from_endpoint(ep: &ProxyEndpoint) -> Option<Self> {
        Some(Self {
            protocol: ep.protocol,
            ip: ep.ip()?,
            port: ep.port,
        })
    }

    pub fn from_extracted(ex: &ExtractedProxy) -> Self {
        Self {
            protocol: ex.protocol,
            ip: ex.ip,
            port: ex.port,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CandEntry {
    pub auth: Option<Box<(String, String)>>,
    pub country: Option<[u8; 2]>,
    srcs: [u16; MAX_SOURCE_REFS],
    nsrc: u8,
    gen: u32,
    pub listed: u16,
    pub score: f32,
    pub checks: u32,
    pub alive_hits: u16,
    pub dead_streak: u8,
    pub next_check_at: i64,
    pub last_checked_at: i64,
    pub last_listed_at: i64,
}

impl CandEntry {
    fn new() -> Self {
        Self {
            auth: None,
            country: None,
            srcs: [0; MAX_SOURCE_REFS],
            nsrc: 0,
            gen: 0,
            listed: 0,
            score: 0.0,
            checks: 0,
            alive_hits: 0,
            dead_streak: 0,
            next_check_at: 0,
            last_checked_at: 0,
            last_listed_at: 0,
        }
    }

    pub fn sources(&self) -> &[u16] {
        &self.srcs[..self.nsrc as usize]
    }

    /// Scheduling priority (higher = check sooner).
    pub fn priority(&self) -> f32 {
        let revived = 1.0 + f32::from(self.alive_hits.min(3));
        let dead_penalty = 0.6_f32.powi(i32::from(self.dead_streak.min(10)));
        self.score * revived * dead_penalty
    }

    pub fn country_str(&self) -> Option<String> {
        self.country.map(|c| String::from_utf8_lossy(&c).into_owned())
    }
}

pub struct CandidateStore {
    map: HashMap<CandKey, CandEntry>,
    gen: u32,
    max_entries: usize,
}

/// xorshift64* — cheap, deterministic for a given seed (good enough for sampling).
pub(crate) fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

impl CandidateStore {
    pub fn new(max_entries: usize) -> Self {
        Self {
            map: HashMap::new(),
            // Start at 1 so newly seeded metadata (`CandEntry.gen == 0`) is never treated as
            // listed before `begin_generation` + `mark_listed` runs.
            gen: 1,
            max_entries: max_entries.max(1_000),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn generation(&self) -> u32 {
        self.gen
    }

    pub fn get(&self, key: &CandKey) -> Option<&CandEntry> {
        self.map.get(key)
    }

    /// Starts a new listing generation (call once per source sweep, before `mark_listed`).
    pub fn begin_generation(&mut self) -> u32 {
        self.gen = self.gen.wrapping_add(1).max(1);
        self.gen
    }

    /// Registers optional credentials / country metadata on `key` without changing listing counts.
    pub fn seed_meta(
        &mut self,
        key: CandKey,
        auth: Option<&(String, String)>,
        country: Option<[u8; 2]>,
        now: i64,
    ) {
        if !self.map.contains_key(&key) && self.map.len() >= self.max_entries {
            return;
        }
        let e = self.map.entry(key).or_insert_with(CandEntry::new);
        e.last_listed_at = now;
        if e.country.is_none() {
            e.country = country;
        }
        if e.auth.is_none() {
            if let Some(a) = auth {
                e.auth = Some(Box::new(a.clone()));
            }
        }
    }

    /// Records that `source_id` (current yield estimate `weight`) lists `key` in this generation.
    pub fn mark_listed(
        &mut self,
        key: CandKey,
        auth: Option<&(String, String)>,
        country: Option<[u8; 2]>,
        source_id: u16,
        weight: f32,
        now: i64,
    ) {
        if !self.map.contains_key(&key) && self.map.len() >= self.max_entries {
            return;
        }
        let gen = self.gen;
        let e = self.map.entry(key).or_insert_with(CandEntry::new);
        if e.gen != gen {
            e.gen = gen;
            e.listed = 0;
            e.score = 0.0;
            e.nsrc = 0;
        }
        if e.sources().contains(&source_id) {
            return;
        }
        e.listed = e.listed.saturating_add(1);
        e.score += weight.max(0.0);
        if (e.nsrc as usize) < MAX_SOURCE_REFS {
            e.srcs[e.nsrc as usize] = source_id;
            e.nsrc += 1;
        }
        e.last_listed_at = now;
        if e.country.is_none() {
            e.country = country;
        }
        if e.auth.is_none() {
            if let Some(a) = auth {
                e.auth = Some(Box::new(a.clone()));
            }
        }
    }

    /// Picks up to `budget` candidates that are listed in the current generation, not in
    /// backoff and not already working: `(1 - explore_fraction)` by priority, the rest sampled
    /// uniformly from never-checked candidates and interleaved so exploration happens even when
    /// a round stops early.
    pub fn select_for_check(
        &self,
        budget: usize,
        now: i64,
        exclude: &HashSet<CandKey>,
        explore_fraction: f32,
        seed: u64,
    ) -> Vec<CandKey> {
        let mut eligible: Vec<(f32, CandKey, bool)> = self
            .map
            .iter()
            .filter(|(k, e)| e.gen == self.gen && e.next_check_at <= now && !exclude.contains(k))
            .map(|(k, e)| (e.priority(), *k, e.checks == 0))
            .collect();
        let n = eligible.len();
        if n == 0 || budget == 0 {
            return Vec::new();
        }
        let budget = budget.min(n);
        let n_explore = ((budget as f32) * explore_fraction.clamp(0.0, 1.0)).round() as usize;
        let n_explore = n_explore.min(budget);
        let n_exploit = budget - n_explore;

        let by_priority_desc = |a: &(f32, CandKey, bool), b: &(f32, CandKey, bool)| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        };
        if n_exploit > 0 && n_exploit < n {
            eligible.select_nth_unstable_by(n_exploit - 1, by_priority_desc);
        }
        let (top, rest) = eligible.split_at_mut(n_exploit.min(n));
        top.sort_unstable_by(by_priority_desc);

        let mut rng = seed | 1;
        let mut explore: Vec<CandKey> = Vec::with_capacity(n_explore);
        if n_explore > 0 && !rest.is_empty() {
            let mut idx: Vec<usize> = (0..rest.len()).filter(|&i| rest[i].2).collect();
            if idx.len() < n_explore {
                idx = (0..rest.len()).collect();
            }
            let take = n_explore.min(idx.len());
            for i in 0..take {
                let j = i + (xorshift(&mut rng) as usize) % (idx.len() - i);
                idx.swap(i, j);
                explore.push(rest[idx[i]].1);
            }
        }

        let mut out = Vec::with_capacity(top.len() + explore.len());
        let stride = if explore.is_empty() {
            usize::MAX
        } else {
            (top.len() / explore.len()).max(1)
        };
        let mut ex_iter = explore.into_iter();
        for (i, (_, k, _)) in top.iter().enumerate() {
            out.push(*k);
            if (i + 1) % stride == 0 {
                if let Some(e) = ex_iter.next() {
                    out.push(e);
                }
            }
        }
        out.extend(ex_iter);
        out
    }

    /// Records a check result and returns the listing sources (for yield attribution).
    pub fn record_check(
        &mut self,
        key: &CandKey,
        alive: bool,
        now: i64,
        backoff_base_secs: i64,
        backoff_max_secs: i64,
    ) -> Vec<u16> {
        let Some(e) = self.map.get_mut(key) else {
            return Vec::new();
        };
        e.checks = e.checks.saturating_add(1);
        e.last_checked_at = now;
        if alive {
            e.dead_streak = 0;
            e.alive_hits = e.alive_hits.saturating_add(1);
            e.next_check_at = now + backoff_base_secs.max(1);
        } else {
            e.dead_streak = e.dead_streak.saturating_add(1).min(30);
            let shift = u32::from(e.dead_streak.saturating_sub(1)).min(16);
            let backoff = backoff_base_secs
                .max(1)
                .saturating_mul(1_i64 << shift)
                .min(backoff_max_secs.max(backoff_base_secs));
            e.next_check_at = now + backoff;
        }
        e.sources().to_vec()
    }

    /// Keeps an evicted working proxy out of rotation for `secs`.
    pub fn quarantine(&mut self, key: &CandKey, now: i64, secs: i64) {
        if let Some(e) = self.map.get_mut(key) {
            e.dead_streak = e.dead_streak.saturating_add(1).min(30);
            e.next_check_at = e.next_check_at.max(now + secs);
        }
    }

    /// Drops candidates no source has listed for `stale_after_secs`, then the oldest entries if
    /// the store is still over capacity. `protect` (working proxies) is never removed.
    pub fn prune(&mut self, now: i64, stale_after_secs: i64, protect: &HashSet<CandKey>) -> usize {
        let before = self.map.len();
        let cutoff = now - stale_after_secs.max(60);
        self.map
            .retain(|k, e| e.last_listed_at >= cutoff || protect.contains(k));
        if self.map.len() > self.max_entries {
            let mut by_age: Vec<(i64, CandKey)> = self
                .map
                .iter()
                .filter(|(k, _)| !protect.contains(k))
                .map(|(k, e)| (e.last_listed_at, *k))
                .collect();
            by_age.sort_unstable();
            let excess = self.map.len() - self.max_entries;
            for (_, k) in by_age.into_iter().take(excess) {
                self.map.remove(&k);
            }
        }
        before - self.map.len()
    }

    /// Candidates currently listed and waiting out a dead backoff.
    pub fn backoff_count(&self, now: i64) -> usize {
        self.map
            .values()
            .filter(|e| e.gen == self.gen && e.next_check_at > now && e.checks > 0)
            .count()
    }

    pub fn listed_count(&self) -> usize {
        self.map.values().filter(|e| e.gen == self.gen).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn key(last: u8, port: u16) -> CandKey {
        CandKey {
            protocol: ProxyProtocol::Http,
            ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)),
            port,
        }
    }

    #[test]
    fn multi_source_candidates_are_checked_first_and_dead_ones_back_off() {
        let mut store = CandidateStore::new(10_000);
        store.begin_generation();
        // A: listed by 3 sources, B: by 1 productive source, C: by 1 noisy source.
        for src in 0..3 {
            store.mark_listed(key(1, 80), None, None, src, 0.05, 100);
        }
        store.mark_listed(key(2, 80), None, Some(*b"US"), 7, 0.10, 100);
        store.mark_listed(key(3, 80), None, None, 8, 0.01, 100);
        // Re-listing by the same source in the same generation is not double counted.
        store.mark_listed(key(3, 80), None, None, 8, 0.01, 100);
        assert_eq!(store.get(&key(3, 80)).unwrap().listed, 1);
        assert_eq!(store.get(&key(2, 80)).unwrap().country, Some(*b"US"));

        let order = store.select_for_check(3, 100, &HashSet::new(), 0.0, 42);
        assert_eq!(order, vec![key(1, 80), key(2, 80), key(3, 80)]);

        // Working proxies are excluded; budget is respected.
        let excl: HashSet<CandKey> = [key(1, 80)].into_iter().collect();
        assert_eq!(store.select_for_check(1, 100, &excl, 0.0, 42), vec![key(2, 80)]);

        // Dead → exponential backoff: 600 s, then 1200 s, capped.
        let srcs = store.record_check(&key(1, 80), false, 100, 600, 3_600);
        assert_eq!(srcs, vec![0, 1, 2]);
        assert_eq!(store.get(&key(1, 80)).unwrap().next_check_at, 700);
        assert!(!store
            .select_for_check(10, 699, &HashSet::new(), 0.0, 1)
            .contains(&key(1, 80)));
        assert!(store
            .select_for_check(10, 700, &HashSet::new(), 0.0, 1)
            .contains(&key(1, 80)));
        store.record_check(&key(1, 80), false, 700, 600, 3_600);
        assert_eq!(store.get(&key(1, 80)).unwrap().next_check_at, 1_900);
        for t in [1_900, 5_000, 9_000] {
            store.record_check(&key(1, 80), false, t, 600, 3_600);
        }
        assert_eq!(store.get(&key(1, 80)).unwrap().next_check_at, 9_000 + 3_600);
        assert_eq!(store.backoff_count(9_001), 1);
    }

    #[test]
    fn exploration_reserves_budget_for_never_checked_candidates() {
        let mut store = CandidateStore::new(10_000);
        store.begin_generation();
        for i in 0..100u8 {
            // Heavier score for low i.
            store.mark_listed(key(i, 1000), None, None, 0, 1.0 / (f32::from(i) + 1.0), 10);
        }
        let picks = store.select_for_check(20, 10, &HashSet::new(), 0.25, 7);
        assert_eq!(picks.len(), 20);
        let top15: HashSet<CandKey> = (0..15u8).map(|i| key(i, 1000)).collect();
        let exploit = picks.iter().filter(|k| top15.contains(k)).count();
        assert_eq!(exploit, 15, "75% of the budget goes to the highest priorities");
        assert_eq!(picks.iter().collect::<HashSet<_>>().len(), 20, "no duplicates");
    }

    #[test]
    fn stale_candidates_are_pruned_and_generations_reset_listing() {
        let mut store = CandidateStore::new(1_000);
        store.begin_generation();
        store.mark_listed(key(1, 80), None, None, 0, 0.1, 1_000);
        store.mark_listed(key(2, 80), None, None, 0, 0.1, 1_000);
        store.begin_generation();
        store.mark_listed(key(2, 80), None, None, 1, 0.1, 50_000);
        // Not listed in the current generation → not selectable.
        assert_eq!(
            store.select_for_check(10, 50_000, &HashSet::new(), 0.0, 3),
            vec![key(2, 80)]
        );
        let protect: HashSet<CandKey> = HashSet::new();
        assert_eq!(store.prune(100_000, 86_400, &protect), 1);
        assert!(store.get(&key(1, 80)).is_none());
        assert!(store.get(&key(2, 80)).is_some());
    }
}
