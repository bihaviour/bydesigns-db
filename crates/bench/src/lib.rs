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
//! architecture). Subcommands group into families (spec 15 "Command
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
//!   * **soak scenario** (`long-run`) — a stability test (issue #80): drive a
//!     steady load while an interval sampler captures a `stats()` + resource
//!     time series, then fit a slope over memory/fds/p99 and fail on a
//!     leak/drift trend (exit 2), the correctness-class verdict.
//!   * **release comparison** (`compare`) — diff two archived JSON records into a
//!     PASS/regression verdict.

pub mod analysis;
pub mod boundary;
pub mod burst;
pub mod compare;
pub mod correctness;
pub mod hist;
pub mod lifecycle;
pub mod longrun;
pub mod pgclient;
/// The YAML workload-profile loader for the `custom` scenario (issue #81). It is
/// gated behind the `custom-profile` cargo feature so a default `twill-bench`
/// build carries no profile-parsing code at all (guardrail 1 of #78 / spec 15:
/// keep CLI-only concerns feature-gated and out of a default build); the parser
/// is hand-rolled (no external YAML dependency), in keeping with the workspace's
/// minimal-dependency ethos.
#[cfg(feature = "custom-profile")]
pub mod profile;
/// The spec-09 validation-campaign drivers (issue #91): the Exp-2
/// group-commit-window sweep with plateau-knee detection (V-2) and the Exp-3
/// N-database sharding orchestrator (V-3). These compose the steady-state
/// experiment writers across a swept dimension and hand the resulting curve to
/// [`analysis`] for the falsifiable verdict.
pub mod sweep;
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

    // `compare` and `boundary` are post-processing over archived records, not
    // runs; each owns its own flag parsing and returns an exit code directly.
    if cmd == "compare" {
        return compare::run(rest);
    }
    if cmd == "boundary" {
        return boundary::run(rest);
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
        "exp2-sweep" => sweep::run_group_commit_sweep(&opts),
        "exp3-shard" => sweep::run_sharding(&opts),
        "herd" => lifecycle::run_herd(&opts),
        "read-heavy" => workload::run_scenario(workload::Scenario::ReadHeavy, &opts),
        "write-heavy" => workload::run_scenario(workload::Scenario::WriteHeavy, &opts),
        "mixed-oltp" => workload::run_scenario(workload::Scenario::MixedOltp, &opts),
        "counter" => correctness::run_counter(&opts),
        "bank-transfer" => correctness::run_bank_transfer(&opts),
        "inventory" => correctness::run_inventory(&opts),
        "document-editing" => correctness::run_document_editing(&opts),
        "scale-to-zero" => lifecycle::run_scale_to_zero(&opts),
        "long-run" => longrun::run_long_run(&opts),
        "burst" => burst::run_burst(&opts),
        "custom" => run_custom(&opts),
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
            // however fast it ran (spec 15 — correctness gates the exit code). A
            // soak run that detected a leak/drift is the same class of failure
            // (issue #80 L5: drift → exit 2, #51-consistent).
            if report.failed_correctness() {
                exit::CORRECTNESS
            } else if report.failed_acceptance() {
                exit::BENCH_FAILED
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

/// Dispatch the `custom` subcommand to the feature-gated profile loader. When
/// the `custom-profile` feature is off, the loader is not compiled in, so the
/// command reports a clear rebuild hint (exit code 3) — the wall that keeps the
/// YAML profile path out of a default build (#78 guardrail 1 / #81 C2).
#[cfg(feature = "custom-profile")]
fn run_custom(opts: &Opts) -> Result<Report, BenchError> {
    profile::run_custom(opts)
}

#[cfg(not(feature = "custom-profile"))]
fn run_custom(_opts: &Opts) -> Result<Report, BenchError> {
    Err(BenchError::Config(
        "the `custom` subcommand requires the YAML profile loader, which is gated \
         behind the `custom-profile` cargo feature (kept off a default build so it \
         carries no profile-parsing code); rebuild with: \
         cargo build -p twill-bench --features custom-profile"
            .into(),
    ))
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
    /// Seed monotonic growth into the `long-run` sampled series so the trend
    /// checker has an unambiguous leak/drift to catch (issue #80 L5 negative
    /// test). Like `LostUpdate`, a QA hook — undocumented in `--help`.
    Leak,
    /// Inject a single pathological commit-latency sample into `exp1` so the V-1
    /// `p999/p50` stall gate has an unambiguous tail to fire on (issue #91 V-1
    /// acceptance: "the ratio gate fires on an injected stall and passes on a
    /// clean run"). Like the others, a QA hook — undocumented in `--help`.
    Stall,
}

fn parse_fault(s: &str) -> Result<Fault, String> {
    match s {
        "lost-update" => Ok(Fault::LostUpdate),
        "leak" => Ok(Fault::Leak),
        "stall" => Ok(Fault::Stall),
        other => Err(format!("invalid --inject-fault '{other}'")),
    }
}

#[derive(Clone)]
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
    /// for the `scale-to-zero` and `burst` scenarios. Short for a smoke run; spec
    /// 09's Experiment 5 uses a long (≈10 min) window on a real deployment.
    pub idle: Duration,
    /// Interval between time-series samples for the `long-run` soak scenario
    /// (issue #80 L1). Short for a smoke run; a real soak samples on the order of
    /// seconds over a multi-hour/day window.
    pub sample_interval: Duration,
    /// Relative drift/leak threshold for `long-run` (issue #80 L3): a gated
    /// metric is flagged when its projected growth over the analyzed window
    /// exceeds this fraction of its baseline (and an absolute floor). Default
    /// `0.10` (10%).
    pub drift_threshold: f64,
    /// Peak offered request rate (rps) for the `burst` scenario — the 20k tier of
    /// the `idle→500→5k→20k→idle` shape; the lower tiers scale with it.
    pub peak_rps: f64,
    /// Ramp duration between `burst` plateaus (the rate-change leg).
    pub ramp: Duration,
    /// Dwell at each active `burst` plateau (the hold at a tier rate).
    pub dwell: Duration,
    /// Path to a YAML workload profile for the `custom` scenario (issue #81).
    /// When set, the profile carries the run's shape and `--url` may be omitted
    /// (the profile may supply it). Only consumed by the feature-gated loader, so
    /// without that feature the field is parsed-but-unread (the rebuild-hint path).
    #[cfg_attr(not(feature = "custom-profile"), allow(dead_code))]
    pub profile: Option<String>,
    /// Deployment provenance recorded in the archived record (issue #91 V-5): the
    /// object-store region the run targeted and the host instance type. They make
    /// a baseline reproducible and let the same-region/cross-region Exp-1 variants
    /// (V-1) be told apart in the archive. Free-form; `None` when unset.
    pub region: Option<String>,
    pub instance_type: Option<String>,
    /// Exp-1 cold-connection variant (V-1): when set, each timed commit opens a
    /// fresh connection so the one-time TLS/handshake cost is paid per sample
    /// (the cold-connection distribution), rather than reusing a warm connection
    /// (steady state). Tagged into the record so the two variants are comparable.
    pub cold_connection: bool,
    /// Exp-1 stall acceptance gate (V-1): when `Some(r)`, a `p999/p50` ratio
    /// above `r` trips the gate (commit-path stall signal). `None` leaves the gate
    /// off (it is reported but never gates the exit code).
    pub max_stall_ratio: Option<f64>,
    /// Number of independent databases in the Exp-3 sharding sweep (V-3) — the
    /// lane count for the W2 lever; the sweep runs `1, 2, 4, … databases`.
    pub databases: usize,
    /// Upper bound of the Exp-2 concurrency/window sweep (V-2): the sweep walks
    /// `1, 2, 4, … writers` up to this many.
    pub sweep_max: usize,
    /// Upper bound of the Exp-5 thundering-herd concurrency sweep (V-4): the sweep
    /// fires `1, 2, 4, … concurrent cold starts` up to this many.
    pub concurrency: usize,
    /// When set, an acceptance gate on a validation sweep (the V-2 plateau guard,
    /// the V-3 sharding-slope guard, the V-4 herd-knee guard) maps a failed gate
    /// to a non-zero exit, the CI-gating form. Off by default so a smoke run
    /// reports the verdict without failing on a single host's modest numbers.
    pub gate: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut raw = RawOpts::default();
        let mut i = 0;
        while i < args.len() {
            let key = args[i].as_str();
            // Bare flags take no value; everything else takes one, fetched once
            // here so the per-flag match stays a flat, low-branch dispatch.
            match key {
                "--json" => {
                    raw.json = true;
                    i += 1;
                    continue;
                }
                "--cold-connection" => {
                    raw.cold_connection = true;
                    i += 1;
                    continue;
                }
                "--gate" => {
                    raw.gate = true;
                    i += 1;
                    continue;
                }
                _ => {}
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
    sample_interval_ms: u64,
    drift_threshold: f64,
    peak_rps: f64,
    ramp_ms: u64,
    dwell_ms: u64,
    profile: Option<String>,
    region: Option<String>,
    instance_type: Option<String>,
    cold_connection: bool,
    max_stall_ratio: Option<f64>,
    databases: usize,
    sweep_max: usize,
    concurrency: usize,
    gate: bool,
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
            sample_interval_ms: 1000,
            drift_threshold: 0.10,
            peak_rps: 20_000.0,
            ramp_ms: 50,
            dwell_ms: 150,
            profile: None,
            region: None,
            instance_type: None,
            cold_connection: false,
            max_stall_ratio: None,
            databases: 4,
            sweep_max: 8,
            concurrency: 8,
            gate: false,
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
            "--sample-interval-ms" => self.sample_interval_ms = parse_num(&value, key)?,
            "--drift-threshold" => self.drift_threshold = parse_num(&value, key)?,
            "--peak-rps" => self.peak_rps = parse_num(&value, key)?,
            "--ramp-ms" => self.ramp_ms = parse_num(&value, key)?,
            "--dwell-ms" => self.dwell_ms = parse_num(&value, key)?,
            "--label" => self.label = value,
            "--transport" => self.transport = parse_transport(&value)?,
            "--server" => self.server = Some(value),
            "--inject-fault" => self.inject_fault = Some(parse_fault(&value)?),
            "--profile" => self.profile = Some(value),
            "--region" => self.region = Some(value),
            "--instance-type" => self.instance_type = Some(value),
            "--max-stall-ratio" => self.max_stall_ratio = Some(parse_num(&value, key)?),
            "--databases" => self.databases = parse_num(&value, key)?,
            "--sweep-max" => self.sweep_max = parse_num(&value, key)?,
            "--concurrency" => self.concurrency = parse_num(&value, key)?,
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
            // `--url` is required for every run *except* `custom`, whose profile
            // may carry the URL itself; an empty string here defers the
            // "missing url" check to the profile loader (issue #81 C4).
            url: match self.url {
                Some(u) => u,
                None if self.profile.is_some() => String::new(),
                None => {
                    return Err(
                        "--url is required (e.g. file:///tmp/bench.db or s3://bucket/db)".into(),
                    )
                }
            },
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
            sample_interval: Duration::from_millis(self.sample_interval_ms.max(1)),
            drift_threshold: self.drift_threshold.max(0.0),
            peak_rps: if self.peak_rps > 0.0 {
                self.peak_rps
            } else {
                1.0
            },
            ramp: Duration::from_millis(self.ramp_ms),
            dwell: Duration::from_millis(self.dwell_ms.max(1)),
            profile: self.profile,
            region: self.region,
            instance_type: self.instance_type,
            cold_connection: self.cold_connection,
            max_stall_ratio: self.max_stall_ratio.filter(|r| *r > 0.0),
            databases: self.databases.max(1),
            sweep_max: self.sweep_max.max(1),
            concurrency: self.concurrency.max(1),
            gate: self.gate,
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
    /// Realized op counts `[select, insert, update, delete]` over the timed
    /// window — populated by the mix driver (`workload.rs`), left zero by the
    /// experiment writers, which issue a single fixed op kind.
    pub op_counts: [u64; 4],
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

    // The cold-connection variant is an Exp-1 (latency-floor) concern only; the
    // throughput experiments always reuse warm connections.
    let cold_connection = opts.cold_connection && matches!(exp, Experiment::LatencyFloor);

    let (tallies, elapsed) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let target = target.clone();
                let warmup = opts.warmup;
                let duration = opts.duration;
                scope.spawn(move || {
                    writer_loop_variant(
                        &target,
                        w,
                        tag,
                        same_row,
                        warmup,
                        duration,
                        cold_connection,
                    )
                })
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

    // Negative-test hook (V-1): fold one pathological sample into the Exp-1
    // distribution so the stall gate has an unambiguous tail to fire on — a
    // multi-thousand-× spike, far above the worst real tail a loaded host
    // produces, so the negative test stays well separated from clean-run jitter.
    // Off in every real run (no `--inject-fault stall`).
    if matches!(exp, Experiment::LatencyFloor) && opts.inject_fault == Some(Fault::Stall) {
        let p50 = merged.value_at_quantile(0.50).max(1);
        merged.record(p50.saturating_mul(5000));
    }

    let commits = merged.count();

    // Exp-1 stall acceptance gate (V-1): the p999/p50 ratio, reported always and
    // (when `--max-stall-ratio` is set) gating the exit code.
    let stall = match exp {
        Experiment::LatencyFloor => Some(StallGate::evaluate(
            merged.value_at_quantile(0.50),
            merged.value_at_quantile(0.999),
            opts.max_stall_ratio,
        )),
        _ => None,
    };

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
        soak: None,
        burst: None,
        mix_realized: None,
        archival: Archival::from_opts(opts),
        stall,
        sweep: None,
        shard: None,
        herd: None,
    })
}

pub(crate) fn setup_schema(w: &mut Writer, same_row: bool) -> Result<(), BenchError> {
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
pub(crate) fn writer_loop(
    target: &Target,
    writer: usize,
    tag: u128,
    same_row: bool,
    warmup: Duration,
    duration: Duration,
) -> Result<Tally, BenchError> {
    writer_loop_variant(target, writer, tag, same_row, warmup, duration, false)
}

/// The writer loop with the Exp-1 cold-connection variant (V-1) exposed: when
/// `cold_connection` is set each *timed* commit opens a fresh connection so the
/// one-time connect/handshake cost is folded into every sample (the
/// cold-connection distribution); when clear, one warm connection is reused for
/// the whole window (steady state). The warm-up always runs on a reused
/// connection — the variant is about the measured path, not setup.
pub(crate) fn writer_loop_variant(
    target: &Target,
    writer: usize,
    tag: u128,
    same_row: bool,
    warmup: Duration,
    duration: Duration,
    cold_connection: bool,
) -> Result<Tally, BenchError> {
    let mut conn = target.open()?;
    let seq = AtomicU64::new(0);

    // The next statement this writer issues: an UPDATE of the one shared row
    // (contention) or an INSERT of a fresh unique key (independent rows).
    let next_sql = || {
        if same_row {
            format!("UPDATE {TABLE_COUNTER} SET n = n + 1 WHERE id = 1")
        } else {
            let i = seq.fetch_add(1, Ordering::Relaxed);
            format!("INSERT INTO {TABLE_LEDGER} (k, v) VALUES ('{tag}-{writer}-{i}', 1)")
        }
    };

    // Commit once with first-committer-wins retry, recording the elapsed since
    // `t0` (which the caller starts — before the connect, for the cold variant).
    let commit = |conn: &mut Writer,
                  t0: Instant,
                  hist: Option<&mut Histogram>,
                  conflicts: &mut u64|
     -> Result<(), BenchError> {
        loop {
            match conn.exec(&next_sql()) {
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

    // Warm-up window: drive load but discard measurements (always warm-reused;
    // the variant is about the measured path, not setup).
    let warm_until = Instant::now() + warmup;
    let mut scratch = 0u64;
    while Instant::now() < warm_until {
        commit(&mut conn, Instant::now(), None, &mut scratch)?;
    }

    // Timed window (each recorded sample is one commit, so hist.count() == commits).
    let mut hist = Histogram::new();
    let mut conflicts = 0u64;
    let until = Instant::now() + duration;
    while Instant::now() < until {
        // Cold-connection variant (V-1): start the timer, *then* open a fresh
        // connection, so the handshake is inside the measured sample. Steady
        // state reuses the one warm connection opened above.
        let t0 = Instant::now();
        if cold_connection {
            conn = target.open()?;
        }
        commit(&mut conn, t0, Some(&mut hist), &mut conflicts)?;
    }

    Ok(Tally {
        conflicts,
        hist,
        op_counts: [0; 4],
    })
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

/// The configured-vs-realized op mix of a `custom` profile run (issue #81): the
/// profile's weights and the op counts the driver actually issued in the timed
/// window. Present only on a `custom` report — it is how the run proves the
/// realized mix tracked the requested shape.
pub(crate) struct MixRealized {
    /// Configured weights `[select, insert, update, delete]` from the profile.
    pub configured: [u64; 4],
    /// Realized op counts `[select, insert, update, delete]` over the run.
    pub realized: [u64; 4],
}

impl MixRealized {
    /// Realized fraction of each op kind over the run; all zero if nothing ran.
    fn fractions(&self) -> [f64; 4] {
        let total: u64 = self.realized.iter().sum();
        if total == 0 {
            return [0.0; 4];
        }
        let mut f = [0.0; 4];
        for (slot, &c) in f.iter_mut().zip(self.realized.iter()) {
            *slot = c as f64 / total as f64;
        }
        f
    }
}

/// Run provenance pinned into every archived record (issue #91 V-5): the bits
/// that make a baseline reproducible and a regression attributable — the region
/// and instance type the run targeted, the storage-backend SHA (from the
/// environment so CI can stamp the exact `crates/storage` revision under test),
/// and a wall-clock capture time. The git SHA already rides on the [`Report`]
/// itself. All optional: a `file://` smoke run carries none of them.
#[derive(Clone, Default)]
pub(crate) struct Archival {
    pub region: Option<String>,
    pub instance_type: Option<String>,
    /// `crates/storage` revision the run linked, from `TWILL_BENCH_BACKEND_SHA`.
    pub backend_sha: Option<String>,
    /// Unix seconds at which the run was captured (0 if the clock was unreadable).
    pub captured_at: u64,
}

impl Archival {
    fn from_opts(opts: &Opts) -> Archival {
        Archival {
            region: opts.region.clone(),
            instance_type: opts.instance_type.clone(),
            backend_sha: std::env::var("TWILL_BENCH_BACKEND_SHA")
                .ok()
                .filter(|s| !s.is_empty()),
            captured_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    /// The archival fields as JSON object members (no braces), comma-prefixed so
    /// they append cleanly onto the flat record. A `null` for any unset field.
    fn json_members(&self) -> String {
        let q = |o: &Option<String>| match o {
            Some(s) => format!("\"{s}\""),
            None => "null".to_string(),
        };
        format!(
            ",\"region\":{},\"instance_type\":{},\"backend_sha\":{},\"captured_at\":{}",
            q(&self.region),
            q(&self.instance_type),
            q(&self.backend_sha),
            self.captured_at,
        )
    }
}

/// The Exp-1 stall acceptance gate (issue #91 V-1): the single-commit `p999/p50`
/// ratio against an optional upper bound. A small ratio is network jitter; a
/// large one is a commit-path stall (sync compaction, CAS-retry storm) to
/// investigate before trusting any downstream curve. The gate is *reported*
/// always; it gates the exit code only when a bound (`--max-stall-ratio`) is set.
pub(crate) struct StallGate {
    pub ratio: f64,
    pub max: Option<f64>,
}

impl StallGate {
    fn evaluate(p50: u64, p999: u64, max: Option<f64>) -> StallGate {
        StallGate {
            ratio: analysis::stall_ratio(p50, p999),
            max,
        }
    }

    /// True when a bound is set and the observed ratio exceeds it.
    fn tripped(&self) -> bool {
        self.max.is_some_and(|m| self.ratio > m)
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
    /// Present for the `long-run` soak scenario; the sampled time-series summary
    /// (per-metric first/last/slope/peak) + the PASS/FAIL drift verdict (#80).
    pub soak: Option<longrun::Soak>,
    /// Present for the `burst` scenario; the offered/realized rate tracking,
    /// peak worker count, and per-ramp scaling-latency distribution.
    pub burst: Option<burst::Burst>,
    /// Present for the `custom` profile scenario (issue #81); the configured vs
    /// realized op mix, proving the driven shape tracked the requested ratios.
    pub mix_realized: Option<MixRealized>,
    /// Run provenance for the archive (issue #91 V-5); always present.
    pub archival: Archival,
    /// Present for `exp1`; the W1 stall acceptance gate (issue #91 V-1).
    pub stall: Option<StallGate>,
    /// Present for `exp2-sweep`; the group-commit-window sweep + plateau knee
    /// and the W1 plateau-engagement gate (issue #91 V-2).
    pub sweep: Option<sweep::Sweep>,
    /// Present for `exp3-shard`; the N-database sharding curve + the W2 near-
    /// linear-scaling gate / cross-DB CAS finding (issue #91 V-3).
    pub shard: Option<sweep::Shard>,
    /// Present for `herd`; the thundering-herd concurrent-cold-start curve +
    /// the spin-up saturation knee (issue #91 V-4).
    pub herd: Option<lifecycle::Herd>,
}

impl Report {
    /// Whether this run failed a correctness-class gate (exit code 2): a violated
    /// ACID invariant on a correctness profile, or a detected leak/drift on a
    /// soak run. Both fail the run however fast it was.
    fn failed_correctness(&self) -> bool {
        self.correctness.as_ref().is_some_and(|c| !c.passed)
            || self.soak.as_ref().is_some_and(|s| !s.passed())
    }

    /// Whether this run failed a validation-acceptance gate (exit code 1, the
    /// CI-gating class): the Exp-1 stall ratio over its bound (V-1), or — when
    /// `--gate` is set — the Exp-2 plateau guard (V-2), the Exp-3 sharding-slope
    /// guard (V-3), or the Exp-5 herd-knee guard (V-4). Off-by-default for the
    /// sweep guards so a smoke run reports the verdict without failing on one
    /// host's modest numbers; the stall gate self-gates on `--max-stall-ratio`.
    fn failed_acceptance(&self) -> bool {
        self.stall.as_ref().is_some_and(|s| s.tripped())
            || self.sweep.as_ref().is_some_and(|s| s.gate_failed())
            || self.shard.as_ref().is_some_and(|s| s.gate_failed())
            || self.herd.as_ref().is_some_and(|h| h.gate_failed())
    }

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
            if let Some(s) = &self.soak {
                // The full-run percentile distribution is the `hist` printed
                // above; this is the time-series trend view it cannot carry.
                s.print_human();
            }
            if let Some(b) = &self.burst {
                // The burst-specific view: the offered/realized rate tracking, the
                // worker count the load drove up, and the per-ramp scaling latency.
                let sp = |q: f64| b.scaling.value_at_quantile(q);
                println!(
                    "burst        peak_rps={:.0}  cycles={}  connections={}",
                    b.peak_rps, b.cycles, b.connections,
                );
                println!(
                    "rate         offered={}  realized={}  tracking={:.3}",
                    b.offered,
                    b.realized,
                    b.rate_tracking(),
                );
                println!("scale-up     peak_warm_instances={}", b.max_warm_instances);
                println!(
                    "scaling µs   p50={}  p90={}  p99={}  max={}  (per-ramp cold start)",
                    sp(0.50),
                    sp(0.90),
                    sp(0.99),
                    b.scaling.max(),
                );
            }
            if let Some(m) = &self.mix_realized {
                // The configured-vs-realized op mix (issue #81): the profile's
                // requested ratios beside the fractions the driver actually drove.
                let total: u64 = m.configured.iter().sum::<u64>().max(1);
                let f = m.fractions();
                let kinds = ["select", "insert", "update", "delete"];
                let cfg = (0..4)
                    .map(|i| {
                        format!(
                            "{}={:.0}%",
                            kinds[i],
                            m.configured[i] as f64 / total as f64 * 100.0
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let got = (0..4)
                    .map(|i| format!("{}={:.1}%", kinds[i], f[i] * 100.0))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("mix want     {cfg}");
                println!("mix got      {got}");
            }
            if let Some(s) = &self.stall {
                // The Exp-1 stall gate (V-1): the p999/p50 ratio + verdict.
                let verdict = match s.max {
                    Some(m) if s.tripped() => format!("STALL (> {m:.0}× bound)"),
                    Some(m) => format!("clean (≤ {m:.0}× bound)"),
                    None => "clean (no bound set)".to_string(),
                };
                println!("stall gate   p999/p50={:.1}×  {verdict}", s.ratio);
            }
            if let Some(s) = &self.sweep {
                s.print_human();
            }
            if let Some(s) = &self.shard {
                s.print_human();
            }
            if let Some(h) = &self.herd {
                h.print_human();
            }
            // Run provenance (V-5): only when something is set (a real-host run).
            if self.archival.region.is_some()
                || self.archival.instance_type.is_some()
                || self.archival.backend_sha.is_some()
            {
                println!(
                    "provenance   region={}  instance={}  backend_sha={}",
                    self.archival.region.as_deref().unwrap_or("-"),
                    self.archival.instance_type.as_deref().unwrap_or("-"),
                    self.archival.backend_sha.as_deref().unwrap_or("-"),
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
        // The soak section (#80): the sampled time-series summary + drift verdict.
        let soak = match &self.soak {
            Some(s) => s.to_json(),
            None => "null".to_string(),
        };
        // The burst section: rate tracking + the per-ramp scaling-latency
        // percentiles. The cold/warm-start counts and peak workers ride the
        // `lifecycle` object above under the settled `twill_*` names.
        let burst = match &self.burst {
            Some(b) => format!(
                "{{\"peak_rps\":{:.1},\"cycles\":{},\"connections\":{},\
                 \"offered\":{},\"realized\":{},\"rate_tracking\":{:.6},\
                 \"peak_warm_instances\":{},\"scaling_p50_us\":{},\"scaling_p90_us\":{},\
                 \"scaling_p99_us\":{},\"scaling_max_us\":{}}}",
                b.peak_rps,
                b.cycles,
                b.connections,
                b.offered,
                b.realized,
                b.rate_tracking(),
                b.max_warm_instances,
                b.scaling.value_at_quantile(0.50),
                b.scaling.value_at_quantile(0.90),
                b.scaling.value_at_quantile(0.99),
                b.scaling.max(),
            ),
            None => "null".to_string(),
        };
        // The custom-profile section (#81): the configured weights and the
        // realized op counts, both as `[select, insert, update, delete]`.
        let mix_realized = match &self.mix_realized {
            Some(m) => format!(
                "{{\"configured\":[{},{},{},{}],\"realized\":[{},{},{},{}]}}",
                m.configured[0],
                m.configured[1],
                m.configured[2],
                m.configured[3],
                m.realized[0],
                m.realized[1],
                m.realized[2],
                m.realized[3],
            ),
            None => "null".to_string(),
        };
        // The validation-campaign sections (issue #91): the Exp-1 stall gate
        // (V-1), the Exp-2 sweep (V-2), the Exp-3 sharding curve (V-3), and the
        // Exp-5 herd curve (V-4). Each is `null` outside its own scenario.
        let stall = match &self.stall {
            Some(s) => format!(
                "{{\"p999_over_p50\":{:.3},\"max\":{},\"tripped\":{}}}",
                s.ratio,
                match s.max {
                    Some(m) => format!("{m:.3}"),
                    None => "null".to_string(),
                },
                s.tripped(),
            ),
            None => "null".to_string(),
        };
        let sweep = self
            .sweep
            .as_ref()
            .map_or("null".to_string(), |s| s.to_json());
        let shard = self
            .shard
            .as_ref()
            .map_or("null".to_string(), |s| s.to_json());
        let herd = self
            .herd
            .as_ref()
            .map_or("null".to_string(), |h| h.to_json());
        println!(
            "{{\"experiment\":\"{}\",\"label\":\"{}\",\"transport\":\"{}\",\"backend\":\"{}\",\
             \"git\":\"{}\",\"writers\":{},\"duration_s\":{:.3},\"commits\":{},\"conflicts\":{},\
             \"failures\":{},\"throughput_per_s\":{:.1},\"p50_us\":{},\"p90_us\":{},\"p95_us\":{},\
             \"p99_us\":{},\"p999_us\":{},\"min_us\":{},\"max_us\":{},\"mean_us\":{:.1},\
             \"correctness\":{},\"lifecycle\":{},\"soak\":{},\"burst\":{},\"mix\":{},\
             \"stall\":{},\"sweep\":{},\"shard\":{},\"herd\":{}{}}}",
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
            soak,
            burst,
            mix_realized,
            stall,
            sweep,
            shard,
            herd,
            self.archival.json_members(),
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
         VALIDATION CAMPAIGN (spec 09 sweeps & gates; issue #91):\n\
         \x20 exp2-sweep      group-commit-window sweep + plateau-knee detection (V-2);\n\
         \x20                 sweeps 1..--sweep-max writers, gates plateau vs the Exp-1 ceiling\n\
         \x20 exp3-shard      N-database sharding sweep (V-3): 1..--databases independent DBs\n\
         \x20                 under one pacer; reports aggregate scaling + cross-DB CAS finding\n\
         \x20 herd            thundering-herd cold starts (V-4): 1..--concurrency simultaneous\n\
         \x20                 cold boots; reports the spin-up saturation knee\n\
         \x20 boundary        build the W1/W2 boundary tables from archived records (V-5):\n\
         \x20                 boundary --dir <DIR> | --record <FILE>… [--out <FILE>] [--gate]\n\
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
         \x20 burst           idle→500→5k→20k rps→idle, repeated: a closed-loop rate driver\n\
         \x20                 swings the controller through load to measure cold/warm starts,\n\
         \x20                 peak workers, and per-ramp scaling latency from pulled snapshots\n\
         \n\
         SOAK SCENARIO (interval-sampled time series; drift/leak verdict → exit 2):\n\
         \x20 long-run        steady load + periodic stats()/resource samples over --duration;\n\
         \x20                 fits a slope on memory/fds/p99 and fails on a leak/drift trend\n\
         \n\
         CUSTOM PROFILE (workload as data; feature-gated YAML loader):\n\
         \x20 custom --profile <FILE>  drive a YAML-described mix (duration, connections, mix,\n\
         \x20                 rows, seed); build with --features custom-profile to enable it\n\
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
         \x20 --cycles <N>         scale-to-zero cold-boot samples / burst cycles (default 20)\n\
         \x20 --idle-ms <MS>       idle window before the reaper scales to zero (default 100)\n\
         \x20 --sample-interval-ms <MS>  long-run time-series sample period (default 1000)\n\
         \x20 --drift-threshold <FRAC>   long-run leak/drift trip fraction (default 0.10)\n\
         \x20 --peak-rps <N>       burst peak offered rate, the 20k tier (default 20000)\n\
         \x20 --ramp-ms <MS>       burst ramp duration between plateaus (default 50)\n\
         \x20 --dwell-ms <MS>      burst hold at each active plateau (default 150)\n\
         \x20 --profile <FILE>     YAML workload profile for `custom` (feature-gated loader)\n\
         \x20 --sweep-max <N>      max writers for exp2-sweep (default 8)\n\
         \x20 --databases <N>      max independent databases for exp3-shard (default 4)\n\
         \x20 --concurrency <N>    max simultaneous cold starts for herd (default 8)\n\
         \x20 --cold-connection    exp1 variant: open a fresh connection per sample\n\
         \x20 --max-stall-ratio <X>  exp1 gate: trip if p999/p50 exceeds X (e.g. 50)\n\
         \x20 --region <TEXT>      object-store region recorded in the archive (V-5)\n\
         \x20 --instance-type <TEXT>  host instance type recorded in the archive (V-5)\n\
         \x20 --gate               map a failed validation-sweep gate to a non-zero exit\n\
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
