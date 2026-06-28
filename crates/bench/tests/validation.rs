//! End-to-end tests for the spec-09 validation campaign surface (issue #91):
//! the Exp-1 stall gate + cold-connection variant (V-1), the Exp-2
//! group-commit-window sweep (V-2), the Exp-3 N-database sharding orchestrator
//! (V-3), the Exp-5 thundering-herd variant (V-4), and the W1/W2 boundary tables
//! (V-5). Like `tests/scenarios.rs` these drive the public [`twill_bench::run_cli`]
//! with a constructed argv — exercising flag parsing, dispatch, the sweep/orchestrator
//! loops, the acceptance gates, and the exit-code mapping together, all embedded
//! over temporary `file://` databases (no transport tooling required).
//!
//! The curve *magnitudes* on a single offline host are modest (group commit
//! barely engages without a network round-trip to amortize, and `file://`
//! contention is cheap), so these tests assert the machinery — that each sweep
//! runs, reports its section, and that the gates fire on an injected fault and
//! stay shut on a clean run — not the numeric verdicts a real-S3 host produces.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use twill_bench::{exit, run_cli};

fn unique_url() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-bench-val-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("file://{}", p.display())
}

fn argv(parts: &[&str]) -> Vec<String> {
    std::iter::once("twill-bench".to_string())
        .chain(parts.iter().map(|s| s.to_string()))
        .collect()
}

#[test]
fn exp1_stall_gate_passes_clean_and_fires_on_injected_stall() {
    let url = unique_url();
    // A clean run stays green: the bound (1000×) sits far above the worst p999/p50
    // a loaded CI host produces from incidental tail jitter, and far below the
    // injected stall (p50×5000), so the two cases never overlap.
    let ok = run_cli(&argv(&[
        "exp1",
        "--url",
        &url,
        "--duration-ms",
        "300",
        "--warmup-ms",
        "80",
        "--max-stall-ratio",
        "1000",
        "--json",
    ]));
    assert_eq!(ok, exit::OK, "clean exp1 should pass the stall gate");

    // The injected stall folds one pathological sample into the distribution, so
    // p999/p50 explodes past the bound → the gate fires (exit 1).
    let url2 = unique_url();
    let tripped = run_cli(&argv(&[
        "exp1",
        "--url",
        &url2,
        "--duration-ms",
        "300",
        "--warmup-ms",
        "80",
        "--max-stall-ratio",
        "1000",
        "--inject-fault",
        "stall",
        "--json",
    ]));
    assert_eq!(
        tripped,
        exit::BENCH_FAILED,
        "an injected stall should trip the ratio gate"
    );
}

#[test]
fn exp1_stall_gate_is_report_only_without_a_bound() {
    // No `--max-stall-ratio`: even the injected stall reports but never gates.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "exp1",
        "--url",
        &url,
        "--duration-ms",
        "300",
        "--warmup-ms",
        "80",
        "--inject-fault",
        "stall",
        "--json",
    ]));
    assert_eq!(code, exit::OK, "no bound set → the gate is report-only");
}

#[test]
fn exp1_cold_connection_variant_runs() {
    let url = unique_url();
    let code = run_cli(&argv(&[
        "exp1",
        "--url",
        &url,
        "--cold-connection",
        "--duration-ms",
        "250",
        "--warmup-ms",
        "60",
        "--json",
    ]));
    assert_eq!(code, exit::OK);
}

#[test]
fn exp2_window_sweep_runs_clean() {
    let url = unique_url();
    let code = run_cli(&argv(&[
        "exp2-sweep",
        "--url",
        &url,
        "--sweep-max",
        "4",
        "--duration-ms",
        "150",
        "--warmup-ms",
        "50",
        "--json",
    ]));
    // The plateau guard is off without `--gate`, so a smoke run is deterministically OK.
    assert_eq!(code, exit::OK);
}

#[test]
fn exp3_sharding_runs_clean_and_rejects_external_server() {
    let url = unique_url();
    let code = run_cli(&argv(&[
        "exp3-shard",
        "--url",
        &url,
        "--databases",
        "4",
        "--writers",
        "2",
        "--duration-ms",
        "150",
        "--warmup-ms",
        "50",
        "--json",
    ]));
    assert_eq!(code, exit::OK);

    // An external single-`--db` server cannot host N independent databases.
    let url2 = unique_url();
    let rejected = run_cli(&argv(&[
        "exp3-shard",
        "--url",
        &url2,
        "--server",
        "127.0.0.1:1",
        "--databases",
        "2",
    ]));
    assert_eq!(rejected, exit::CONFIG, "external --server must be rejected");
}

#[test]
fn herd_runs_clean() {
    let url = unique_url();
    let code = run_cli(&argv(&[
        "herd",
        "--url",
        &url,
        "--concurrency",
        "4",
        "--rows",
        "16",
        "--idle-ms",
        "60",
        "--json",
    ]));
    assert_eq!(code, exit::OK);
}

/// Write a one-line JSON record to a temp file, returning its path.
fn write_record(name: &str, line: &str) -> String {
    let mut p = std::env::temp_dir();
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    p.push(format!(
        "twill-bench-rec-{}-{n}-{name}.json",
        std::process::id()
    ));
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "{line}").unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn boundary_table_builds_and_gates_must_rows() {
    // A clean set of records: every MUST row passes.
    let exp1 = write_record(
        "exp1",
        r#"{"experiment":"exp1-latency-floor","stall":{"p999_over_p50":6.0}}"#,
    );
    let exp2 = write_record(
        "exp2",
        r#"{"experiment":"exp2-window-sweep","sweep":{"plateau":40000.0,"gain":40.0}}"#,
    );
    let exp3 = write_record(
        "exp3",
        r#"{"experiment":"exp3-sharding","shard":{"efficiency":0.92}}"#,
    );
    let exp4 = write_record(
        "exp4",
        r#"{"experiment":"exp4-crash-safety","crash_safety":true}"#,
    );

    // Without --gate the boundary table prints and exits OK regardless.
    let code = run_cli(&argv(&[
        "boundary", "--record", &exp1, "--record", &exp2, "--record", &exp3, "--record", &exp4,
    ]));
    assert_eq!(code, exit::OK);

    // With --gate and all MUST rows met (incl. crash-safety PASS), still OK.
    let gated = run_cli(&argv(&[
        "boundary", "--gate", "--record", &exp1, "--record", &exp2, "--record", &exp3, "--record",
        &exp4,
    ]));
    assert_eq!(gated, exit::OK, "all MUST rows met → gate passes");

    // A pathological Exp-1 ratio makes a MUST row FAIL → --gate fails the run.
    let bad = write_record(
        "exp1bad",
        r#"{"experiment":"exp1-latency-floor","stall":{"p999_over_p50":250.0}}"#,
    );
    let failed = run_cli(&argv(&["boundary", "--gate", "--record", &bad]));
    assert_eq!(
        failed,
        exit::BENCH_FAILED,
        "a failing MUST row trips the boundary gate"
    );
}

#[test]
fn boundary_requires_records() {
    let code = run_cli(&argv(&["boundary"]));
    assert_eq!(code, exit::CONFIG);
}
