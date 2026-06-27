//! The W1/W2 boundary tables (issue #91 V-5; spec 09 "Reporting & boundary
//! tables" + the decision rule). The validation campaign's whole purpose is to
//! measure the boundary *once* and then route each tool by reading it off a
//! table rather than re-measuring. This post-processor turns a directory of
//! archived run records (the one-line JSON the driver emits for Exp-1, the
//! Exp-2 sweep, the Exp-3 shard sweep, the Exp-5 herd, and the Exp-4
//! crash-safety gate) into that table: one row per spec-09 acceptance metric,
//! its target, the observed value, and a PASS/FAIL/pending verdict.
//!
//! Like `compare`, it is pure post-processing over records the driver already
//! wrote — no engine, no transport, no extra dependency (it reuses `compare`'s
//! tiny flat-JSON scanner). With `--gate` a failed or missing MUST row exits
//! non-zero, so a scheduled CI job can hold the boundary tables to their
//! acceptance criteria.

use crate::compare::{field_num, field_str, last_json_object};
use crate::exit;

/// One row of the boundary table: a spec-09 acceptance metric, its target text,
/// the observed value (or `pending` when no record supplied it), and a verdict.
struct Row {
    axis: &'static str,
    metric: &'static str,
    target: &'static str,
    observed: String,
    /// `Some(true)` PASS, `Some(false)` FAIL, `None` pending (no record).
    pass: Option<bool>,
    /// A spec-09 MUST row — gating, when `--gate` is set, on PASS.
    must: bool,
}

impl Row {
    fn verdict(&self) -> &'static str {
        match self.pass {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "pending",
        }
    }
}

/// Parse `boundary`'s flags and emit the table, returning a process exit code.
/// Records are taken from any number of `--record <FILE>` flags and/or every
/// file under `--dir <DIR>`. `--out <FILE>` also writes the Markdown table to a
/// file; `--gate` fails the run when a MUST row is FAIL or pending.
pub fn run(args: &[String]) -> i32 {
    let mut records: Vec<String> = Vec::new();
    let mut dir: Option<String> = None;
    let mut out: Option<String> = None;
    let mut gate = false;

    let mut i = 0;
    while i < args.len() {
        let key = args[i].as_str();
        if key == "--gate" {
            gate = true;
            i += 1;
            continue;
        }
        let Some(val) = args.get(i + 1).cloned() else {
            eprintln!("error: missing value for {key}");
            return exit::CONFIG;
        };
        match key {
            "--record" => records.push(val),
            "--dir" => dir = Some(val),
            "--out" => out = Some(val),
            other => {
                eprintln!("error: unknown flag {other} for boundary");
                return exit::CONFIG;
            }
        }
        i += 2;
    }

    if let Some(d) = &dir {
        match std::fs::read_dir(d) {
            Ok(entries) => {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_file() {
                        records.push(p.to_string_lossy().into_owned());
                    }
                }
            }
            Err(e) => {
                eprintln!("error: --dir {d}: {e}");
                return exit::CONFIG;
            }
        }
    }

    if records.is_empty() {
        eprintln!("error: boundary needs --record <FILE> (repeatable) and/or --dir <DIR>");
        return exit::CONFIG;
    }

    // Load every record's last JSON object, keyed by experiment, skipping files
    // that are not records (so a directory of mixed artifacts is fine).
    let mut by_experiment: Vec<(String, String)> = Vec::new();
    for path in &records {
        if let Ok(line) = last_json_object(path) {
            if let Some(exp) = field_str(&line, "experiment") {
                by_experiment.push((exp, line));
            }
        }
    }

    let find = |needle: &str| -> Option<String> {
        by_experiment
            .iter()
            .find(|(exp, _)| exp.contains(needle))
            .map(|(_, line)| line.clone())
    };

    let rows = build_rows(&find);
    let table = render(&rows);
    print!("{table}");

    if let Some(path) = &out {
        if let Err(e) = std::fs::write(path, &table) {
            eprintln!("error: --out {path}: {e}");
            return exit::CONFIG;
        }
    }

    // Gate: a MUST row that is FAIL or still pending fails the campaign.
    if gate {
        let unmet: Vec<&Row> = rows
            .iter()
            .filter(|r| r.must && r.pass != Some(true))
            .collect();
        if !unmet.is_empty() {
            eprintln!(
                "boundary gate: {} MUST row(s) unmet ({})",
                unmet.len(),
                unmet
                    .iter()
                    .map(|r| r.metric)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            return exit::BENCH_FAILED;
        }
    }
    exit::OK
}

/// Build the boundary rows from whatever records are present. A `find(needle)`
/// returns the record line whose `experiment` contains `needle`, or `None`.
fn build_rows(find: &dyn Fn(&str) -> Option<String>) -> Vec<Row> {
    let mut rows = Vec::new();

    // ── W1 axis: latency floor (Exp 1) ──────────────────────────────────────
    let exp1 = find("latency-floor");
    let stall_ratio = exp1.as_deref().and_then(|l| field_num(l, "p999_over_p50"));
    rows.push(Row {
        axis: "W1",
        metric: "Exp 1 — latency floor",
        target: "p999/p50 a small multiple (no stalls)",
        observed: stall_ratio
            .map(|r| format!("{r:.1}×"))
            .unwrap_or_else(|| "pending".into()),
        // No fixed number in spec 09 (it is per-deployment); treat a ratio under
        // ~50× as clean, the suggested commit-path-stall threshold.
        pass: stall_ratio.map(|r| r <= 50.0),
        must: true,
    });

    // ── W1 axis: group-commit plateau (Exp 2) ───────────────────────────────
    let exp2 = find("window-sweep").or_else(|| find("group-commit"));
    let plateau = exp2.as_deref().and_then(|l| field_num(l, "plateau"));
    let gain = exp2.as_deref().and_then(|l| field_num(l, "gain"));
    rows.push(Row {
        axis: "W1",
        metric: "Exp 2 — group-commit plateau",
        target: "commits/sec (the W1 ceiling lift)",
        observed: plateau
            .map(|p| format!("{p:.0}/s"))
            .unwrap_or_else(|| "pending".into()),
        pass: plateau.map(|p| p > 0.0),
        must: false,
    });
    rows.push(Row {
        axis: "W1",
        metric: "Exp 2 — plateau / Exp-1 ceiling",
        target: "10–100× (batching works)",
        observed: gain
            .map(|g| format!("{g:.1}×"))
            .unwrap_or_else(|| "pending".into()),
        // spec 09 red flag: plateau ≤ 1.5× the ceiling means no coalescing.
        pass: gain.map(|g| g > 1.5),
        must: true,
    });

    // ── W2 axis: sharding (Exp 3) ────────────────────────────────────────────
    let exp3 = find("sharding").or_else(|| find("contention"));
    let efficiency = exp3.as_deref().and_then(|l| field_num(l, "efficiency"));
    rows.push(Row {
        axis: "W2",
        metric: "Exp 3 — sharding N-DB scaling",
        target: "~linear in N (efficiency ≥ 0.80)",
        observed: efficiency
            .map(|e| format!("{e:.2}"))
            .unwrap_or_else(|| "pending".into()),
        pass: efficiency.map(|e| e >= 0.8),
        must: true,
    });

    // ── Exp 5: thundering-herd knee ──────────────────────────────────────────
    let exp5 = find("herd");
    let knee = exp5
        .as_deref()
        .and_then(|l| field_num(l, "knee_concurrency"));
    rows.push(Row {
        axis: "Exp5",
        metric: "Exp 5 — herd cold-start knee",
        target: "≥ expected peak fan-out (no early saturation)",
        observed: knee
            .map(|k| {
                if k > 0.0 {
                    format!("knee@{k:.0}")
                } else {
                    "no knee".into()
                }
            })
            .unwrap_or_else(|| "pending".into()),
        // SHOULD, not MUST: a present record with a degraded flag is informative.
        pass: exp5
            .as_deref()
            .map(|l| field_str(l, "degraded").as_deref() != Some("true")),
        must: false,
    });

    // ── Exp 4: crash-safety durability gate ──────────────────────────────────
    // The crash-safety proof is a CI gate in `crates/storage/tests/crash_safety.rs`
    // (the Experiment-4 seed sweep); the campaign records its verdict so the
    // durability proof travels with the numbers. A record carrying
    // `"crash_safety":true` (stamped by the CI job) lands the PASS.
    // `"crash_safety"` is a JSON boolean (no quotes), so read the literal rather
    // than a quoted string: `true` → PASS, `false` → FAIL, absent → pending.
    let crash = find("crash-safety").map(|l| l.contains("\"crash_safety\":true"));
    rows.push(Row {
        axis: "Exp4",
        metric: "Exp 4 — crash-safety durability",
        target: "zero acked-write loss across the seed sweep",
        observed: match crash {
            Some(true) => "PASS".into(),
            Some(false) => "FAIL".into(),
            None => "pending (see crash_safety CI gate)".into(),
        },
        pass: crash,
        must: true,
    });

    rows
}

/// Render the rows as a GitHub-flavoured Markdown table (the spec-09 boundary
/// table form, reviewable in a PR or the docs).
fn render(rows: &[Row]) -> String {
    let mut s = String::new();
    s.push_str("## Spec-09 W1/W2 boundary table\n\n");
    s.push_str("| Axis | Metric | Target | Observed | Verdict |\n");
    s.push_str("|------|--------|--------|----------|--------|\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {}{} | {} | {} | {} |\n",
            r.axis,
            r.metric,
            if r.must { " *(MUST)*" } else { "" },
            r.target,
            r.observed,
            r.verdict(),
        ));
    }
    let met = rows.iter().filter(|r| r.pass == Some(true)).count();
    let pending = rows.iter().filter(|r| r.pass.is_none()).count();
    s.push_str(&format!(
        "\n_{met}/{} rows PASS, {pending} pending._\n",
        rows.len(),
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_pass_fail_and_pending_from_records() {
        // A clean Exp-1 record + an engaged Exp-2 sweep + a linear Exp-3 shard.
        let exp1 = r#"{"experiment":"exp1-latency-floor","stall":{"p999_over_p50":6.0,"max":null,"tripped":false}}"#;
        let exp2 = r#"{"experiment":"exp2-window-sweep","sweep":{"plateau":40000.0,"gain":40.0,"engaged":true}}"#;
        let exp3 =
            r#"{"experiment":"exp3-sharding","shard":{"efficiency":0.92,"scales_linearly":true}}"#;
        let find = |needle: &str| -> Option<String> {
            match needle {
                _ if "exp1-latency-floor".contains(needle) => Some(exp1.to_string()),
                _ if "exp2-window-sweep".contains(needle) => Some(exp2.to_string()),
                _ if "exp3-sharding".contains(needle) => Some(exp3.to_string()),
                _ => None,
            }
        };
        let rows = build_rows(&find);
        // Exp-1 stall row passes (6× ≤ 50×).
        assert_eq!(rows[0].pass, Some(true));
        // Exp-2 gain row passes (40× > 1.5×).
        let gain_row = rows
            .iter()
            .find(|r| r.metric.contains("plateau / Exp-1"))
            .unwrap();
        assert_eq!(gain_row.pass, Some(true));
        // Exp-3 sharding row passes (0.92 ≥ 0.80).
        let shard_row = rows.iter().find(|r| r.metric.contains("sharding")).unwrap();
        assert_eq!(shard_row.pass, Some(true));
        // No Exp-4 record → crash-safety row is pending.
        let crash_row = rows
            .iter()
            .find(|r| r.metric.contains("crash-safety"))
            .unwrap();
        assert_eq!(crash_row.pass, None);
    }

    #[test]
    fn stall_row_fails_on_a_pathological_ratio() {
        let exp1 = r#"{"experiment":"exp1-latency-floor","stall":{"p999_over_p50":120.0}}"#;
        let find = |needle: &str| -> Option<String> {
            if "exp1-latency-floor".contains(needle) {
                Some(exp1.to_string())
            } else {
                None
            }
        };
        let rows = build_rows(&find);
        assert_eq!(rows[0].pass, Some(false)); // 120× > 50× → stall
        assert!(render(&rows).contains("FAIL"));
    }
}
