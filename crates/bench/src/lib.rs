//! `twill-bench` — the benchmarking, correctness, and serverless-efficiency
//! driver for Twill DB (spec 09 — the falsifiable validation plan; spec 15 — the
//! CLI that operationalizes it; issue #6 / #29). It reports latency as
//! **percentiles** (p50/p90/p95/p99/p999 via an HDR-style [`Histogram`]), never
//! mean-only, exactly as spec 09 mandates, and — for the correctness profiles —
//! asserts ACID invariants over the data it just drove, failing the run (exit
//! code 2) when an invariant is violated regardless of how fast it was.
//!
//! Two transports drive the *same* experiments, so the embedded and server paths
//! are measured the same way (spec 09 — "it applies to both the embedded FFI and
//! server pgwire paths"):
//!
//!   * **embedded** (default) — drive the engine directly through the in-process
//!     Rust/FFI API. The offline-buildable smoke path.
//!   * **pgwire** — drive the engine through the Postgres wire protocol via
//!     [`pgclient`]. With no `--server`, the driver spins up an in-process
//!     [`twill-server`] listener (so the wire path is offline-testable); with
//!     `--server host:port` it points at a deployed `engine-server` (the form a
//!     real-host run, or `pgbench`, takes).
//!
//! Either transport runs against any `--url` backend (`file://` for a smoke run,
//! an object-store URL on a real host for the W1 tail that gates the
//! architecture). Subcommands group into four families (spec 15 "Command
//! structure"):
//!   * **experiments** (`exp1`/`exp2`/`exp3`) — the spec-09 latency floor,
//!     group-commit curve, and contention wall.
//!   * **request-mix scenarios** (`read-heavy`/`write-heavy`/`mixed-oltp`) — named
//!     workload shapes driving a ratio-controlled SELECT/INSERT/UPDATE/DELETE mix.
//!   * **correctness profiles** (`counter`/`bank-transfer`/`inventory`/
//!     `document-editing`) — drive a contended workload, then assert an ACID
//!     invariant over the result (no lost update, conserved balance, no oversell,
//!     no lost edit) and exit non-zero on violation.
//!   * **lifecycle scenarios** (`scale-to-zero`) — controller-driven cold-path
//!     measurement (spec 09 Experiment 5): drive query → idle past the reaper →
//!     query, pull the [`controller`] stats snapshot at the run boundaries, and
//!     report the cold-boot distribution + the serverless-efficiency figures.
//!   * **release comparison** (`compare`) — diff two archived JSON records into a
//!     PASS/regression verdict.

pub mod compare;
pub mod correctness;
pub mod hist;
pub mod lifecycle;
pub mod pgclient;
pub mod workload;

use engine::{Connection, EngineStatus, Value};
use hist::Histogram;
use pgclient::{ExecError, PgClient};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const TABLE_LEDGER: &str = "bench_ledger";
pub(crate) const TABLE_COUNTER: &str = "bench_counter";

/// Process exit codes (spec 15 "Exit codes"). A run is only a success (`0`) when
/// it both completed *and* preserved correctness; a fast run that loses an acked
/// write exits `2`, not `0`.
pub mod exit {
    /// Success — completed and (for correctness profiles) invariants held.
    pub const OK: i32 = 0;
    /// The benchmark itself failed (engine/transport error, or `compare`
    /// detected a regression).
    pub const BENCH_FAILED: i32 = 1;
    /// A correctness invariant was violated (lost update, broken balance, …).
    pub const CORRECTNESS: i32 = 2;
    /// Bad flags / usage / unknown subcommand.
    pub const CONFIG: i32 = 3;
    /// Could not connect to / open the target.
    pub const CONNECTION: i32 = 4;
}

/// A driver failure, carrying which [`exit`] code it maps to. Run-time failures
/// (the common case) convert from `String` via [`From`], so `?` on an internal
/// `Result<_, String>` yields a [`BenchError::Run`]; the connection and config
/// classes are constructed explicitly where they arise.
#[derive(Debug)]
pub enum BenchError {
    /// Bad flags / usage → [`exit::CONFIG`].
    Config(String),
    /// Could not connect to / open the target → [`exit::CONNECTION`].
    Connection(String),
    /// Engine/transport error mid-run → [`exit::BENCH_FAILED`].
    Run(String),
}

impl BenchError {
    pub fn code(&self) -> i32 {
        match self {
            BenchError::Config(_) => exit::CONFIG,
            BenchError::Connection(_) => exit::CONNECTION,
            BenchError::Run(_) => exit::BENCH_FAILED,
        }
    }
}

impl std::fmt::Display for BenchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BenchError::Config(m) => write!(f, "configuration error: {m}"),
            BenchError::Connection(m) => write!(f, "connection error: {m}"),
            BenchError::Run(m) => write!(f, "benchmark failed: {m}"),
        }
    }
}

impl From<String> for BenchError {
    fn from(m: String) -> Self {
        BenchError::Run(m)
    }
}

/// CLI entry point (the `twill-bench` binary is a thin shim over this). Computes
/// the [`exit`] code and terminates the process with it.
pub fn cli_main() {
    std::process::exit(run_cli(&std::env::args().collect::<Vec<_>>()));
}

/// The dispatch core, factored out of [`cli_main`] so tests can assert exit
/// codes without spawning a process.
pub fn run_cli(args: &[String]) -> i32 {
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let rest = &args[2.min(args.len())..];

    // `compare` is post-processing over archived records, not a run; it owns its
    // own flag parsing and returns an exit code directly.
    if cmd == "compare" {
        return compare::run(rest);
    }
    if matches!(cmd, "help" | "-h" | "--help") {
        print_help();
        return exit::OK;
    }

    let opts = match Opts::parse(rest) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            return exit::CONFIG;
        }
    };

    let result = match cmd {
        "exp1" => run_experiment(Experiment::LatencyFloor, &opts),
        "exp2" => run_experiment(Experiment::GroupCommit, &opts),
        "exp3" => run_experiment(Experiment::Contention, &opts),
        "read-heavy" => workload::run_scenario(workload::Scenario::ReadHeavy, &opts),
        "write-heavy" => workload::run_scenario(workload::Scenario::WriteHeavy, &opts),
        "mixed-oltp" => workload::run_scenario(workload::Scenario::MixedOltp, &opts),
        "counter" => correctness::run_counter(&opts),
        "bank-transfer" => correctness::run_bank_transfer(&opts),
        "inventory" => correctness::run_inventory(&opts),
        "document-editing" => correctness::run_document_editing(&opts),
        "scale-to-zero" => lifecycle::run_scale_to_zero(&opts),
        other => {
            eprintln!("error: unknown subcommand '{other}'\n");
            print_help();
            return exit::CONFIG;
        }
    };

    match result {
        Ok(report) => {
            report.print();
            // A profile that drove the data but broke an invariant is a failure,
            // however fast it ran (spec 15 — correctness gates the exit code).
            if report.correctness.as_ref().is_some_and(|c| !c.passed) {
                exit::CORRECTNESS
            } else {
                exit::OK
            }
        }
        Err(e) => {
            eprintln!("{e}");
            e.code()
        }
    }
}

#[derive(Clone, Copy)]
enum Experiment {
    LatencyFloor,
    GroupCommit,
    Contention,
}

impl Experiment {
    fn name(self) -> &'static str {
        match self {
            Experiment::LatencyFloor => "exp1-latency-floor",
            Experiment::GroupCommit => "exp2-group-commit",
            Experiment::Contention => "exp3-contention-wall",
        }
    }
}

/// Which front door the writers drive (spec 09: embedded FFI vs server pgwire).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Embedded,
    Pgwire,
}

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Transport::Embedded => "embedded",
            Transport::Pgwire => "pgwire",
        }
    }
}

fn parse_transport(s: &str) -> Result<Transport, String> {
    match s {
        "embedded" => Ok(Transport::Embedded),
        "pgwire" => Ok(Transport::Pgwire),
        other => Err(format!("invalid --transport '{other}'")),
    }
}

/// Test-only fault injection for validating the correctness checkers themselves.
/// `--inject-fault lost-update` makes a correctness profile deliberately drop one
/// acked write, so the negative test can prove a violated invariant maps to
/// [`exit::CORRECTNESS`] rather than only ever exercising the PASS path on a
/// correct engine. Undocumented in `--help`: it is a QA hook, not a user feature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    LostUpdate,
}

fn parse_fault(s: &str) -> Result<Fault, String> {
    match s {
        "lost-update" => Ok(Fault::LostUpdate),
        other => Err(format!("invalid --inject-fault '{other}'")),
    }
}

pub(crate) struct Opts {
    pub url: String,
    pub writers: usize,
    pub warmup: Duration,
    pub duration: Duration,
    /// Per-writer operation count for the fixed-work correctness profiles
    /// (`counter`/`bank-transfer`/`inventory`/`document-editing`), where the
    /// expected result must be known exactly.
    pub ops: u64,
    /// Pre-seeded row count for the request-mix scenarios' working set.
    pub rows: u64,
    pub label: String,
    pub transport: Transport,
    /// Emit only the one-line JSON record on stdout (for scripting / CI).
    pub json: bool,
    /// `Some(addr)` drives an already-running `engine-server` over pgwire;
    /// `None` (with `--transport pgwire`) spins one up in-process.
    pub server: Option<String>,
    /// Test-only fault to inject into a correctness profile (see [`Fault`]);
    /// `None` in every real run.
    pub inject_fault: Option<Fault>,
    /// Number of scale-to-zero cycles (cold-boot samples) for the
    /// `scale-to-zero` lifecycle scenario.
    pub cycles: u64,
    /// Idle timeout before the controller's reaper tears a warm instance down,
    /// for the `scale-to-zero` scenario. Short for a smoke run; spec 09's
    /// Experiment 5 uses a long (≈10 min) window on a real deployment.
    pub idle: Duration,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut raw = RawOpts::default();
        let mut i = 0;
        while i < args.len() {
            let key = args[i].as_str();
            // `--json` is a bare flag; everything else takes a value, fetched
            // once here so the per-flag match stays a flat, low-branch dispatch.
            if key == "--json" {
                raw.json = true;
                i += 1;
                continue;
            }
            let value = args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {key}"))?;
            raw.set(key, value)?;
            i += 2;
        }
        raw.finish()
    }
}

/// Mutable accumulator for [`Opts::parse`]: one field per flag, filled as the
/// argv is walked. Split out so the flag dispatch ([`RawOpts::set`]) and the
/// final validation ([`RawOpts::finish`]) are each a small, single-purpose unit.
struct RawOpts {
    url: Option<String>,
    writers: usize,
    warmup_ms: u64,
    duration_ms: u64,
    ops: u64,
    rows: u64,
    label: String,
    transport: Transport,
    json: bool,
    server: Option<String>,
    inject_fault: Option<Fault>,
    cycles: u64,
    idle_ms: u64,
}

impl Default for RawOpts {
    fn default() -> RawOpts {
        RawOpts {
            url: None,
            writers: 1,
            warmup_ms: 200,
            duration_ms: 1000,
            ops: 200,
            rows: 1000,
            label: String::new(),
            transport: Transport::Embedded,
            json: false,
            server: None,
            inject_fault: None,
            cycles: 20,
            idle_ms: 100,
        }
    }
}

impl RawOpts {
    /// Apply one `--flag value` pair (the value already fetched by the caller).
    fn set(&mut self, key: &str, value: String) -> Result<(), String> {
        match key {
            "--url" => self.url = Some(value),
            "--writers" => self.writers = parse_num(&value, key)?,
            "--warmup-ms" => self.warmup_ms = parse_num(&value, key)?,
            "--duration-ms" => self.duration_ms = parse_num(&value, key)?,
            "--ops" => self.ops = parse_num(&value, key)?,
            "--rows" => self.rows = parse_num(&value, key)?,
            "--cycles" => self.cycles = parse_num(&value, key)?,
            "--idle-ms" => self.idle_ms = parse_num(&value, key)?,
            "--label" => self.label = value,
            "--transport" => self.transport = parse_transport(&value)?,
            "--server" => self.server = Some(value),
            "--inject-fault" => self.inject_fault = Some(parse_fault(&value)?),
            other => return Err(format!("unknown flag {other}")),
        }
        Ok(())
    }

    /// Validate and finalize into an [`Opts`].
    fn finish(mut self) -> Result<Opts, String> {
        // `--server` implies the pgwire transport (it has no meaning embedded).
        if self.server.is_some() {
            self.transport = Transport::Pgwire;
        }
        Ok(Opts {
            url: self
                .url
                .ok_or("--url is required (e.g. file:///tmp/bench.db or s3://bucket/db)")?,
            writers: self.writers.max(1),
            warmup: Duration::from_millis(self.warmup_ms),
            duration: Duration::from_millis(self.duration_ms),
            ops: self.ops.max(1),
            rows: self.rows.max(1),
            label: self.label,
            transport: self.transport,
            json: self.json,
            server: self.server,
            inject_fault: self.inject_fault,
            cycles: self.cycles.max(1),
            idle: Duration::from_millis(self.idle_ms.max(1)),
        })
    }
}

/// Parse a numeric flag value, mapping a parse failure to `invalid <flag>`.
fn parse_num<T: std::str::FromStr>(value: &str, flag: &str) -> Result<T, String> {
    value.parse().map_err(|_| format!("invalid {flag}"))
}

/// What a writer connects to, cloneable so each writer thread opens its own.
#[derive(Clone)]
pub(crate) enum Target {
    /// Drive the engine in-process at this URL.
    Embedded(String),
    /// Drive the engine through pgwire at this `host:port`.
    Pgwire(String),
}

impl Target {
    /// Open a writer against this target. A failure here is a [connection
    /// error](exit::CONNECTION), not a generic run failure.
    pub fn open(&self) -> Result<Writer, BenchError> {
        match self {
            Target::Embedded(url) => Connection::open(url)
                .map(Writer::Embedded)
                .map_err(|e| BenchError::Connection(format!("open {url}: {e}"))),
            Target::Pgwire(addr) => PgClient::connect(addr)
                .map(Writer::Pg)
                .map_err(|e| BenchError::Connection(format!("connect {addr}: {e}"))),
        }
    }
}

/// A transport-agnostic writer: an embedded connection or a pgwire client.
pub(crate) enum Writer {
    Embedded(Connection),
    Pg(PgClient),
}

/// The classification a writer loop reacts to, identical across transports.
pub(crate) enum Outcome {
    Ok,
    Conflict,
    Fatal(String),
}

impl Writer {
    /// Run a statement, classifying the result the same way over either
    /// transport: clean commit, retry-able conflict, or fatal error.
    pub fn exec(&mut self, sql: &str) -> Outcome {
        match self {
            Writer::Embedded(c) => match c.exec(sql) {
                Ok(()) => Outcome::Ok,
                Err(e) if e.status == EngineStatus::ErrConflict => Outcome::Conflict,
                Err(e) => Outcome::Fatal(e.to_string()),
            },
            Writer::Pg(c) => match c.exec(sql) {
                Ok(()) => Outcome::Ok,
                Err(ExecError::Conflict) => Outcome::Conflict,
                Err(ExecError::Fatal(m)) => Outcome::Fatal(m),
            },
        }
    }

    /// Read a single integer scalar (first cell of the first row), identical
    /// across transports — used by the mix read path and the post-run
    /// correctness assertions.
    pub fn query_i64(&mut self, sql: &str) -> Result<i64, String> {
        match self {
            Writer::Embedded(c) => {
                let rs = c.query(sql).map_err(|e| e.to_string())?;
                match rs.rows.first().and_then(|r| r.first()) {
                    Some(Value::Int(i)) => Ok(*i),
                    Some(v) => Err(format!("non-integer scalar: {}", v.type_name())),
                    None => Err("query returned no rows".into()),
                }
            }
            Writer::Pg(c) => c.query_scalar_i64(sql).map_err(|e| match e {
                ExecError::Conflict => "unexpected conflict on read".to_string(),
                ExecError::Fatal(m) => m,
            }),
        }
    }

    /// Run a read for its side effect of exercising the read path, discarding the
    /// rows. A point read that matches *zero* rows (e.g. a key a concurrent
    /// writer just deleted in the mix) is a valid, successful read — unlike
    /// [`Writer::query_i64`], which is for assertions that require a row.
    pub fn read(&mut self, sql: &str) -> Result<(), String> {
        match self {
            Writer::Embedded(c) => c.query(sql).map(|_| ()).map_err(|e| e.to_string()),
            Writer::Pg(c) => c.exec(sql).map_err(|e| match e {
                ExecError::Conflict => "unexpected conflict on read".to_string(),
                ExecError::Fatal(m) => m,
            }),
        }
    }
}

/// A writer's tally from one run window.
pub(crate) struct Tally {
    pub conflicts: u64,
    pub hist: Histogram,
}

/// Resolve the run target, spinning up an in-process pgwire server if needed.
pub(crate) fn resolve_target(opts: &Opts) -> Result<Target, BenchError> {
    match opts.transport {
        Transport::Embedded => Ok(Target::Embedded(opts.url.clone())),
        Transport::Pgwire => match &opts.server {
            Some(addr) => Ok(Target::Pgwire(addr.clone())),
            None => {
                // Spin up an in-process listener on an ephemeral port serving the
                // same `--url` backend; the bind happens here so a client can
                // connect immediately, the accept loop runs on a detached thread.
                let listener = TcpListener::bind("127.0.0.1:0")
                    .map_err(|e| BenchError::Connection(format!("bind in-process server: {e}")))?;
                let addr = listener
                    .local_addr()
                    .map_err(|e| BenchError::Connection(format!("server addr: {e}")))?
                    .to_string();
                let url = opts.url.clone();
                std::thread::spawn(move || {
                    let _ = twill_server::serve_listener(listener, &url);
                });
                Ok(Target::Pgwire(addr))
            }
        },
    }
}

/// A per-run tag keeping inserted keys unique across repeated runs against the
/// same durable database (a `PRIMARY KEY` would otherwise collide). Not a
/// cryptographic value — just a uniqueness suffix for benchmark row keys.
pub(crate) fn run_tag() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

fn run_experiment(exp: Experiment, opts: &Opts) -> Result<Report, BenchError> {
    // exp1 is the single-writer floor regardless of --writers.
    let writers = match exp {
        Experiment::LatencyFloor => 1,
        _ => opts.writers,
    };
    let same_row = matches!(exp, Experiment::Contention);

    let target = resolve_target(opts)?;

    // Setup: schema + (for the contention case) the one shared counter row, over
    // the chosen transport so the server path also exercises DDL on the wire.
    let mut setup = target.open()?;
    setup_schema(&mut setup, same_row)?;

    let tag = run_tag();

    let (tallies, elapsed) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let target = target.clone();
                let warmup = opts.warmup;
                let duration = opts.duration;
                scope.spawn(move || writer_loop(&target, w, tag, same_row, warmup, duration))
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
    let commits = merged.count();

    Ok(Report {
        experiment: exp.name(),
        label: opts.label.clone(),
        transport: opts.transport.name(),
        url_scheme: url_scheme(&opts.url),
        writers,
        duration_s: elapsed.as_secs_f64(),
        commits,
        conflicts,
        failures: 0,
        throughput: commits as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        hist: merged,
        git_sha: git_sha(),
        json_only: opts.json,
        correctness: None,
        lifecycle: None,
    })
}

fn setup_schema(w: &mut Writer, same_row: bool) -> Result<(), BenchError> {
    match w.exec(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE_LEDGER} (k TEXT PRIMARY KEY, v INTEGER)"
    )) {
        Outcome::Ok | Outcome::Conflict => {}
        Outcome::Fatal(m) => return Err(BenchError::Run(format!("create ledger: {m}"))),
    }
    match w.exec(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE_COUNTER} (id INTEGER PRIMARY KEY, n INTEGER)"
    )) {
        Outcome::Ok | Outcome::Conflict => {}
        Outcome::Fatal(m) => return Err(BenchError::Run(format!("create counter: {m}"))),
    }
    if same_row {
        // Idempotent across reruns: ignore a duplicate-key (already seeded) error.
        let _ = w.exec(&format!("INSERT INTO {TABLE_COUNTER} VALUES (1, 0)"));
    }
    Ok(())
}

/// One writer: warm up (discarded), then commit in a timed window, recording the
/// per-commit latency (including any first-committer-wins retry, which is what a
/// real client experiences — identical across embedded and pgwire transports).
fn writer_loop(
    target: &Target,
    writer: usize,
    tag: u128,
    same_row: bool,
    warmup: Duration,
    duration: Duration,
) -> Result<Tally, BenchError> {
    let mut conn = target.open()?;
    let seq = AtomicU64::new(0);

    let one = |conn: &mut Writer,
               hist: Option<&mut Histogram>,
               conflicts: &mut u64|
     -> Result<(), BenchError> {
        let t0 = Instant::now();
        loop {
            let sql = if same_row {
                format!("UPDATE {TABLE_COUNTER} SET n = n + 1 WHERE id = 1")
            } else {
                let i = seq.fetch_add(1, Ordering::Relaxed);
                format!("INSERT INTO {TABLE_LEDGER} (k, v) VALUES ('{tag}-{writer}-{i}', 1)")
            };
            match conn.exec(&sql) {
                Outcome::Ok => break,
                Outcome::Conflict => {
                    *conflicts += 1;
                    continue;
                }
                Outcome::Fatal(m) => {
                    return Err(BenchError::Run(format!(
                        "writer {writer} commit failed: {m}"
                    )))
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
        one(&mut conn, None, &mut scratch)?;
    }

    // Timed window (each recorded sample is one commit, so hist.count() == commits).
    let mut hist = Histogram::new();
    let mut conflicts = 0u64;
    let until = Instant::now() + duration;
    while Instant::now() < until {
        one(&mut conn, Some(&mut hist), &mut conflicts)?;
    }

    Ok(Tally { conflicts, hist })
}

/// The result of any correctness profile: a named ACID invariant, whether it
/// held, and a human detail string (e.g. `expected 1200, got 1200`).
pub(crate) struct Correctness {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// The controller-sourced lifecycle section of a [`Report`], present only for
/// the `scale-to-zero` scenario (spec 15 — serverless-efficiency). Carries the
/// `ControllerStats` deltas observed across the run plus the derived
/// serverless-efficiency figures (pure arithmetic over those deltas), reported
/// under the settled `twill_*` metric vocabulary.
pub(crate) struct Lifecycle {
    /// Cold→warm transitions over the run (one per cold-boot cycle).
    pub cold_starts: u64,
    /// Reuses of an already-warm instance.
    pub warm_starts: u64,
    /// Warm→Cold teardowns (the scale-to-zero events the scenario drives).
    pub scale_to_zero: u64,
    /// Peak databases warming simultaneously.
    pub peak_workers: u64,
    /// Cumulative compute time the instance spent serving / warm-but-idle.
    pub compute_active_us: u64,
    pub compute_idle_us: u64,
    /// Cumulative warm-admission wait (the scheduler/admission segment).
    pub admission_wait_us: u64,
    /// Lease heartbeats observed over the run.
    pub lease_renews: u64,
    /// Backend page versions fetched across the cold reads — the numerator of
    /// `storage_reads_per_query`, pulled from `EngineStats.storage` (the
    /// `StorageStats` snapshot) after each warm read. `0` on the embedded
    /// `file://` path (the in-memory store serves the read with no backend
    /// fetch); nonzero on an object store with a cold cache.
    pub page_reads: u64,
    /// Cold reads driven (one per cycle) — the denominator for per-query figures.
    pub queries: u64,
}

impl Lifecycle {
    /// `utilization` = active / (active + idle); the share of resident time the
    /// compute actually served. `0.0` when nothing was resident.
    fn utilization(&self) -> f64 {
        let denom = (self.compute_active_us + self.compute_idle_us) as f64;
        if denom > 0.0 {
            self.compute_active_us as f64 / denom
        } else {
            0.0
        }
    }
    /// `compute_seconds_per_query` = active seconds / cold reads driven.
    fn compute_seconds_per_query(&self) -> f64 {
        if self.queries > 0 {
            (self.compute_active_us as f64 / 1e6) / self.queries as f64
        } else {
            0.0
        }
    }
    /// `storage_reads_per_query` = backend page reads / cold reads driven — how
    /// much the storage layer was hit per query. `0.0` when the warm in-memory
    /// store satisfied every read without a backend fetch (the `file://` path).
    fn storage_reads_per_query(&self) -> f64 {
        if self.queries > 0 {
            self.page_reads as f64 / self.queries as f64
        } else {
            0.0
        }
    }
    /// `avg_worker_lifetime` (seconds) = total resident time / workers spawned.
    /// Each cold start materializes a worker that lives (active + idle) until it
    /// scales to zero, so `cold_starts` is the worker count. `0.0` if none ran.
    fn avg_worker_lifetime_s(&self) -> f64 {
        if self.cold_starts > 0 {
            ((self.compute_active_us + self.compute_idle_us) as f64 / 1e6) / self.cold_starts as f64
        } else {
            0.0
        }
    }
    /// `worker_reuse_ratio` = warm starts / all starts; the share of `start`
    /// calls that hit an already-warm instance. `0.0` if nothing started.
    fn worker_reuse_ratio(&self) -> f64 {
        let starts = (self.cold_starts + self.warm_starts) as f64;
        if starts > 0.0 {
            self.warm_starts as f64 / starts
        } else {
            0.0
        }
    }
}

pub(crate) struct Report {
    pub experiment: &'static str,
    pub label: String,
    pub transport: &'static str,
    pub url_scheme: String,
    pub writers: usize,
    pub duration_s: f64,
    /// Successful operations (commits / reads) recorded in the timed window.
    pub commits: u64,
    /// Retry-able conflicts encountered and retried (never lost).
    pub conflicts: u64,
    /// Operations that failed irrecoverably (non-zero is itself a red flag).
    pub failures: u64,
    pub throughput: f64,
    pub hist: Histogram,
    pub git_sha: String,
    /// When true, [`Report::print`] emits only the one-line JSON record.
    pub json_only: bool,
    /// Present for correctness profiles; drives the `exit::CORRECTNESS` gate.
    pub correctness: Option<Correctness>,
    /// Present for the `scale-to-zero` scenario; the controller-sourced
    /// lifecycle deltas + derived serverless-efficiency figures.
    pub lifecycle: Option<Lifecycle>,
}

impl Report {
    fn print(&self) {
        let p = |q: f64| self.hist.value_at_quantile(q);
        if !self.json_only {
            // Human-readable summary.
            println!("── {} ─────────────────────────────", self.experiment);
            if !self.label.is_empty() {
                println!("label        {}", self.label);
            }
            println!("transport    {}", self.transport);
            println!("backend      {}://", self.url_scheme);
            println!("git          {}", self.git_sha);
            println!("writers      {}", self.writers);
            println!("duration     {:.2}s", self.duration_s);
            println!("ops          {} (ok)", self.commits);
            println!("conflicts    {} (retried)", self.conflicts);
            println!("failures     {}", self.failures);
            println!("throughput   {:.0} ops/s", self.throughput);
            println!(
                "latency µs   p50={}  p90={}  p95={}  p99={}  p999={}  min={}  max={}  mean={:.1}",
                p(0.50),
                p(0.90),
                p(0.95),
                p(0.99),
                p(0.999),
                self.hist.min(),
                self.hist.max(),
                self.hist.mean(),
            );
            if let Some(c) = &self.correctness {
                println!(
                    "correctness  {} — {} ({})",
                    c.name,
                    if c.passed { "PASS" } else { "FAIL" },
                    c.detail,
                );
            }
            if let Some(l) = &self.lifecycle {
                // The cold-boot distribution is the `hist` printed above; this is
                // the controller-sourced lifecycle + serverless-efficiency view.
                println!("cold starts  {}", l.cold_starts);
                println!("warm starts  {}", l.warm_starts);
                println!("scale→0      {}", l.scale_to_zero);
                println!("peak workers {}", l.peak_workers);
                println!(
                    "compute      active={:.3}s  idle={:.3}s  admission_wait={}µs  lease_renews={}",
                    l.compute_active_us as f64 / 1e6,
                    l.compute_idle_us as f64 / 1e6,
                    l.admission_wait_us,
                    l.lease_renews,
                );
                println!(
                    "reuse        worker_reuse_ratio={:.3}  storage_reads={}",
                    l.worker_reuse_ratio(),
                    l.page_reads,
                );
                // The serverless-efficiency report (spec 15): operational cost,
                // pure arithmetic over the controller + storage snapshot deltas.
                println!(
                    "efficiency   utilization={:.3}  compute_s/query={:.6}  \
                     storage_reads/query={:.3}  avg_worker_lifetime={:.3}s",
                    l.utilization(),
                    l.compute_seconds_per_query(),
                    l.storage_reads_per_query(),
                    l.avg_worker_lifetime_s(),
                );
            }
        }
        // Machine-readable record (one JSON line) for archiving / plotting.
        let correctness = match &self.correctness {
            Some(c) => format!(
                "{{\"name\":\"{}\",\"passed\":{},\"detail\":\"{}\"}}",
                c.name, c.passed, c.detail
            ),
            None => "null".to_string(),
        };
        // The lifecycle section reports the controller deltas + derived
        // serverless-efficiency figures under the settled `twill_*` names.
        let lifecycle = match &self.lifecycle {
            Some(l) => format!(
                "{{\"twill_cold_start_total\":{},\"twill_warm_start_total\":{},\
                 \"twill_worker_reuse_ratio\":{:.6},\
                 \"twill_scale_to_zero_total\":{},\"twill_peak_workers\":{},\
                 \"twill_compute_active_seconds_total\":{:.6},\
                 \"twill_compute_idle_seconds_total\":{:.6},\"twill_admission_wait_us\":{},\
                 \"twill_lease_renew_total\":{},\"twill_storage_page_reads_total\":{},\
                 \"utilization\":{:.6},\"compute_seconds_per_query\":{:.6},\
                 \"storage_reads_per_query\":{:.6},\"scale_to_zero_count\":{},\
                 \"avg_worker_lifetime\":{:.6}}}",
                l.cold_starts,
                l.warm_starts,
                l.worker_reuse_ratio(),
                l.scale_to_zero,
                l.peak_workers,
                l.compute_active_us as f64 / 1e6,
                l.compute_idle_us as f64 / 1e6,
                l.admission_wait_us,
                l.lease_renews,
                l.page_reads,
                l.utilization(),
                l.compute_seconds_per_query(),
                l.storage_reads_per_query(),
                l.scale_to_zero,
                l.avg_worker_lifetime_s(),
            ),
            None => "null".to_string(),
        };
        println!(
            "{{\"experiment\":\"{}\",\"label\":\"{}\",\"transport\":\"{}\",\"backend\":\"{}\",\
             \"git\":\"{}\",\"writers\":{},\"duration_s\":{:.3},\"commits\":{},\"conflicts\":{},\
             \"failures\":{},\"throughput_per_s\":{:.1},\"p50_us\":{},\"p90_us\":{},\"p95_us\":{},\
             \"p99_us\":{},\"p999_us\":{},\"min_us\":{},\"max_us\":{},\"mean_us\":{:.1},\
             \"correctness\":{},\"lifecycle\":{}}}",
            self.experiment,
            self.label,
            self.transport,
            self.url_scheme,
            self.git_sha,
            self.writers,
            self.duration_s,
            self.commits,
            self.conflicts,
            self.failures,
            self.throughput,
            p(0.50),
            p(0.90),
            p(0.95),
            p(0.99),
            p(0.999),
            self.hist.min(),
            self.hist.max(),
            self.hist.mean(),
            correctness,
            lifecycle,
        );
    }
}

/// The scheme of a connection URL (`file`, `s3`, …) for the report, defaulting
/// to `?` if the URL is malformed.
pub(crate) fn url_scheme(url: &str) -> String {
    url.split("://").next().unwrap_or("?").to_string()
}

/// Best-effort short commit SHA for reproducibility (spec 09 SHOULD); `unknown`
/// if unavailable. Honors `TWILL_BENCH_GIT_SHA` first (CI may pin it).
fn git_sha() -> String {
    if let Ok(sha) = std::env::var("TWILL_BENCH_GIT_SHA") {
        if !sha.is_empty() {
            return sha;
        }
    }
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn print_help() {
    eprintln!(
        "twill-bench — benchmark, correctness & efficiency driver (spec 09 / 15)\n\
         \n\
         USAGE:\n\
         \x20 twill-bench <command> --url <URL> [flags]\n\
         \n\
         EXPERIMENTS (spec 09):\n\
         \x20 exp1            single-commit latency floor (one sequential writer)\n\
         \x20 exp2            group-commit throughput curve (N independent-row writers)\n\
         \x20 exp3            write-contention wall (N writers on the same row)\n\
         \n\
         REQUEST-MIX SCENARIOS (ratio-controlled SELECT/INSERT/UPDATE/DELETE):\n\
         \x20 read-heavy      90%% read / 10%% insert\n\
         \x20 write-heavy     20%% read / 80%% insert\n\
         \x20 mixed-oltp      70%% read / 20%% insert / 8%% update / 2%% delete\n\
         \n\
         CORRECTNESS PROFILES (assert an invariant; exit 2 on violation):\n\
         \x20 counter         N writers increment one row; asserts zero lost updates\n\
         \x20 bank-transfer   concurrent transfers; asserts the summed balance is conserved\n\
         \x20 inventory       concurrent stock decrements; asserts no oversell (no negative stock)\n\
         \x20 document-editing concurrent edits to one row (read-modify-write); asserts no lost edit\n\
         \n\
         LIFECYCLE SCENARIOS (controller-driven; serverless-efficiency report):\n\
         \x20 scale-to-zero   query → idle past the reaper → query (spec 09 Exp 5 cold read);\n\
         \x20                 reports the cold-boot distribution + controller-sourced efficiency\n\
         \n\
         RELEASE COMPARISON:\n\
         \x20 compare --baseline <FILE> --candidate <FILE> [--threshold <FRAC>]\n\
         \n\
         FLAGS:\n\
         \x20 --url <URL>          file:///path or s3://bucket/db (required for runs)\n\
         \x20 --transport <T>      embedded (default) or pgwire (server path)\n\
         \x20 --server <HOST:PORT> drive a running engine-server (implies pgwire)\n\
         \x20 --writers <N>        concurrent writers (default 1)\n\
         \x20 --warmup-ms <MS>     discarded warm-up window (default 200)\n\
         \x20 --duration-ms <MS>   timed window for experiments/scenarios (default 1000)\n\
         \x20 --ops <N>            per-writer ops for the correctness profiles (default 200)\n\
         \x20 --rows <N>           pre-seeded working-set size for the mix scenarios (default 1000)\n\
         \x20 --cycles <N>         scale-to-zero cold-boot samples (default 20)\n\
         \x20 --idle-ms <MS>       idle window before the reaper scales to zero (default 100)\n\
         \x20 --label <TEXT>       free-form tag recorded in the output\n\
         \x20 --json               emit only the one-line JSON record (for scripting)\n\
         \n\
         Reports p50/p90/p95/p99/p999 (never mean-only) plus a JSON line for archiving.\n\
         `--transport pgwire` without `--server` spins up an in-process listener,\n\
         so the wire path runs offline; point `--server` at a deployed engine-server\n\
         (or reproduce with pgbench) for real-host numbers. Use file:// for a smoke\n\
         run; an object-store URL on a real host for the W1 tail that gates placement."
    );
}

#[cfg(test)]
mod tests {
    use super::Lifecycle;

    /// The serverless-efficiency report (spec 15 / #53 step 3) is pure arithmetic
    /// over the controller + storage snapshot deltas. Pin every derived figure so
    /// a vocabulary or formula change is a visible test change, not a silent drift.
    #[test]
    fn serverless_efficiency_figures_are_exact() {
        // 4 cold starts, each ~3s active + ~1s idle resident; 4 cold reads, 8
        // backend page reads total; one warm reuse mixed in.
        let l = Lifecycle {
            cold_starts: 4,
            warm_starts: 1,
            scale_to_zero: 4,
            peak_workers: 1,
            compute_active_us: 12_000_000,
            compute_idle_us: 4_000_000,
            admission_wait_us: 0,
            lease_renews: 2,
            page_reads: 8,
            queries: 4,
        };
        // utilization = active / (active + idle) = 12 / 16.
        assert!((l.utilization() - 0.75).abs() < 1e-9);
        // compute_seconds_per_query = 12s / 4 = 3s.
        assert!((l.compute_seconds_per_query() - 3.0).abs() < 1e-9);
        // storage_reads_per_query = 8 reads / 4 = 2.
        assert!((l.storage_reads_per_query() - 2.0).abs() < 1e-9);
        // avg_worker_lifetime = (active + idle) / workers = 16s / 4 = 4s.
        assert!((l.avg_worker_lifetime_s() - 4.0).abs() < 1e-9);
        // worker_reuse_ratio = warm / (cold + warm) = 1 / 5.
        assert!((l.worker_reuse_ratio() - 0.2).abs() < 1e-9);
    }

    /// Every per-unit figure divides by a counter; an empty run (no workers, no
    /// queries) must yield `0.0`, never a NaN/∞ that would poison the JSON record.
    #[test]
    fn serverless_efficiency_figures_are_zero_safe() {
        let l = Lifecycle {
            cold_starts: 0,
            warm_starts: 0,
            scale_to_zero: 0,
            peak_workers: 0,
            compute_active_us: 0,
            compute_idle_us: 0,
            admission_wait_us: 0,
            lease_renews: 0,
            page_reads: 0,
            queries: 0,
        };
        for v in [
            l.utilization(),
            l.compute_seconds_per_query(),
            l.storage_reads_per_query(),
            l.avg_worker_lifetime_s(),
            l.worker_reuse_ratio(),
        ] {
            assert_eq!(v, 0.0);
            assert!(v.is_finite());
        }
    }
}
