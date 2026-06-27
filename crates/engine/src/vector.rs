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
    /// VH-2: the node's row version has been deleted/superseded as of the index's
    /// head (a committed delete). Unlike `removed`, a `dead` node is STILL returned
    /// by [`VectorIndex::search`] — an older MVCC snapshot may legitimately still
    /// see the row, so the executor re-checks visibility per result. The flag only
    /// drives maintenance statistics (over-fetch sizing + the compaction trigger).
    dead: bool,
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
    /// VH-2: count of `dead` (head-deleted) nodes still present in the graph —
    /// the maintenance statistic that drives over-fetch sizing and the compaction
    /// trigger ([`VectorIndex::needs_maintenance`]).
    dead_count: usize,
    /// VH-2: the committed LSN as of the last *compacting* rebuild (one that
    /// dropped head-deleted nodes from the graph). A snapshot strictly below this
    /// floor may still need a dropped row, so the executor falls back to a
    /// brute-force scan for it (see `exec::knn_select`). `0` = never compacted, so
    /// the index serves every snapshot.
    rebuild_floor: u64,
}

/// VH-2 maintenance policy (spec 12 §"delete churn"). A compacting rebuild is
/// triggered once head-deleted nodes make up at least `MAINT_DEAD_RATIO_PCT` of a
/// graph of at least `MAINT_MIN_NODES` nodes — small enough indexes just absorb
/// the dead nodes (over-fetch covers them) rather than paying repeated rebuilds.
pub const MAINT_MIN_NODES: usize = 64;
pub const MAINT_DEAD_RATIO_PCT: usize = 20;

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
            dead_count: 0,
            rebuild_floor: 0,
        }
    }

    /// Number of nodes present in the graph (live + head-deleted), i.e. every
    /// node `search` may still return. This is the over-fetch cap.
    pub fn len(&self) -> usize {
        self.by_vid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_vid.is_empty()
    }

    /// VH-2: nodes whose row version is live at the index head (present minus the
    /// head-deleted ones) — the denominator the executor uses to widen over-fetch
    /// so a query still surfaces `k` *live* results under heavy delete churn.
    pub fn live_len(&self) -> usize {
        self.by_vid.len().saturating_sub(self.dead_count)
    }

    /// VH-2: the compaction floor — the lowest snapshot LSN this index can answer
    /// directly (`0` = every snapshot; see [`VectorIndex::rebuild_floor`] field).
    pub fn rebuild_floor(&self) -> u64 {
        self.rebuild_floor
    }

    /// VH-2: whether head-deleted density has crossed the rebuild threshold on a
    /// graph large enough to be worth compacting.
    pub fn needs_maintenance(&self) -> bool {
        let n = self.by_vid.len();
        n >= MAINT_MIN_NODES && self.dead_count * 100 >= n * MAINT_DEAD_RATIO_PCT
    }

    /// VH-2: set the compaction floor (called after a compacting rebuild from the
    /// store, so the executor knows the lowest snapshot this graph can answer).
    pub fn set_rebuild_floor(&mut self, lsn: u64) {
        self.rebuild_floor = lsn;
    }

    /// VH-2: mark `vid`'s node head-deleted (a committed delete/supersede). The
    /// node stays navigable and still returns from `search` for older snapshots;
    /// only the maintenance statistic changes. Idempotent.
    pub fn mark_dead(&mut self, vid: u64) {
        if let Some(&idx) = self.by_vid.get(&vid) {
            if !self.nodes[idx].dead {
                self.nodes[idx].dead = true;
                self.dead_count += 1;
            }
        }
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
            dead: false,
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

    /// Top-`k` nearest vectors to `query`, nearest first. `k` doubles as the
    /// search width, so the executor over-fetches (to absorb MVCC-invisible and
    /// head-deleted hits) simply by asking for more. `ef_search` overrides the
    /// index's configured search width for this call (VH-3 — the per-query /
    /// per-session recall knob); `None` uses the index default. Head-deleted
    /// (`dead`) nodes are returned (the executor re-checks MVCC visibility); only
    /// rolled-back (`removed`) nodes are skipped.
    pub fn search(&self, query: &[f32], k: usize, ef_search: Option<usize>) -> Vec<(u64, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if k == 0 {
            return Vec::new();
        }
        let ef = ef_search.unwrap_or(self.def.params.ef_search).max(k);
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
            if self.nodes[idx].dead {
                self.dead_count = self.dead_count.saturating_sub(1);
            }
            self.nodes[idx].removed = true;
        }
    }
}

// ---- VH-1: page-laid-out graph (cold-read realization, spec 12) -------------
//
// The HNSW graph is a *derived* structure (rebuilt from the WAL on replay), so
// nothing here changes the storage seam: these are opaque, fixed-size page images
// that flow through the existing `put_page`/`get_page` contract exactly like a row
// page would (`STORAGE_TRAIT_VERSION` stays 3 — the trait never learns "index page
// vs row page"). Laying the graph out as pages lets a cold open *load* the graph
// in a bounded number of page reads instead of replaying every vector through the
// O(N·log N) insertion path; when no current checkpoint exists (e.g. a freshly
// diverged branch) the engine simply falls back to the rebuild-from-rows warm, so
// correctness never depends on a page image being present or fresh.

/// Page payload budget. Must not exceed the storage layer's `PAGE_SIZE` (4096);
/// `put_page` rejects an over-size image, and the engine caps writes to this.
pub const PAGE_CAP: usize = 4096;

const PAGE_MAGIC: &[u8; 8] = b"TWIVIDX1";

/// The decoded header of a graph page-checkpoint (page 0 of an index's region).
/// Lets the cold-open path validate a checkpoint (magic + index name + the
/// committed LSN it reflects) before reading — and trusting — its body pages.
#[derive(Clone, Debug)]
pub struct VectorPageHeader {
    pub metric: Metric,
    pub dim: usize,
    pub node_count: usize,
    pub entry: Option<usize>,
    pub max_level: usize,
    /// The store's `committed_lsn` this checkpoint reflects. The cold-open path
    /// adopts the checkpoint only when it exactly matches the replayed head.
    pub reflected_lsn: u64,
    pub dead_count: usize,
    pub rebuild_floor: u64,
    pub num_body_pages: usize,
    /// The index name, guarding against a page-id-region hash collision.
    pub name: String,
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}
fn get_u32(b: &[u8], at: &mut usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let v = u32::from_le_bytes(b.get(*at..end)?.try_into().ok()?);
    *at = end;
    Some(v)
}
fn get_u64(b: &[u8], at: &mut usize) -> Option<u64> {
    let end = at.checked_add(8)?;
    let v = u64::from_le_bytes(b.get(*at..end)?.try_into().ok()?);
    *at = end;
    Some(v)
}

/// Parse just the header page of a checkpoint (the first frame). Returns `None`
/// on a bad magic or a truncated/garbled image, so a stale or collided page is
/// rejected rather than misread.
pub fn parse_page_header(frame: &[u8]) -> Option<VectorPageHeader> {
    if frame.len() < 8 || &frame[..8] != PAGE_MAGIC {
        return None;
    }
    let mut at = 8usize;
    let metric = Metric::from_tag(*frame.get(at)?);
    at += 1;
    let dim = get_u32(frame, &mut at)? as usize;
    let node_count = get_u64(frame, &mut at)? as usize;
    let entry_raw = get_u64(frame, &mut at)?;
    let entry = (entry_raw != u64::MAX).then_some(entry_raw as usize);
    let max_level = get_u32(frame, &mut at)? as usize;
    let reflected_lsn = get_u64(frame, &mut at)?;
    let dead_count = get_u64(frame, &mut at)? as usize;
    let rebuild_floor = get_u64(frame, &mut at)?;
    let num_body_pages = get_u32(frame, &mut at)? as usize;
    let name_len = get_u32(frame, &mut at)? as usize;
    let end = at.checked_add(name_len)?;
    let name = String::from_utf8(frame.get(at..end)?.to_vec()).ok()?;
    Some(VectorPageHeader {
        metric,
        dim,
        node_count,
        entry,
        max_level,
        reflected_lsn,
        dead_count,
        rebuild_floor,
        num_body_pages,
        name,
    })
}

impl VectorIndex {
    /// Serialize the graph into page frames for VH-1: `frames[0]` is the header
    /// page, `frames[1..]` the body pages (each ≤ [`PAGE_CAP`]). Nodes are written
    /// in ordinal order so adjacency lists (stored as ordinals) survive a
    /// round-trip exactly. Returns `None` if the index is empty or a single node
    /// record cannot fit in a page (a pathologically large dimension × degree) —
    /// the caller then keeps the rebuild-from-rows path.
    pub fn to_page_frames(&self, reflected_lsn: u64) -> Option<Vec<Vec<u8>>> {
        if self.nodes.is_empty() {
            return None;
        }
        let dim = self.nodes[0].vector.len();
        // Pack node records greedily into body pages.
        let mut body: Vec<Vec<u8>> = Vec::new();
        let mut page: Vec<u8> = Vec::with_capacity(PAGE_CAP);
        let mut count: u32 = 0;
        let flush = |page: &mut Vec<u8>, count: &mut u32, body: &mut Vec<Vec<u8>>| {
            if *count == 0 {
                return;
            }
            let mut framed = Vec::with_capacity(4 + page.len());
            put_u32(&mut framed, *count);
            framed.extend_from_slice(page);
            body.push(framed);
            page.clear();
            *count = 0;
        };
        for node in &self.nodes {
            if node.vector.len() != dim {
                return None; // ragged dimensions — refuse to checkpoint
            }
            let mut rec = Vec::new();
            put_u64(&mut rec, node.vid);
            let flags = (node.removed as u8) | ((node.dead as u8) << 1);
            rec.push(flags);
            put_u32(&mut rec, node.neighbors.len() as u32);
            for &x in &node.vector {
                rec.extend_from_slice(&x.to_le_bytes());
            }
            for lvl in &node.neighbors {
                put_u32(&mut rec, lvl.len() as u32);
                for &nb in lvl {
                    put_u32(&mut rec, nb as u32);
                }
            }
            // A record must fit within a page (minus the 4-byte count prefix).
            if rec.len() + 4 > PAGE_CAP {
                return None;
            }
            if 4 + page.len() + rec.len() > PAGE_CAP {
                flush(&mut page, &mut count, &mut body);
            }
            page.extend_from_slice(&rec);
            count += 1;
        }
        flush(&mut page, &mut count, &mut body);

        let mut header = Vec::with_capacity(64 + self.def.name.len());
        header.extend_from_slice(PAGE_MAGIC);
        header.push(self.def.params.metric.tag());
        put_u32(&mut header, dim as u32);
        put_u64(&mut header, self.nodes.len() as u64);
        put_u64(
            &mut header,
            self.entry.map(|e| e as u64).unwrap_or(u64::MAX),
        );
        put_u32(&mut header, self.max_level as u32);
        put_u64(&mut header, reflected_lsn);
        put_u64(&mut header, self.dead_count as u64);
        put_u64(&mut header, self.rebuild_floor);
        put_u32(&mut header, body.len() as u32);
        put_u32(&mut header, self.def.name.len() as u32);
        header.extend_from_slice(self.def.name.as_bytes());
        if header.len() > PAGE_CAP {
            return None; // an absurdly long index name; skip the checkpoint
        }

        let mut frames = Vec::with_capacity(1 + body.len());
        frames.push(header);
        frames.extend(body);
        Some(frames)
    }

    /// Reconstruct a graph from page frames produced by [`Self::to_page_frames`]
    /// (`frames[0]` header, rest body). Returns `None` on any inconsistency, so a
    /// corrupt or partial checkpoint falls back to rebuild-from-rows.
    pub fn from_page_frames(def: IndexDef, frames: &[Vec<u8>]) -> Option<VectorIndex> {
        let header = parse_page_header(frames.first()?)?;
        if header.num_body_pages + 1 != frames.len() {
            return None;
        }
        // The checkpoint must agree with the definition we are loading it under.
        if header.metric != def.params.metric {
            return None;
        }
        let dim = header.dim;
        let mut nodes: Vec<Node> = Vec::with_capacity(header.node_count);
        for body in &frames[1..] {
            let mut at = 0usize;
            let count = get_u32(body, &mut at)?;
            for _ in 0..count {
                let vid = get_u64(body, &mut at)?;
                let flags = *body.get(at)?;
                at += 1;
                let nlevels = get_u32(body, &mut at)? as usize;
                let mut vector = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let end = at.checked_add(4)?;
                    vector.push(f32::from_le_bytes(body.get(at..end)?.try_into().ok()?));
                    at = end;
                }
                let mut neighbors = Vec::with_capacity(nlevels);
                for _ in 0..nlevels {
                    let deg = get_u32(body, &mut at)? as usize;
                    let mut lvl = Vec::with_capacity(deg);
                    for _ in 0..deg {
                        lvl.push(get_u32(body, &mut at)? as usize);
                    }
                    neighbors.push(lvl);
                }
                nodes.push(Node {
                    vid,
                    vector,
                    neighbors,
                    removed: (flags & 1) != 0,
                    dead: (flags & 2) != 0,
                });
            }
        }
        if nodes.len() != header.node_count {
            return None;
        }
        let mut by_vid = HashMap::with_capacity(nodes.len());
        let mut dead_count = 0usize;
        for (idx, node) in nodes.iter().enumerate() {
            if node.removed {
                continue;
            }
            by_vid.insert(node.vid, idx);
            if node.dead {
                dead_count += 1;
            }
        }
        // The recomputed head-deleted count must match the header (corruption guard).
        if dead_count != header.dead_count {
            return None;
        }
        Some(VectorIndex {
            def,
            nodes,
            by_vid,
            entry: header.entry,
            max_level: header.max_level,
            rng: 0x243F_6A88_85A3_08D3,
            dead_count,
            rebuild_floor: header.rebuild_floor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx_with(n: usize, dim: usize) -> VectorIndex {
        let def = IndexDef {
            name: "t_e".into(),
            table: "t".into(),
            column: "e".into(),
            params: IndexParams {
                metric: Metric::L2,
                ..IndexParams::default()
            },
        };
        let mut ix = VectorIndex::new(def);
        for i in 0..n {
            let f = i as f32;
            ix.insert(i as u64 + 1, vec![f, f * 0.5, dim as f32 - f]);
        }
        ix
    }

    #[test]
    fn page_round_trip_preserves_search() {
        let ix = idx_with(200, 3);
        let q = vec![17.0, 8.5, -14.0];
        let before = ix.search(&q, 5, None);
        let frames = ix.to_page_frames(42).expect("serializes");
        // Every frame respects the page budget.
        for f in &frames {
            assert!(f.len() <= PAGE_CAP, "frame {} > PAGE_CAP", f.len());
        }
        let hdr = parse_page_header(&frames[0]).unwrap();
        assert_eq!(hdr.reflected_lsn, 42);
        assert_eq!(hdr.name, "t_e");
        assert_eq!(hdr.node_count, 200);
        let restored = VectorIndex::from_page_frames(ix.def.clone(), &frames).expect("restores");
        let after = restored.search(&q, 5, None);
        assert_eq!(before, after, "page round-trip changes nothing");
        assert_eq!(restored.len(), ix.len());
    }

    #[test]
    fn ef_search_override_widens_search() {
        // A query-time ef override is honored independently of the index default.
        let ix = idx_with(300, 3);
        let q = vec![100.0, 50.0, -97.0];
        let narrow = ix.search(&q, 1, Some(1));
        let wide = ix.search(&q, 1, Some(256));
        assert_eq!(narrow.len(), 1);
        assert_eq!(wide.len(), 1);
    }

    #[test]
    fn empty_index_has_no_checkpoint() {
        let ix = idx_with(0, 3);
        assert!(ix.to_page_frames(1).is_none());
    }

    #[test]
    fn mark_dead_tracks_density_and_maintenance() {
        let mut ix = idx_with(100, 3);
        assert_eq!(ix.live_len(), 100);
        assert!(!ix.needs_maintenance());
        for vid in 1..=30u64 {
            ix.mark_dead(vid);
        }
        assert_eq!(ix.len() - ix.live_len(), 30);
        assert_eq!(ix.live_len(), 70);
        assert!(ix.needs_maintenance(), "30% dead crosses the threshold");
        // Idempotent.
        ix.mark_dead(1);
        assert_eq!(ix.live_len(), 70);
    }
}
