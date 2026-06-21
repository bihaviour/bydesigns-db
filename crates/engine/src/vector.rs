//! Phase 5 — in-core vector search: the `vector(N)` type's index side (spec 12).
//!
//! Vector search is a STORAGE/EXECUTION capability (a column type + an access
//! method the planner can choose), so by the Capabilities deciding rule it lives
//! INSIDE the engine — not composed around it. Crucially it rides the *same*
//! durability path the rows do: the HNSW graph is a **derived** structure over a
//! table's vector column, rebuilt by replaying the engine WAL on open, exactly
//! like the MVCC row store (`store.rs`) is. Nothing is written to a side file.
//!
//! That is the whole payoff (spec 12 §"branching payoff"): because the index is
//! derived from the same WAL a branch's overlay captures, **branching the
//! database branches the vector index with it** — open a branch, replay its log,
//! and you get the branch's index, isolated from the base. Scale-to-zero and
//! S3-backing fall out identically: the warm is the replay, the durable floor is
//! the WAL on whichever backend the connection string selected.
//!
//! The graph itself follows the `hnswlib`/`usearch` lineage (multi-layer
//! navigable small world) adapted to the engine's value types, kept compact and
//! dependency-free (a deterministic SplitMix64 level generator, no `rand` crate).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

/// Distance metric an HNSW index is built and queried under. Each maps to one
/// SQL distance operator (`<->` L2, `<=>` cosine, `<#>` inner product), so the
/// planner can only push a query into the index when the operator matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    Cosine,
    L2,
    InnerProduct,
}

impl Metric {
    /// Parse a `metric = '...'` index option (case-insensitive; a few aliases).
    pub fn from_name(s: &str) -> Option<Metric> {
        match s.to_ascii_lowercase().as_str() {
            "cosine" | "cos" => Some(Metric::Cosine),
            "l2" | "euclidean" | "l2sq" => Some(Metric::L2),
            "ip" | "inner_product" | "dot" | "negative_inner_product" => Some(Metric::InnerProduct),
            _ => None,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            Metric::Cosine => "cosine",
            Metric::L2 => "l2",
            Metric::InnerProduct => "inner_product",
        }
    }

    pub fn tag(self) -> u8 {
        match self {
            Metric::Cosine => 0,
            Metric::L2 => 1,
            Metric::InnerProduct => 2,
        }
    }

    pub fn from_tag(t: u8) -> Metric {
        match t {
            1 => Metric::L2,
            2 => Metric::InnerProduct,
            _ => Metric::Cosine,
        }
    }
}

/// Distance under `metric`. Smaller is always "nearer", so an ascending
/// `ORDER BY ... LIMIT k` is a top-k nearest-neighbour query for every metric
/// (inner product is negated, matching pgvector's `<#>`).
pub fn distance(metric: Metric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        Metric::L2 => l2(a, b),
        Metric::Cosine => cosine_distance(a, b),
        Metric::InnerProduct => -dot(a, b),
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn l2(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let na = dot(a, a).sqrt();
    let nb = dot(b, b).sqrt();
    if na == 0.0 || nb == 0.0 {
        return 1.0;
    }
    1.0 - dot(a, b) / (na * nb)
}

/// Tuning knobs for an HNSW index (spec 12 §Configuration knobs). Defaults are
/// the usual sensible starting point; `ef_search` is the primary online knob.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IndexParams {
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
    pub metric: Metric,
}

impl Default for IndexParams {
    fn default() -> IndexParams {
        IndexParams {
            m: 16,
            ef_construction: 200,
            ef_search: 64,
            metric: Metric::Cosine,
        }
    }
}

/// The durable definition of an index (what the WAL carries). The graph is not
/// stored — it is rebuilt from the column's vectors on replay.
#[derive(Clone, Debug)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub column: String,
    pub params: IndexParams,
}

struct Node {
    vid: u64,
    vector: Vec<f32>,
    /// `neighbors[level]` is the adjacency list at that layer.
    neighbors: Vec<Vec<usize>>,
    /// Tombstone: a node whose row version was rolled back. Kept for navigation
    /// (it is still a valid point in space) but never returned as a result.
    removed: bool,
}

/// An HNSW vector index over one table column. Holds the graph plus a `vid ->
/// node` map; MVCC visibility is resolved by the executor against the row
/// versions the returned vids identify, so the index itself stays MVCC-agnostic.
pub struct VectorIndex {
    pub def: IndexDef,
    nodes: Vec<Node>,
    by_vid: HashMap<u64, usize>,
    entry: Option<usize>,
    max_level: usize,
    rng: u64,
}

/// A `(distance, node)` pair ordered by distance (ties broken by node index for
/// a total order, so it is safe in a `BinaryHeap`).
#[derive(Clone, Copy)]
struct Scored {
    dist: f32,
    idx: usize,
}

impl PartialEq for Scored {
    fn eq(&self, o: &Self) -> bool {
        self.dist == o.dist && self.idx == o.idx
    }
}
impl Eq for Scored {}
impl Ord for Scored {
    fn cmp(&self, o: &Self) -> Ordering {
        self.dist.total_cmp(&o.dist).then(self.idx.cmp(&o.idx))
    }
}
impl PartialOrd for Scored {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl VectorIndex {
    pub fn new(def: IndexDef) -> VectorIndex {
        VectorIndex {
            def,
            nodes: Vec::new(),
            by_vid: HashMap::new(),
            entry: None,
            max_level: 0,
            rng: 0x243F_6A88_85A3_08D3, // fixed seed → deterministic builds
        }
    }

    /// Number of live (non-tombstoned) vectors.
    pub fn len(&self) -> usize {
        self.by_vid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_vid.is_empty()
    }

    pub fn metric(&self) -> Metric {
        self.def.params.metric
    }

    fn metric_of(&self) -> Metric {
        self.def.params.metric
    }

    fn dist(&self, q: &[f32], idx: usize) -> f32 {
        distance(self.metric_of(), q, &self.nodes[idx].vector)
    }

    fn neighbors(&self, idx: usize, level: usize) -> &[usize] {
        self.nodes[idx]
            .neighbors
            .get(level)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn next_u64(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Geometric layer assignment with mean `1/ln(m)`, capped to keep the tower
    /// shallow for small indexes.
    fn random_level(&mut self) -> usize {
        let u = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        let u = u.max(1e-12);
        let ml = 1.0 / (self.def.params.m.max(2) as f64).ln();
        ((-u.ln() * ml).floor() as usize).min(16)
    }

    /// Add `(vid, vector)`. Idempotent on a vid already present.
    pub fn insert(&mut self, vid: u64, vector: Vec<f32>) {
        if self.by_vid.contains_key(&vid) {
            return;
        }
        let level = self.random_level();
        let idx = self.nodes.len();
        self.nodes.push(Node {
            vid,
            vector,
            neighbors: vec![Vec::new(); level + 1],
            removed: false,
        });
        self.by_vid.insert(vid, idx);

        let Some(entry) = self.entry else {
            self.entry = Some(idx);
            self.max_level = level;
            return;
        };
        self.link_new_node(idx, entry, level);
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(idx);
        }
    }

    /// Greedy-descend from the entry point, then connect the new node into every
    /// layer at or below its assigned level (classic HNSW insertion).
    fn link_new_node(&mut self, idx: usize, entry: usize, level: usize) {
        let q = self.nodes[idx].vector.clone();
        let mut ep = entry;
        let mut l = self.max_level;
        while l > level {
            if let Some(best) = self.search_layer(&q, ep, 1, l).first() {
                ep = best.idx;
            }
            l -= 1;
        }
        let ef = self.def.params.ef_construction;
        let top = level.min(self.max_level);
        for lvl in (0..=top).rev() {
            let found = self.search_layer(&q, ep, ef, lvl);
            let m = if lvl == 0 {
                self.def.params.m * 2
            } else {
                self.def.params.m
            };
            for s in found.iter().take(m) {
                self.connect(idx, s.idx, lvl);
                self.connect(s.idx, idx, lvl);
                self.prune(s.idx, lvl, m);
            }
            if let Some(best) = found.first() {
                ep = best.idx;
            }
        }
    }

    fn connect(&mut self, a: usize, b: usize, lvl: usize) {
        if a == b {
            return;
        }
        let nbrs = &mut self.nodes[a].neighbors;
        while nbrs.len() <= lvl {
            nbrs.push(Vec::new());
        }
        if !nbrs[lvl].contains(&b) {
            nbrs[lvl].push(b);
        }
    }

    /// Cap node `a`'s degree at level `lvl` to `m`, keeping its nearest links.
    fn prune(&mut self, a: usize, lvl: usize, m: usize) {
        if self.nodes[a].neighbors.get(lvl).map_or(0, Vec::len) <= m {
            return;
        }
        let av = self.nodes[a].vector.clone();
        let metric = self.metric_of();
        let mut scored: Vec<Scored> = self.nodes[a].neighbors[lvl]
            .iter()
            .map(|&nb| Scored {
                dist: distance(metric, &av, &self.nodes[nb].vector),
                idx: nb,
            })
            .collect();
        scored.sort_unstable();
        scored.truncate(m);
        self.nodes[a].neighbors[lvl] = scored.into_iter().map(|s| s.idx).collect();
    }

    /// Best-first search of one layer, returning up to `ef` candidates sorted
    /// nearest-first (the standard HNSW `SEARCH-LAYER`).
    fn search_layer(&self, q: &[f32], entry: usize, ef: usize, level: usize) -> Vec<Scored> {
        let mut visited = HashSet::new();
        visited.insert(entry);
        let start = Scored {
            dist: self.dist(q, entry),
            idx: entry,
        };
        let mut candidates = BinaryHeap::new(); // min-heap via Reverse
        candidates.push(std::cmp::Reverse(start));
        let mut result = BinaryHeap::new(); // max-heap: farthest kept on top
        result.push(start);

        while let Some(std::cmp::Reverse(c)) = candidates.pop() {
            let farthest = result.peek().map_or(f32::INFINITY, |s| s.dist);
            if c.dist > farthest && result.len() >= ef {
                break;
            }
            for i in 0..self.neighbors(c.idx, level).len() {
                let nb = self.neighbors(c.idx, level)[i];
                if !visited.insert(nb) {
                    continue;
                }
                let d = self.dist(q, nb);
                let farthest = result.peek().map_or(f32::INFINITY, |s| s.dist);
                if d < farthest || result.len() < ef {
                    let scored = Scored { dist: d, idx: nb };
                    candidates.push(std::cmp::Reverse(scored));
                    result.push(scored);
                    if result.len() > ef {
                        result.pop();
                    }
                }
            }
        }
        let mut out = result.into_vec();
        out.sort_unstable();
        out
    }

    /// Top-`k` nearest live vectors to `query`, nearest first. `k` doubles as the
    /// search width, so the executor over-fetches (to absorb MVCC-invisible and
    /// tombstoned hits) simply by asking for more.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 {
            return Vec::new();
        }
        let ef = self.def.params.ef_search.max(k);
        let mut ep = entry;
        let mut l = self.max_level;
        while l > 0 {
            if let Some(best) = self.search_layer(query, ep, 1, l).first() {
                ep = best.idx;
            }
            l -= 1;
        }
        let found = self.search_layer(query, ep, ef, 0);
        let mut out = Vec::with_capacity(k.min(found.len()));
        for s in found {
            let node = &self.nodes[s.idx];
            if node.removed {
                continue;
            }
            out.push((node.vid, s.dist));
            if out.len() >= k {
                break;
            }
        }
        out
    }

    /// Tombstone a vid (a rolled-back pending insert). The node stays in the
    /// graph for navigation but is never returned again.
    pub fn remove(&mut self, vid: u64) {
        if let Some(idx) = self.by_vid.remove(&vid) {
            self.nodes[idx].removed = true;
        }
    }
}
