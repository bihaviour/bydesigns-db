//! Operator-facing ops metrics for `engine-server` (spec 07) — opt-in, no phone-home.
//!
//! A hand-rolled Prometheus text-exposition endpoint (`GET /metrics`) plus a
//! `GET /healthz` liveness probe, served on a **separate** address from the
//! Postgres-wire listener and only when `--metrics HOST:PORT` is passed. It is
//! *self-hosted*: the server never sends anything outbound — an operator (or a
//! Prometheus scraper) pulls from it, so there is nothing to opt out of and no
//! secret to leak (consistent with `.claude/rules/security.md`).
//!
//! Two sources feed the exposition:
//!
//! * **Wire-level counters** live in process-global atomics, bumped on the hot
//!   path ([`ConnGuard`] for connections, [`query_ok`]/[`query_err`] for
//!   statements). They cost a relaxed atomic add and need no engine coupling.
//! * **Engine / storage gauges** are read live from a short-lived
//!   [`engine::Connection`] per scrape — exactly like a client connecting. For
//!   `file://` this joins the process-global `Database` registry (no re-open,
//!   no extra lease); for object stores a scrape is one cheap catalog open, so
//!   scrape at a sane interval (15s+).
//!
//! No new dependency: the HTTP framing is a dozen lines of `std::net`, in the
//! same hand-rolled spirit as the project's WAL codec and base64.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use engine::Connection;

// ---- process-global wire-level counters -------------------------------------

static CONNECTIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static CONNECTIONS_ACTIVE: AtomicI64 = AtomicI64::new(0);
static QUERIES_TOTAL: AtomicU64 = AtomicU64::new(0);
static QUERY_ERRORS_TOTAL: AtomicU64 = AtomicU64::new(0);
static ROWS_RETURNED_TOTAL: AtomicU64 = AtomicU64::new(0);
static START: OnceLock<Instant> = OnceLock::new();

/// RAII guard for the connection counters: constructed when a client connection
/// is accepted, decrements the active gauge on drop (panic-safe, so an aborted
/// connection never strands the gauge).
pub struct ConnGuard;

impl ConnGuard {
    pub fn new() -> Self {
        CONNECTIONS_TOTAL.fetch_add(1, Ordering::Relaxed);
        CONNECTIONS_ACTIVE.fetch_add(1, Ordering::Relaxed);
        ConnGuard
    }
}

impl Default for ConnGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        CONNECTIONS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Record a statement that completed successfully, returning `rows` result rows.
pub fn query_ok(rows: usize) {
    QUERIES_TOTAL.fetch_add(1, Ordering::Relaxed);
    ROWS_RETURNED_TOTAL.fetch_add(rows as u64, Ordering::Relaxed);
}

/// Record a statement that ended in an error.
pub fn query_err() {
    QUERIES_TOTAL.fetch_add(1, Ordering::Relaxed);
    QUERY_ERRORS_TOTAL.fetch_add(1, Ordering::Relaxed);
}

// ---- exposition -------------------------------------------------------------

fn counter(s: &mut String, name: &str, help: &str, v: u64) {
    s.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n"
    ));
}

fn gauge(s: &mut String, name: &str, help: &str, v: i64) {
    s.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n"
    ));
}

/// Render the full Prometheus exposition. Wire counters are always present;
/// engine/storage gauges are read from `conn` when one is supplied (the warm
/// connection the exporter holds open), otherwise from a throwaway open of
/// `db_url` — best-effort, so a scrape never fails.
///
/// Prefer passing the persistent `conn`: the engine/storage stats are
/// **per-`Database`-instance** runtime counters, so a throwaway open reads ~0
/// for them (only the replayed `committed_lsn` survives). The exporter's warm
/// connection shares the same registry `Database` as the client connections, so
/// its counters reflect true server-wide activity.
pub fn render(conn: Option<&Connection>, db_url: &str) -> String {
    let mut s = String::with_capacity(4096);

    let uptime = START.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);
    gauge(
        &mut s,
        "twilldb_uptime_seconds",
        "Seconds since the metrics exporter started.",
        uptime as i64,
    );
    counter(
        &mut s,
        "twilldb_connections_total",
        "Client connections accepted.",
        CONNECTIONS_TOTAL.load(Ordering::Relaxed),
    );
    gauge(
        &mut s,
        "twilldb_connections_active",
        "Currently open client connections.",
        CONNECTIONS_ACTIVE.load(Ordering::Relaxed),
    );
    counter(
        &mut s,
        "twilldb_queries_total",
        "SQL statements executed (ok + error).",
        QUERIES_TOTAL.load(Ordering::Relaxed),
    );
    counter(
        &mut s,
        "twilldb_query_errors_total",
        "SQL statements that returned an error.",
        QUERY_ERRORS_TOTAL.load(Ordering::Relaxed),
    );
    counter(
        &mut s,
        "twilldb_rows_returned_total",
        "Result rows returned to clients.",
        ROWS_RETURNED_TOTAL.load(Ordering::Relaxed),
    );

    // Use the warm connection if given; otherwise open a throwaway one.
    let owned;
    let stats_conn = match conn {
        Some(c) => c,
        None => match Connection::open(db_url) {
            Ok(c) => {
                owned = c;
                &owned
            }
            Err(e) => {
                s.push_str(&format!("# engine stats unavailable: {e}\n"));
                return s;
            }
        },
    };

    {
        {
            let st = stats_conn.stats();
            counter(
                &mut s,
                "twilldb_engine_commits_total",
                "Transactions committed durably.",
                st.commits,
            );
            counter(
                &mut s,
                "twilldb_engine_durable_appends_total",
                "Durable WAL append batches (commits>appends proves group-commit coalescing).",
                st.durable_appends,
            );
            gauge(
                &mut s,
                "twilldb_engine_committed_lsn",
                "Highest committed (visible) LSN.",
                st.committed_lsn as i64,
            );
            counter(
                &mut s,
                "twilldb_engine_write_acquires_total",
                "Write-lane acquisitions (~write transactions).",
                st.write_acquires,
            );
            counter(
                &mut s,
                "twilldb_engine_write_handoffs_total",
                "Write acquisitions that waited on another writer (serialized-handoff count).",
                st.write_handoffs,
            );
            counter(
                &mut s,
                "twilldb_engine_write_wait_us_total",
                "Cumulative microseconds writers spent blocked on the write lane.",
                st.write_wait_us_total,
            );
            counter(
                &mut s,
                "twilldb_storage_wal_appends_total",
                "Durable WAL appends performed at the storage seam.",
                st.storage.wal_appends,
            );
            counter(
                &mut s,
                "twilldb_storage_wal_bytes_total",
                "Total encoded WAL bytes appended.",
                st.storage.wal_bytes,
            );
            counter(
                &mut s,
                "twilldb_storage_page_reads_total",
                "Page versions read through the seam.",
                st.storage.page_reads,
            );
            counter(
                &mut s,
                "twilldb_storage_cache_hits_total",
                "Page reads served from a warm cache.",
                st.storage.cache_hits,
            );
            counter(
                &mut s,
                "twilldb_storage_cache_misses_total",
                "Page reads that fetched from the backend.",
                st.storage.cache_misses,
            );
            counter(
                &mut s,
                "twilldb_storage_fetch_latency_us_total",
                "Cumulative backend-fetch latency, microseconds (object stores).",
                st.storage.fetch_latency_us_total,
            );
            counter(
                &mut s,
                "twilldb_storage_fsyncs_total",
                "fsync/durable-flush operations performed.",
                st.storage.fsyncs,
            );
        }
    }
    s
}

// ---- minimal HTTP/1.1 exposer ----------------------------------------------

/// Bind `addr` and serve `/metrics` + `/healthz` until the listener errors.
/// Blocks the calling thread; the binary runs it on a dedicated thread.
pub fn serve(addr: &str, db_url: &str) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    serve_listener(listener, db_url)
}

/// Serve an already-bound listener (lets tests bind an ephemeral port first).
///
/// One thread per scrape (so a slow/stalled client can't block others), sharing
/// a single warm [`Connection`] behind a mutex. The warm connection is reused
/// across scrapes so the engine/storage gauges reflect the shared registry
/// `Database`'s server-wide counters (a throwaway open per scrape would read
/// cold per-instance counters); the mutex is held only for the brief stats read,
/// not the socket I/O. (`Connection` is `Send`, gated by `tests/metrics.rs`.)
pub fn serve_listener(listener: TcpListener, db_url: &str) -> io::Result<()> {
    START.get_or_init(Instant::now);
    let db_url: Arc<str> = Arc::from(db_url);
    // Open the warm connection up front; this also keeps the `Database` resident
    // so client connections share it. Re-opened lazily per scrape if it's missing.
    let warm: Arc<Mutex<Option<Connection>>> = Arc::new(Mutex::new(Connection::open(&db_url).ok()));
    for incoming in listener.incoming() {
        let Ok(mut stream) = incoming else { continue };
        let db_url = db_url.clone();
        let warm = warm.clone();
        std::thread::spawn(move || {
            let _ = handle_http(&mut stream, &warm, &db_url);
        });
    }
    Ok(())
}

fn handle_http(
    stream: &mut TcpStream,
    warm: &Mutex<Option<Connection>>,
    db_url: &str,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    // The request line is all we route on; one read is plenty for `GET /path`.
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let path = head.split_whitespace().nth(1).unwrap_or("/");

    let (status, ctype, body) = match path {
        "/metrics" => {
            // Lock only for the stats read; reuse the warm connection, re-opening
            // it if a prior open failed. A poisoned lock still yields the value.
            let mut guard = warm.lock().unwrap_or_else(|p| p.into_inner());
            if guard.is_none() {
                *guard = Connection::open(db_url).ok();
            }
            let body = render(guard.as_ref(), db_url);
            ("200 OK", "text/plain; version=0.0.4; charset=utf-8", body)
        }
        "/healthz" | "/health" => ("200 OK", "text/plain; charset=utf-8", "ok\n".to_string()),
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}
