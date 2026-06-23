//! End-to-end tests for the request-mix scenarios, correctness profiles, the
//! exit-code contract, and `compare` (spec 15). These drive the public
//! [`twill_bench::run_cli`] entry point with constructed argv — exactly what the
//! `twill-bench` binary does — so they exercise flag parsing, dispatch, the
//! workload loops, the post-run ACID assertions, and the exit-code mapping
//! together, all embedded over a temporary `file://` database (no transport
//! tooling required; the pgwire path is pinned separately in `tests/pgwire.rs`).

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use twill_bench::{exit, run_cli};

fn unique_url() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-bench-scn-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("file://{}", p.display())
}

/// Build an argv (`twill-bench <parts…>`) the way the binary receives it.
fn argv(parts: &[&str]) -> Vec<String> {
    std::iter::once("twill-bench".to_string())
        .chain(parts.iter().map(|s| s.to_string()))
        .collect()
}

#[test]
fn request_mix_scenarios_run_clean_over_all_ops() {
    // Each named scenario drives its SELECT/INSERT/UPDATE/DELETE mix to
    // completion (mixed-oltp deletes seeded rows, so a later read can miss — that
    // must remain a successful read, not a run failure).
    for scenario in ["read-heavy", "write-heavy", "mixed-oltp"] {
        let url = unique_url();
        let code = run_cli(&argv(&[
            scenario,
            "--url",
            &url,
            "--writers",
            "2",
            "--warmup-ms",
            "0",
            "--duration-ms",
            "150",
            "--rows",
            "40",
            "--json",
        ]));
        assert_eq!(code, exit::OK, "scenario {scenario} should run cleanly");
    }
}

#[test]
fn counter_profile_passes_when_no_update_is_lost() {
    // The engine retries first-committer-wins conflicts, so every increment must
    // survive: final == writers * ops → exit 0.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "counter",
        "--url",
        &url,
        "--writers",
        "4",
        "--ops",
        "150",
        "--json",
    ]));
    assert_eq!(code, exit::OK, "counter must conserve every increment");
}

#[test]
fn bank_transfer_profile_passes_when_balance_is_conserved() {
    // Concurrent atomic transfers must conserve the summed balance → exit 0.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "bank-transfer",
        "--url",
        &url,
        "--writers",
        "4",
        "--ops",
        "60",
        "--json",
    ]));
    assert_eq!(code, exit::OK, "balance must be conserved across transfers");
}

#[test]
fn missing_url_is_a_config_error() {
    assert_eq!(run_cli(&argv(&["exp1"])), exit::CONFIG);
}

#[test]
fn unknown_subcommand_is_a_config_error() {
    assert_eq!(
        run_cli(&argv(&["nope", "--url", "file:///tmp/x.db"])),
        exit::CONFIG
    );
}

#[test]
fn unknown_flag_is_a_config_error() {
    assert_eq!(
        run_cli(&argv(&[
            "exp1",
            "--url",
            "file:///tmp/x.db",
            "--bogus",
            "1"
        ])),
        exit::CONFIG
    );
}

#[test]
fn unopenable_target_is_a_connection_error() {
    // An unknown URL scheme can't be opened → exit 4 (connection), distinct from
    // a config error (3) or a run failure (1).
    let code = run_cli(&argv(&[
        "counter",
        "--url",
        "bogus://nowhere",
        "--writers",
        "1",
        "--ops",
        "1",
        "--json",
    ]));
    assert_eq!(code, exit::CONNECTION);
}

#[test]
fn help_exits_zero() {
    assert_eq!(run_cli(&argv(&["help"])), exit::OK);
    assert_eq!(run_cli(&argv(&["--help"])), exit::OK);
}

// ---- compare ------------------------------------------------------------

fn write_record(throughput: f64, p99: u64, p999: u64, git: &str) -> String {
    let mut p = std::env::temp_dir();
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    p.push(format!("twill-bench-rec-{}-{n}.json", std::process::id()));
    let line = format!(
        "{{\"experiment\":\"exp2-group-commit\",\"git\":\"{git}\",\
         \"throughput_per_s\":{throughput:.1},\"p50_us\":100,\"p99_us\":{p99},\"p999_us\":{p999}}}"
    );
    let mut f = std::fs::File::create(&p).unwrap();
    // A human line above the record, to prove `compare` finds the JSON line.
    writeln!(f, "── exp2 ─────────").unwrap();
    writeln!(f, "{line}").unwrap();
    p.display().to_string()
}

#[test]
fn compare_passes_on_equivalent_records() {
    let base = write_record(1000.0, 200, 500, "aaaa111");
    let cand = write_record(1010.0, 205, 510, "bbbb222");
    let code = run_cli(&argv(&[
        "compare",
        "--baseline",
        &base,
        "--candidate",
        &cand,
    ]));
    assert_eq!(code, exit::OK);
}

#[test]
fn compare_flags_a_latency_regression() {
    let base = write_record(1000.0, 200, 500, "aaaa111");
    // p99 doubled — well past the default 10% threshold.
    let cand = write_record(1000.0, 400, 500, "bbbb222");
    let code = run_cli(&argv(&[
        "compare",
        "--baseline",
        &base,
        "--candidate",
        &cand,
    ]));
    assert_eq!(code, exit::BENCH_FAILED);
}

#[test]
fn compare_flags_a_throughput_regression() {
    let base = write_record(1000.0, 200, 500, "aaaa111");
    // Throughput halved.
    let cand = write_record(500.0, 200, 500, "bbbb222");
    let code = run_cli(&argv(&[
        "compare",
        "--baseline",
        &base,
        "--candidate",
        &cand,
    ]));
    assert_eq!(code, exit::BENCH_FAILED);
}

#[test]
fn compare_missing_flags_is_a_config_error() {
    let base = write_record(1000.0, 200, 500, "aaaa111");
    assert_eq!(
        run_cli(&argv(&["compare", "--baseline", &base])),
        exit::CONFIG
    );
}
