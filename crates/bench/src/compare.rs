//! Release comparison (spec 15 "Release comparison"): diff two archived JSON
//! records — a baseline and a candidate — into a PASS/regression verdict, the
//! CI-friendly form of "did vX.Y.Z get slower?". This is pure post-processing
//! over records the driver already emits (each carries the git SHA and the full
//! percentile set), so it needs neither the engine nor a transport.
//!
//! The records are the flat one-line JSON the driver prints; to stay
//! dependency-free (the project hand-rolls its codecs) we extract just the
//! numeric fields we compare with a tiny scanner rather than pulling in a JSON
//! crate. A file may contain a human summary too — we take the last line that
//! looks like a JSON object.
//!
//! Verdict: the candidate regresses if its throughput drops, or its p99/p999
//! latency rises, by more than `--threshold` (default 10%). A regression exits
//! [`BENCH_FAILED`](crate::exit::BENCH_FAILED); a clean comparison exits
//! [`OK`](crate::exit::OK); bad flags or unreadable records exit
//! [`CONFIG`](crate::exit::CONFIG).

use crate::exit;

/// Parse `compare`'s own flags and run the diff, returning a process exit code.
pub fn run(args: &[String]) -> i32 {
    let mut baseline = None;
    let mut candidate = None;
    let mut threshold = 0.10f64;

    let mut i = 0;
    while i < args.len() {
        let key = args[i].as_str();
        let val = args.get(i + 1).cloned();
        match key {
            "--baseline" => baseline = val,
            "--candidate" => candidate = val,
            "--threshold" => match val.as_deref().map(str::parse::<f64>) {
                Some(Ok(t)) if t >= 0.0 => threshold = t,
                _ => {
                    eprintln!("error: --threshold must be a non-negative fraction (e.g. 0.10)");
                    return exit::CONFIG;
                }
            },
            other => {
                eprintln!("error: unknown flag {other} for compare");
                return exit::CONFIG;
            }
        }
        i += 2;
    }

    let (Some(baseline), Some(candidate)) = (baseline, candidate) else {
        eprintln!("error: compare requires --baseline <FILE> and --candidate <FILE>");
        return exit::CONFIG;
    };

    let base = match load_record(&baseline) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: baseline {baseline}: {e}");
            return exit::CONFIG;
        }
    };
    let cand = match load_record(&candidate) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: candidate {candidate}: {e}");
            return exit::CONFIG;
        }
    };

    print_comparison(&base, &cand, threshold)
}

/// The subset of a JSON record `compare` reasons about.
struct Record {
    experiment: String,
    git: String,
    throughput: f64,
    p50: f64,
    p99: f64,
    p999: f64,
}

fn load_record(path: &str) -> Result<Record, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    // Take the last line that looks like a JSON object (a file may also hold the
    // human summary the driver prints above the record).
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('{') && l.ends_with('}'))
        .ok_or("no JSON record found")?;

    Ok(Record {
        experiment: field_str(line, "experiment").unwrap_or_else(|| "?".into()),
        git: field_str(line, "git").unwrap_or_else(|| "?".into()),
        throughput: field_num(line, "throughput_per_s").ok_or("missing throughput_per_s")?,
        p50: field_num(line, "p50_us").ok_or("missing p50_us")?,
        p99: field_num(line, "p99_us").ok_or("missing p99_us")?,
        p999: field_num(line, "p999_us").ok_or("missing p999_us")?,
    })
}

fn print_comparison(base: &Record, cand: &Record, threshold: f64) -> i32 {
    println!("── release comparison ─────────────────────────────");
    println!("experiment   {} → {}", base.experiment, cand.experiment);
    println!("git          {} → {}", base.git, cand.git);
    println!("threshold    ±{:.0}%", threshold * 100.0);
    println!();

    // Throughput: higher is better; latency: lower is better.
    let tput = pct_change(base.throughput, cand.throughput);
    let p50 = pct_change(base.p50, cand.p50);
    let p99 = pct_change(base.p99, cand.p99);
    let p999 = pct_change(base.p999, cand.p999);

    row("throughput/s", base.throughput, cand.throughput, tput);
    row("p50 µs", base.p50, cand.p50, p50);
    row("p99 µs", base.p99, cand.p99, p99);
    row("p999 µs", base.p999, cand.p999, p999);
    println!();

    // A regression: throughput fell, or a tail latency rose, beyond threshold.
    let throughput_regressed = tput < -threshold;
    let latency_regressed = p99 > threshold || p999 > threshold;
    if throughput_regressed || latency_regressed {
        let mut why = Vec::new();
        if throughput_regressed {
            why.push(format!("throughput {:+.1}%", tput * 100.0));
        }
        if p99 > threshold {
            why.push(format!("p99 {:+.1}%", p99 * 100.0));
        }
        if p999 > threshold {
            why.push(format!("p999 {:+.1}%", p999 * 100.0));
        }
        println!("verdict      REGRESSION ({})", why.join(", "));
        exit::BENCH_FAILED
    } else {
        println!("verdict      PASS");
        exit::OK
    }
}

/// Relative change from `base` to `cand` as a signed fraction (0.10 = +10%).
fn pct_change(base: f64, cand: f64) -> f64 {
    if base == 0.0 {
        0.0
    } else {
        (cand - base) / base
    }
}

fn row(label: &str, base: f64, cand: f64, delta: f64) {
    println!(
        "{label:<13}{base:>12.1} → {cand:>12.1}  ({:+.1}%)",
        delta * 100.0
    );
}

// ---- minimal flat-JSON field extraction --------------------------------

/// Extract a string field `"key":"value"` from a flat JSON object line.
fn field_str(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = json.find(&needle)? + needle.len();
    let end = json[start..].find('"')? + start;
    Some(json[start..end].to_string())
}

/// Extract a numeric field `"key":<number>` from a flat JSON object line.
fn field_num(json: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let end = rest
        .find(|c: char| {
            !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
        })
        .unwrap_or(rest.len());
    rest[..end].parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REC: &str = "{\"experiment\":\"exp2-group-commit\",\"label\":\"\",\"transport\":\"embedded\",\
        \"backend\":\"file\",\"git\":\"abc1234\",\"writers\":8,\"duration_s\":1.000,\"commits\":1000,\
        \"conflicts\":0,\"failures\":0,\"throughput_per_s\":1000.0,\"p50_us\":100,\"p90_us\":150,\
        \"p95_us\":180,\"p99_us\":200,\"p999_us\":500,\"min_us\":50,\"max_us\":900,\"mean_us\":120.0,\
        \"correctness\":null}";

    #[test]
    fn extracts_string_and_numeric_fields() {
        assert_eq!(field_str(REC, "experiment").unwrap(), "exp2-group-commit");
        assert_eq!(field_str(REC, "git").unwrap(), "abc1234");
        assert_eq!(field_num(REC, "throughput_per_s").unwrap(), 1000.0);
        assert_eq!(field_num(REC, "p99_us").unwrap(), 200.0);
        assert_eq!(field_num(REC, "p999_us").unwrap(), 500.0);
        assert!(field_num(REC, "absent").is_none());
    }

    #[test]
    fn pct_change_is_signed_relative() {
        assert!((pct_change(100.0, 110.0) - 0.10).abs() < 1e-9);
        assert!((pct_change(100.0, 90.0) + 0.10).abs() < 1e-9);
        assert_eq!(pct_change(0.0, 5.0), 0.0);
    }
}
