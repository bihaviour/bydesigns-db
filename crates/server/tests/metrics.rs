//! Ops-metrics exporter tests: the process-global counters render in Prometheus
//! text format, statement counters move, and the `/metrics` + `/healthz` HTTP
//! surface answers over a real socket against a live engine.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use twill_server::metrics;

fn unique_db() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-metrics-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("file://{}", p.display())
}

/// `render` always emits the wire-level counters, in valid exposition format,
/// and reaches the engine for the live gauges when the URL opens.
#[test]
fn render_includes_wire_and_engine_metrics() {
    let db = unique_db();
    let out = metrics::render(None, &db);

    for name in [
        "twilldb_uptime_seconds",
        "twilldb_connections_total",
        "twilldb_connections_active",
        "twilldb_queries_total",
        "twilldb_query_errors_total",
        "twilldb_rows_returned_total",
        // engine/storage gauges — present because file:// opens cleanly
        "twilldb_engine_committed_lsn",
        "twilldb_engine_commits_total",
        "twilldb_storage_wal_appends_total",
    ] {
        assert!(out.contains(name), "missing metric {name} in:\n{out}");
        assert!(
            out.contains(&format!("# TYPE {name} ")),
            "missing TYPE line for {name}"
        );
    }
    // A bad URL must not panic the scrape — it degrades to a comment.
    let bad = metrics::render(None, "file:///nonexistent/dir/which/should/not/open.db");
    assert!(bad.contains("twilldb_connections_total"));
}

/// `query_ok`/`query_err` move the totals monotonically.
#[test]
fn statement_counters_advance() {
    let db = unique_db();
    let total_before = read_counter(&metrics::render(None, &db), "twilldb_queries_total");
    let err_before = read_counter(&metrics::render(None, &db), "twilldb_query_errors_total");

    metrics::query_ok(3);
    metrics::query_err();

    let total_after = read_counter(&metrics::render(None, &db), "twilldb_queries_total");
    let err_after = read_counter(&metrics::render(None, &db), "twilldb_query_errors_total");

    assert!(
        total_after >= total_before + 2,
        "queries_total did not advance"
    );
    assert!(err_after > err_before, "query_errors_total did not advance");
}

/// The HTTP surface answers `/metrics`, `/healthz`, and 404s everything else.
#[test]
fn http_endpoint_serves_metrics_and_health() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let db = unique_db();
    thread::spawn(move || {
        let _ = metrics::serve_listener(listener, &db);
    });

    let (status, body) = http_get(&addr, "/metrics");
    assert!(status.contains("200"), "metrics status: {status}");
    assert!(body.contains("twilldb_connections_total"), "body: {body}");

    let (status, body) = http_get(&addr, "/healthz");
    assert!(status.contains("200"), "health status: {status}");
    assert!(body.contains("ok"));

    let (status, _) = http_get(&addr, "/nope");
    assert!(status.contains("404"), "expected 404, got: {status}");
}

/// The warm-connection design shares one `Connection` across scrape threads, so
/// it must stay `Send` (the exporter's doc comment asserts this).
#[test]
fn connection_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<engine::Connection>();
}

// ---- helpers ---------------------------------------------------------------

fn read_counter(exposition: &str, name: &str) -> u64 {
    for line in exposition.lines() {
        if let Some(rest) = line.strip_prefix(name) {
            if let Some(v) = rest.split_whitespace().next() {
                if let Ok(n) = v.parse::<u64>() {
                    return n;
                }
            }
        }
    }
    panic!("counter {name} not found");
}

fn http_get(addr: &str, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    let status = resp.lines().next().unwrap_or("").to_string();
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}
