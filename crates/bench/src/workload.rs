//! Request-mix scenarios (spec 15 "Benchmark scenarios"): named workload shapes
//! that drive a ratio-controlled mix of `SELECT`/`INSERT`/`UPDATE`/`DELETE`
//! against a pre-seeded working set, over either transport. Unlike the spec-09
//! experiments (which isolate one lever each), a scenario approximates a real
//! application's request distribution, so the reported percentiles characterize
//! the engine under a representative load rather than a single operation.
//!
//! The op kind for each request is drawn from the scenario's [`Mix`] weights by a
//! tiny, dependency-free per-writer PRNG ([`Rng`], xorshift64) — deterministic
//! given the run nonce and writer index, so a run is reproducible. Conflicts are
//! retried exactly as the experiments retry them (so a contended `UPDATE` never
//! loses), and every operation's end-to-end latency (including retries) lands in
//! the shared HDR histogram.

use crate::hist::Histogram;
use crate::{
    resolve_target, run_nonce, url_scheme, BenchError, Opts, Outcome, Report, Tally, Target, Writer,
};
use std::time::{Duration, Instant};

const TABLE_MIX: &str = "bench_mix";

/// A named request-mix workload shape.
#[derive(Clone, Copy)]
pub enum Scenario {
    /// 90% read / 10% insert — analytical-leaning.
    ReadHeavy,
    /// 20% read / 80% insert — ingestion.
    WriteHeavy,
    /// 70% read / 20% insert / 8% update / 2% delete — SaaS OLTP.
    MixedOltp,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::ReadHeavy => "read-heavy",
            Scenario::WriteHeavy => "write-heavy",
            Scenario::MixedOltp => "mixed-oltp",
        }
    }

    fn mix(self) -> Mix {
        match self {
            Scenario::ReadHeavy => Mix::new(90, 10, 0, 0),
            Scenario::WriteHeavy => Mix::new(20, 80, 0, 0),
            Scenario::MixedOltp => Mix::new(70, 20, 8, 2),
        }
    }
}

/// The four operation kinds a mix scenario issues.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Select,
    Insert,
    Update,
    Delete,
}

/// Integer request-ratio weights over the four op kinds. Drawing an op walks the
/// cumulative weight, so the long-run mix matches the configured ratios.
#[derive(Clone, Copy)]
pub struct Mix {
    select: u64,
    insert: u64,
    update: u64,
    delete: u64,
}

impl Mix {
    pub fn new(select: u64, insert: u64, update: u64, delete: u64) -> Mix {
        Mix {
            select,
            insert,
            update,
            delete,
        }
    }

    fn total(&self) -> u64 {
        (self.select + self.insert + self.update + self.delete).max(1)
    }

    /// Pick an op kind for `r` in `[0, total())`.
    pub fn pick(&self, r: u64) -> Op {
        let r = r % self.total();
        if r < self.select {
            Op::Select
        } else if r < self.select + self.insert {
            Op::Insert
        } else if r < self.select + self.insert + self.update {
            Op::Update
        } else {
            Op::Delete
        }
    }
}

/// A minimal xorshift64 PRNG — no external dependency, deterministic per seed, so
/// a scenario run is reproducible from its nonce + writer index.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // Avoid the zero fixed point; mix the seed so adjacent writers diverge.
        Rng((seed ^ 0x9E37_79B9_7F4A_7C15).max(1))
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `[0, n)` (n >= 1).
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// Run a request-mix scenario: seed the working set, then drive the mix from all
/// writers over a timed window and report the latency distribution.
pub(crate) fn run_scenario(scenario: Scenario, opts: &Opts) -> Result<Report, BenchError> {
    let mix = scenario.mix();
    let target = resolve_target(opts)?;
    let nonce = run_nonce();

    // Setup: schema + a deterministic working set of `--rows` keys [0, rows) the
    // read/update/delete ops target. Inserts use keys >= rows (nonce-disjoint).
    let mut setup = target.open()?;
    seed_working_set(&mut setup, opts.rows)?;

    let (tallies, elapsed) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..opts.writers)
            .map(|w| {
                let target = target.clone();
                let warmup = opts.warmup;
                let duration = opts.duration;
                let rows = opts.rows;
                scope.spawn(move || mix_writer(&target, w, nonce, mix, rows, warmup, duration))
            })
            .collect();
        let start = Instant::now();
        let tallies: Vec<Result<Tally, BenchError>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        (tallies, start.elapsed())
    });

    let mut merged = Histogram::new();
    let mut conflicts = 0u64;
    for t in tallies {
        let t = t?;
        merged.merge(&t.hist);
        conflicts += t.conflicts;
    }
    let ops = merged.count();

    Ok(Report {
        experiment: scenario.name(),
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers: opts.writers,
        duration_s: elapsed.as_secs_f64(),
        commits: ops,
        conflicts,
        failures: 0,
        throughput: ops as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        hist: merged,
        git_sha: crate::git_sha(),
        json_only: opts.json,
        correctness: None,
    })
}

fn seed_working_set(w: &mut Writer, rows: u64) -> Result<(), BenchError> {
    match w.exec(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE_MIX} (k INTEGER PRIMARY KEY, v INTEGER)"
    )) {
        Outcome::Ok | Outcome::Conflict => {}
        Outcome::Fatal(m) => return Err(BenchError::Run(format!("create mix table: {m}"))),
    }
    // Seed [0, rows). Idempotent across reruns: a duplicate-key insert is ignored.
    for k in 0..rows {
        match w.exec(&format!("INSERT INTO {TABLE_MIX} (k, v) VALUES ({k}, 0)")) {
            Outcome::Ok | Outcome::Conflict => {}
            Outcome::Fatal(_) => break, // already seeded (PK clash) — stop early
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn mix_writer(
    target: &Target,
    writer: usize,
    nonce: u128,
    mix: Mix,
    rows: u64,
    warmup: Duration,
    duration: Duration,
) -> Result<Tally, BenchError> {
    let mut conn = target.open()?;
    let mut rng = Rng::new(nonce as u64 ^ ((writer as u64).wrapping_mul(0x100_0000_01b3)));
    // Inserts carve a per-writer key band above the seeded working set so they
    // never collide across writers or with the seed.
    let mut insert_seq: u64 = 0;
    let insert_base = rows + (writer as u64) * 1_000_000_000 + 1;

    let one = |conn: &mut Writer,
               rng: &mut Rng,
               insert_seq: &mut u64,
               hist: Option<&mut Histogram>,
               conflicts: &mut u64|
     -> Result<(), BenchError> {
        let op = mix.pick(rng.next_u64());
        let t0 = Instant::now();
        match op {
            Op::Select => {
                // A point read over the working set; counts as one completed op.
                // A miss (the key may have been deleted) is still a valid read.
                let k = rng.below(rows);
                conn.read(&format!("SELECT v FROM {TABLE_MIX} WHERE k = {k}"))
                    .map_err(|m| BenchError::Run(format!("writer {writer} read failed: {m}")))?;
            }
            _ => {
                // The mutating ops retry on a first-committer/first-toucher
                // conflict, exactly as a real client would.
                loop {
                    let sql = match op {
                        Op::Insert => {
                            let k = insert_base + *insert_seq;
                            *insert_seq += 1;
                            format!("INSERT INTO {TABLE_MIX} (k, v) VALUES ({k}, 1)")
                        }
                        Op::Update => {
                            let k = rng.below(rows);
                            format!("UPDATE {TABLE_MIX} SET v = v + 1 WHERE k = {k}")
                        }
                        Op::Delete => {
                            let k = rng.below(rows);
                            format!("DELETE FROM {TABLE_MIX} WHERE k = {k}")
                        }
                        Op::Select => unreachable!(),
                    };
                    match conn.exec(&sql) {
                        Outcome::Ok => break,
                        Outcome::Conflict => {
                            *conflicts += 1;
                            continue;
                        }
                        Outcome::Fatal(m) => {
                            return Err(BenchError::Run(format!(
                                "writer {writer} {op:?} failed: {m}"
                            )))
                        }
                    }
                }
            }
        }
        if let Some(h) = hist {
            h.record(t0.elapsed().as_micros() as u64);
        }
        Ok(())
    };

    // Warm-up window: drive load but discard measurements.
    let warm_until = Instant::now() + warmup;
    let mut scratch = 0u64;
    while Instant::now() < warm_until {
        one(&mut conn, &mut rng, &mut insert_seq, None, &mut scratch)?;
    }

    let mut hist = Histogram::new();
    let mut conflicts = 0u64;
    let until = Instant::now() + duration;
    while Instant::now() < until {
        one(
            &mut conn,
            &mut rng,
            &mut insert_seq,
            Some(&mut hist),
            &mut conflicts,
        )?;
    }

    Ok(Tally { conflicts, hist })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Over many draws, a [`Mix`]'s op distribution tracks its configured ratios
    /// within a small tolerance — the property the scenarios depend on.
    #[test]
    fn mix_draws_track_the_configured_ratios() {
        let mix = Scenario::MixedOltp.mix(); // 70 / 20 / 8 / 2
        let mut rng = Rng::new(1);
        let n = 100_000u64;
        let (mut s, mut i, mut u, mut d) = (0u64, 0u64, 0u64, 0u64);
        for _ in 0..n {
            match mix.pick(rng.next_u64()) {
                Op::Select => s += 1,
                Op::Insert => i += 1,
                Op::Update => u += 1,
                Op::Delete => d += 1,
            }
        }
        let frac = |c: u64| c as f64 / n as f64;
        assert!((frac(s) - 0.70).abs() < 0.02, "select {}", frac(s));
        assert!((frac(i) - 0.20).abs() < 0.02, "insert {}", frac(i));
        assert!((frac(u) - 0.08).abs() < 0.02, "update {}", frac(u));
        assert!((frac(d) - 0.02).abs() < 0.02, "delete {}", frac(d));
    }

    /// A pure-read mix never draws a write, and a pure-write mix never reads —
    /// the boundary cases that keep `read-heavy`/`write-heavy` faithful.
    #[test]
    fn boundary_weights_never_draw_the_zero_class() {
        let mut rng = Rng::new(42);
        let read_only = Mix::new(1, 0, 0, 0);
        let write_only = Mix::new(0, 1, 0, 0);
        for _ in 0..1000 {
            assert_eq!(read_only.pick(rng.next_u64()), Op::Select);
            assert_eq!(write_only.pick(rng.next_u64()), Op::Insert);
        }
    }

    /// The PRNG is deterministic for a given seed (reproducible runs) and two
    /// seeds diverge (writers don't march in lockstep).
    #[test]
    fn rng_is_deterministic_and_seed_sensitive() {
        let seq = |seed| {
            let mut r = Rng::new(seed);
            (0..8).map(|_| r.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(seq(7), seq(7));
        assert_ne!(seq(7), seq(8));
    }
}
