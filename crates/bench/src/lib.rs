//! `twill-bench` — the benchmark driver for the validation plan (spec 09; issue
//! #6 / #29). It reports latency as **percentiles** (p50/p99/p999 via an
//! HDR-style [`Histogram`]), never mean-only, exactly as spec 09 mandates.
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
//! architecture). Subcommands map to the plan's experiments:
//!   * `exp1` — single-commit latency floor (one sequential writer).
//!   * `exp2` — group-commit throughput curve (N independent-row writers).
//!   * `exp3` — write-contention wall (N writers hammering the same row).

pub mod hist;
pub mod pgclient;

use engine::{Connection, EngineStatus};
use hist::Histogram;
use pgclient::{ExecError, PgClient};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TABLE_LEDGER: &str = "bench_ledger";
const TABLE_COUNTER: &str = "bench_counter";

/// CLI entry point (the `twill-bench` binary is a thin shim over this).
pub fn cli_main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let opts = match Opts::parse(&args[2.min(args.len())..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            std::process::exit(2);
        }
    };

    let result = match cmd {
        "exp1" => run_experiment(Experiment::LatencyFloor, &opts),
        "exp2" => run_experiment(Experiment::GroupCommit, &opts),
        "exp3" => run_experiment(Experiment::Contention, &opts),
        "help" | "-h" | "--help" => {
            print_help();
            return;
        }
        other => {
            eprintln!("error: unknown subcommand '{other}'\n");
            print_help();
            std::process::exit(2);
        }
    };

    match result {
        Ok(report) => report.print(),
        Err(e) => {
            eprintln!("benchmark failed: {e}");
            std::process::exit(1);
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

struct Opts {
    url: String,
    writers: usize,
    warmup: Duration,
    duration: Duration,
    label: String,
    transport: Transport,
    /// `Some(addr)` drives an already-running `engine-server` over pgwire;
    /// `None` (with `--transport pgwire`) spins one up in-process.
    server: Option<String>,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut url = None;
        let mut writers = 1usize;
        let mut warmup_ms = 200u64;
        let mut duration_ms = 1000u64;
        let mut label = String::new();
        let mut transport = Transport::Embedded;
        let mut server = None;

        let mut i = 0;
        while i < args.len() {
            let key = args[i].as_str();
            let val = || {
                args.get(i + 1)
                    .cloned()
                    .ok_or_else(|| format!("missing value for {key}"))
            };
            match key {
                "--url" => url = Some(val()?),
                "--writers" => writers = val()?.parse().map_err(|_| "invalid --writers")?,
                "--warmup-ms" => warmup_ms = val()?.parse().map_err(|_| "invalid --warmup-ms")?,
                "--duration-ms" => {
                    duration_ms = val()?.parse().map_err(|_| "invalid --duration-ms")?
                }
                "--label" => label = val()?,
                "--transport" => transport = parse_transport(&val()?)?,
                "--server" => server = Some(val()?),
                other => return Err(format!("unknown flag {other}")),
            }
            i += 2;
        }

        // `--server` implies the pgwire transport (it has no meaning embedded).
        if server.is_some() {
            transport = Transport::Pgwire;
        }

        Ok(Opts {
            url: url.ok_or("--url is required (e.g. file:///tmp/bench.db or s3://bucket/db)")?,
            writers: writers.max(1),
            warmup: Duration::from_millis(warmup_ms),
            duration: Duration::from_millis(duration_ms),
            label,
            transport,
            server,
        })
    }
}

/// What a writer connects to, cloneable so each writer thread opens its own.
#[derive(Clone)]
enum Target {
    /// Drive the engine in-process at this URL.
    Embedded(String),
    /// Drive the engine through pgwire at this `host:port`.
    Pgwire(String),
}

impl Target {
    fn open(&self) -> Result<Writer, String> {
        match self {
            Target::Embedded(url) => Connection::open(url)
                .map(Writer::Embedded)
                .map_err(|e| format!("open {url}: {e}")),
            Target::Pgwire(addr) => PgClient::connect(addr)
                .map(Writer::Pg)
                .map_err(|e| format!("connect {addr}: {e}")),
        }
    }
}

/// A transport-agnostic writer: an embedded connection or a pgwire client.
enum Writer {
    Embedded(Connection),
    Pg(PgClient),
}

/// The classification a writer loop reacts to, identical across transports.
enum Outcome {
    Ok,
    Conflict,
    Fatal(String),
}

impl Writer {
    fn exec(&mut self, sql: &str) -> Outcome {
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
}

/// A writer's tally from one run window.
struct Tally {
    conflicts: u64,
    hist: Histogram,
}

/// Resolve the run target, spinning up an in-process pgwire server if needed.
/// The returned `_server` (when present) keeps the listener thread alive for the
/// run's duration — dropping the database it serves only on return.
fn resolve_target(opts: &Opts) -> Result<Target, String> {
    match opts.transport {
        Transport::Embedded => Ok(Target::Embedded(opts.url.clone())),
        Transport::Pgwire => match &opts.server {
            Some(addr) => Ok(Target::Pgwire(addr.clone())),
            None => {
                // Spin up an in-process listener on an ephemeral port serving the
                // same `--url` backend; the bind happens here so a client can
                // connect immediately, the accept loop runs on a detached thread.
                let listener = TcpListener::bind("127.0.0.1:0")
                    .map_err(|e| format!("bind in-process server: {e}"))?;
                let addr = listener
                    .local_addr()
                    .map_err(|e| format!("server addr: {e}"))?
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

fn run_experiment(exp: Experiment, opts: &Opts) -> Result<Report, String> {
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

    // A per-run nonce keeps inserted ledger keys unique across repeated runs on
    // the same durable database (PRIMARY KEY would otherwise collide).
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);

    let (tallies, elapsed) = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let target = target.clone();
                let warmup = opts.warmup;
                let duration = opts.duration;
                scope.spawn(move || writer_loop(&target, w, nonce, same_row, warmup, duration))
            })
            .collect();
        let start = Instant::now();
        let tallies: Vec<Result<Tally, String>> =
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
        url_scheme: opts.url.split("://").next().unwrap_or("?").to_string(),
        writers,
        duration_s: elapsed.as_secs_f64(),
        commits,
        conflicts,
        throughput: commits as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE),
        hist: merged,
        git_sha: git_sha(),
    })
}

fn setup_schema(w: &mut Writer, same_row: bool) -> Result<(), String> {
    match w.exec(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE_LEDGER} (k TEXT PRIMARY KEY, v INTEGER)"
    )) {
        Outcome::Ok => {}
        Outcome::Conflict => {}
        Outcome::Fatal(m) => return Err(format!("create ledger: {m}")),
    }
    match w.exec(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE_COUNTER} (id INTEGER PRIMARY KEY, n INTEGER)"
    )) {
        Outcome::Ok => {}
        Outcome::Conflict => {}
        Outcome::Fatal(m) => return Err(format!("create counter: {m}")),
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
    nonce: u128,
    same_row: bool,
    warmup: Duration,
    duration: Duration,
) -> Result<Tally, String> {
    let mut conn = target.open()?;
    let seq = AtomicU64::new(0);

    let one = |conn: &mut Writer,
               hist: Option<&mut Histogram>,
               conflicts: &mut u64|
     -> Result<(), String> {
        let t0 = Instant::now();
        loop {
            let sql = if same_row {
                format!("UPDATE {TABLE_COUNTER} SET n = n + 1 WHERE id = 1")
            } else {
                let i = seq.fetch_add(1, Ordering::Relaxed);
                format!("INSERT INTO {TABLE_LEDGER} (k, v) VALUES ('{nonce}-{writer}-{i}', 1)")
            };
            match conn.exec(&sql) {
                Outcome::Ok => break,
                Outcome::Conflict => {
                    *conflicts += 1;
                    continue;
                }
                Outcome::Fatal(m) => return Err(format!("writer {writer} commit failed: {m}")),
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

struct Report {
    experiment: &'static str,
    label: String,
    transport: &'static str,
    url_scheme: String,
    writers: usize,
    duration_s: f64,
    commits: u64,
    conflicts: u64,
    throughput: f64,
    hist: Histogram,
    git_sha: String,
}

impl Report {
    fn print(&self) {
        let p = |q: f64| self.hist.value_at_quantile(q);
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
        println!("commits      {}", self.commits);
        println!("conflicts    {} (retried)", self.conflicts);
        println!("throughput   {:.0} commits/s", self.throughput);
        println!(
            "latency µs   p50={}  p99={}  p999={}  min={}  max={}  mean={:.1}",
            p(0.50),
            p(0.99),
            p(0.999),
            self.hist.min(),
            self.hist.max(),
            self.hist.mean(),
        );
        // Machine-readable record (one JSON line) for archiving / plotting.
        println!(
            "{{\"experiment\":\"{}\",\"label\":\"{}\",\"transport\":\"{}\",\"backend\":\"{}\",\
             \"git\":\"{}\",\"writers\":{},\"duration_s\":{:.3},\"commits\":{},\"conflicts\":{},\
             \"throughput_per_s\":{:.1},\"p50_us\":{},\"p99_us\":{},\"p999_us\":{},\
             \"min_us\":{},\"max_us\":{},\"mean_us\":{:.1}}}",
            self.experiment,
            self.label,
            self.transport,
            self.url_scheme,
            self.git_sha,
            self.writers,
            self.duration_s,
            self.commits,
            self.conflicts,
            self.throughput,
            p(0.50),
            p(0.99),
            p(0.999),
            self.hist.min(),
            self.hist.max(),
            self.hist.mean(),
        );
    }
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
        "twill-bench — benchmark driver (spec 09 / issue #29)\n\
         \n\
         USAGE:\n\
         \x20 twill-bench <exp1|exp2|exp3> --url <URL> [flags]\n\
         \n\
         EXPERIMENTS:\n\
         \x20 exp1   single-commit latency floor (one sequential writer)\n\
         \x20 exp2   group-commit throughput curve (N independent-row writers)\n\
         \x20 exp3   write-contention wall (N writers on the same row)\n\
         \n\
         FLAGS:\n\
         \x20 --url <URL>          file:///path or s3://bucket/db (required)\n\
         \x20 --transport <T>      embedded (default) or pgwire (server path)\n\
         \x20 --server <HOST:PORT> drive a running engine-server (implies pgwire)\n\
         \x20 --writers <N>        concurrent writers for exp2/exp3 (default 1)\n\
         \x20 --warmup-ms <MS>     discarded warm-up window (default 200)\n\
         \x20 --duration-ms <MS>   timed window (default 1000)\n\
         \x20 --label <TEXT>       free-form tag recorded in the output\n\
         \n\
         Reports p50/p99/p999 (never mean-only) plus a JSON line for archiving.\n\
         `--transport pgwire` without `--server` spins up an in-process listener,\n\
         so the wire path runs offline; point `--server` at a deployed\n\
         engine-server (or reproduce with pgbench) for real-host numbers.\n\
         Use file:// for a smoke run; an object-store URL on a real host for the\n\
         W1 tail numbers that gate placement."
    );
}
