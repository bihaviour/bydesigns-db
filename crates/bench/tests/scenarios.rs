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
fn inventory_profile_passes_with_no_oversell() {
    // Stock is seeded to exactly writers * ops; every decrement must succeed and
    // the shelf must land at exactly zero (no lost decrement, no negative stock).
    let url = unique_url();
    let code = run_cli(&argv(&[
        "inventory",
        "--url",
        &url,
        "--writers",
        "4",
        "--ops",
        "80",
        "--json",
    ]));
    assert_eq!(code, exit::OK, "inventory must sell each unit exactly once");
}

#[test]
fn document_editing_profile_passes_with_no_lost_edit() {
    // Concurrent client-side read-modify-write edits to one row: snapshot isolation
    // must conflict any colliding commit so no edit is lost → final rev == work.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "document-editing",
        "--url",
        &url,
        "--writers",
        "4",
        "--ops",
        "60",
        "--json",
    ]));
    assert_eq!(code, exit::OK, "every concurrent edit must be preserved");
}

#[test]
fn scale_to_zero_drives_cold_starts_cleanly() {
    // The lifecycle scenario (spec 09 Exp 5 / spec 15): each cycle cold-starts the
    // instance, reads the seeded rows back, then idles past the reaper. A clean
    // exit proves every cold read saw its durable state across the teardown (the
    // scenario fails the run otherwise). Short idle window + few cycles keep it
    // fast while still exercising the full cold path end to end.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "scale-to-zero",
        "--url",
        &url,
        "--rows",
        "20",
        "--cycles",
        "3",
        "--idle-ms",
        "40",
        "--json",
    ]));
    assert_eq!(
        code,
        exit::OK,
        "scale-to-zero must complete with no durable loss across teardowns"
    );
}

#[test]
fn burst_scales_up_and_down_with_no_durable_loss() {
    // The `burst` autoscaling-stress scenario (issue #79 / spec 15): a closed-loop
    // rate driver swings a scaled-down shape (idle→tiers→idle, low peak) at an
    // in-process controller. A clean exit proves the scenario's own gates held —
    // the worker count rose under load (scale-up) and fell back to zero on the
    // idle plateaus (scale-down), and every acked INSERT survived the
    // scale-to-zero teardowns (zero acked-write loss). Small peak/dwell/idle keep
    // it fast while still driving the full up/down/up cycle the issue specifies.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "burst",
        "--url",
        &url,
        "--peak-rps",
        "200",
        "--cycles",
        "2",
        "--writers",
        "4",
        "--ramp-ms",
        "20",
        "--dwell-ms",
        "60",
        "--idle-ms",
        "30",
        "--json",
    ]));
    assert_eq!(
        code,
        exit::OK,
        "burst must scale up and down with no acked-write loss"
    );
}

#[test]
fn burst_rejects_the_pgwire_form() {
    // Like scale-to-zero, burst owns an in-process controller; a deployed server
    // runs its own out of the bench's reach, so the pgwire/server form is a config
    // error (that path is the spec-09 scale form against a real deployment).
    assert_eq!(
        run_cli(&argv(&[
            "burst",
            "--url",
            "file:///tmp/x.db",
            "--transport",
            "pgwire",
        ])),
        exit::CONFIG
    );
}

#[test]
fn scale_to_zero_rejects_the_pgwire_form() {
    // The scenario owns an in-process controller; a deployed server runs its own,
    // out of the bench's reach, so the pgwire/server form is a config error.
    assert_eq!(
        run_cli(&argv(&[
            "scale-to-zero",
            "--url",
            "file:///tmp/x.db",
            "--transport",
            "pgwire",
        ])),
        exit::CONFIG
    );
}

#[test]
fn long_run_flat_control_passes() {
    // A short soak with no seeded growth: the interval sampler captures a series,
    // the trend checker fits a slope over memory/fds/p99, and a steady run trends
    // up on nothing past its floor → clean exit. Short duration + fast sampling
    // keeps it quick while still producing enough samples to fit a line.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "long-run",
        "--url",
        &url,
        "--writers",
        "2",
        "--warmup-ms",
        "20",
        "--duration-ms",
        "600",
        "--sample-interval-ms",
        "20",
        "--json",
    ]));
    assert_eq!(
        code,
        exit::OK,
        "a flat soak control run must not be flagged as a leak/drift"
    );
}

#[test]
fn long_run_seeded_leak_exits_two() {
    // The negative case (#80 L5): the test-only `leak` fault seeds monotonic
    // growth into the sampled series, so the trend checker must detect it and
    // fail the run with the correctness exit code (2) — proving the checker
    // bites, not only that the PASS path works.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "long-run",
        "--url",
        &url,
        "--writers",
        "2",
        "--warmup-ms",
        "20",
        "--duration-ms",
        "600",
        "--sample-interval-ms",
        "20",
        "--inject-fault",
        "leak",
        "--json",
    ]));
    assert_eq!(
        code,
        exit::CORRECTNESS,
        "a seeded leak/drift must fail the soak drift gate"
    );
}

#[test]
fn long_run_rejects_the_pgwire_form() {
    // The soak samples this process's own resources; a deployed server runs out
    // of reach, so the pgwire/server form is a config error (like scale-to-zero).
    assert_eq!(
        run_cli(&argv(&[
            "long-run",
            "--url",
            "file:///tmp/x.db",
            "--transport",
            "pgwire",
        ])),
        exit::CONFIG
    );
}

#[test]
fn seeded_lost_update_violation_exits_two() {
    // The negative case: inject a real lost update (one acked increment dropped)
    // and prove the no-lost-update checker catches it and fails the run with the
    // correctness exit code (2) — not a clean exit, however fast the run was.
    let url = unique_url();
    let code = run_cli(&argv(&[
        "counter",
        "--url",
        &url,
        "--writers",
        "2",
        "--ops",
        "50",
        "--inject-fault",
        "lost-update",
        "--json",
    ]));
    assert_eq!(
        code,
        exit::CORRECTNESS,
        "a seeded lost update must fail the correctness gate"
    );
}

#[test]
fn unknown_fault_kind_is_a_config_error() {
    assert_eq!(
        run_cli(&argv(&[
            "counter",
            "--url",
            "file:///tmp/x.db",
            "--inject-fault",
            "bogus",
        ])),
        exit::CONFIG
    );
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
