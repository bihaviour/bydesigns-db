//! End-to-end tests for the `custom` subcommand and its feature gate (issue
//! #81). These drive the public [`twill_bench::run_cli`] with constructed argv —
//! exactly what the binary does — so they exercise flag parsing, the
//! feature-gated dispatch, the YAML loader, the validation error paths, and the
//! exit-code contract together.
//!
//! The file compiles in *both* feature states; each test is `cfg`-gated to the
//! state it asserts, so the with/without-feature build matrix runs the right
//! half each way and the wall (default build → rebuild hint) is pinned alongside
//! the gated behaviour.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use twill_bench::{exit, run_cli};

fn unique_path(suffix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "twill-bench-cust-{}-{n}{suffix}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p.display().to_string()
}

/// Write `body` to a temp `.yaml` file and return its path.
fn write_profile(body: &str) -> String {
    let path = unique_path(".yaml");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    path
}

fn argv(parts: &[&str]) -> Vec<String> {
    std::iter::once("twill-bench".to_string())
        .chain(parts.iter().map(|s| s.to_string()))
        .collect()
}

// ---- feature OFF: the wall ------------------------------------------------

/// With the loader compiled out, `custom` must report a clear rebuild hint and
/// exit with the config code (3) — never silently no-op or crash. This is the
/// default-build half of the wall (#78 guardrail 1 / #81 C2).
#[cfg(not(feature = "custom-profile"))]
#[test]
fn custom_without_feature_reports_rebuild_hint() {
    let profile = write_profile("duration_ms: 100\nmix:\n  select: 1\n");
    let code = run_cli(&argv(&[
        "custom",
        "--profile",
        &profile,
        "--url",
        "file:///tmp/x.db",
    ]));
    assert_eq!(
        code,
        exit::CONFIG,
        "custom without the feature must be a config error with a rebuild hint"
    );
}

// ---- feature ON: the loader ----------------------------------------------

/// The happy path: a profile that carries its own `url` drives a clean run.
#[cfg(feature = "custom-profile")]
#[test]
fn custom_runs_a_profile_carrying_its_own_url() {
    let db = unique_path(".db");
    let profile = write_profile(&format!(
        "label: smoke\nurl: file://{db}\nduration_ms: 120\nwarmup_ms: 0\nconnections: 2\nrows: 40\nseed: 7\nmix:\n  select: 80\n  insert: 20\n"
    ));
    let code = run_cli(&argv(&["custom", "--profile", &profile, "--json"]));
    assert_eq!(code, exit::OK, "a valid profile must run cleanly");
}

/// A CLI `--url` overrides (or supplies) the backend when the profile omits one.
#[cfg(feature = "custom-profile")]
#[test]
fn custom_url_flag_supplies_the_backend() {
    let db = unique_path(".db");
    let profile = write_profile(
        "duration_ms: 120\nwarmup_ms: 0\nconnections: 2\nrows: 40\nmix:\n  select: 70\n  insert: 20\n  update: 8\n  delete: 2\n",
    );
    let code = run_cli(&argv(&[
        "custom",
        "--profile",
        &profile,
        "--url",
        &format!("file://{db}"),
        "--json",
    ]));
    assert_eq!(code, exit::OK, "the --url flag must supply the backend");
}

/// A profile with neither its own `url` nor a `--url` flag is a config error
/// (#81 C4: missing url).
#[cfg(feature = "custom-profile")]
#[test]
fn custom_missing_url_is_a_config_error() {
    let profile = write_profile("duration_ms: 120\nmix:\n  select: 1\n");
    assert_eq!(
        run_cli(&argv(&["custom", "--profile", &profile, "--json"])),
        exit::CONFIG
    );
}

/// A contradictory profile (all-zero mix) → config error (#81 C4).
#[cfg(feature = "custom-profile")]
#[test]
fn custom_all_zero_mix_is_a_config_error() {
    let profile = write_profile(
        "url: file:///tmp/x.db\nduration_ms: 120\nmix:\n  select: 0\n  insert: 0\n  update: 0\n  delete: 0\n",
    );
    assert_eq!(
        run_cli(&argv(&["custom", "--profile", &profile, "--json"])),
        exit::CONFIG
    );
}

/// A malformed profile (unknown op key) → config error.
#[cfg(feature = "custom-profile")]
#[test]
fn custom_malformed_profile_is_a_config_error() {
    let profile = write_profile("url: file:///tmp/x.db\nduration_ms: 120\nmix:\n  upsert: 5\n");
    assert_eq!(
        run_cli(&argv(&["custom", "--profile", &profile, "--json"])),
        exit::CONFIG
    );
}

/// Omitting `--profile` entirely → config error.
#[cfg(feature = "custom-profile")]
#[test]
fn custom_without_profile_flag_is_a_config_error() {
    assert_eq!(
        run_cli(&argv(&["custom", "--url", "file:///tmp/x.db", "--json"])),
        exit::CONFIG
    );
}

/// An unreadable profile path → config error (not a crash).
#[cfg(feature = "custom-profile")]
#[test]
fn custom_unreadable_profile_is_a_config_error() {
    assert_eq!(
        run_cli(&argv(&[
            "custom",
            "--profile",
            "/no/such/twill-profile.yaml",
            "--url",
            "file:///tmp/x.db",
            "--json",
        ])),
        exit::CONFIG
    );
}
