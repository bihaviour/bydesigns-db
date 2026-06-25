//! Feature-gated (`custom-profile`) YAML workload-profile loader for the
//! `custom` subcommand (issue #81; spec 15 "Benchmark scenarios — custom").
//!
//! The named scenarios (`read-heavy`/`write-heavy`/`mixed-oltp`) make the op
//! *ratios* data; `custom` makes the *whole shape* data — duration, connections,
//! the working-set size, the seed, and the op mix — so a workload we don't want
//! to name in the binary becomes a file rather than code. A parsed [`Profile`]
//! is translated into the existing transport-agnostic mix driver
//! ([`crate::workload::run_mix_core`]), so `custom` reuses the *same* loop as the
//! named scenarios — there is no second execution path.
//!
//! **Scope line (deliberately narrow).** A profile expresses exactly the
//! mix-driver vocabulary: the four op kinds' weights plus the run knobs. There is
//! **no custom SQL** in v1 — the value over `--mix` is making the whole shape
//! data, not turning the profile into a mini query language. Anything outside the
//! documented field set is a hard parse error, not a silent ignore.
//!
//! **Dependency wall.** The YAML parser here is hand-rolled — a tiny line-based
//! reader for the flat-keys-plus-one-`mix`-block subset the schema needs — so the
//! feature pulls in *no* external crate (the workspace keeps its two-dependency
//! footprint). The `custom-profile` feature gate still stands the wall the issue
//! asks for: a default build compiles this module out entirely, and the `custom`
//! command then reports a rebuild hint (see [`crate::run_custom`]).

use crate::workload::{Mix, MixRun};
use crate::{BenchError, MixRealized, Opts, Report};
use std::time::Duration;

/// A parsed workload profile — the whole shape of a `custom` run as data.
///
/// The numeric knobs carry the same millisecond / count vocabulary as the CLI
/// flags (`duration_ms`, `warmup_ms`, `connections`, `rows`), so a profile reads
/// like the flags it replaces. `url` is optional (a `--url` flag overrides it);
/// `seed` fixes the op stream for a reproducible realized mix.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// Free-form label recorded in the report (optional).
    pub label: Option<String>,
    /// Backend URL; a CLI `--url` overrides it, and absence of both is an error.
    pub url: Option<String>,
    /// Timed measurement window (ms). Must be > 0.
    pub duration_ms: u64,
    /// Discarded warm-up window (ms).
    pub warmup_ms: u64,
    /// Concurrent writers. Must be >= 1.
    pub connections: usize,
    /// Pre-seeded working-set row count.
    pub rows: u64,
    /// Fixed PRNG seed for a reproducible op stream (optional).
    pub seed: Option<u64>,
    /// Per-op weights over the four mix-driver op kinds.
    pub mix: ProfileMix,
}

/// The op-ratio weights of a profile, one per mix-driver op kind. Drawing an op
/// walks the cumulative weight, so the long-run mix tracks these ratios — the
/// weights are arbitrary positive integers, not required to sum to 100.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfileMix {
    pub select: u64,
    pub insert: u64,
    pub update: u64,
    pub delete: u64,
}

impl Default for Profile {
    /// Defaults mirror the CLI flag defaults, so a minimal profile (just a `mix`
    /// block) runs the same shape the named scenarios would at their defaults.
    fn default() -> Profile {
        Profile {
            label: None,
            url: None,
            duration_ms: 1000,
            warmup_ms: 200,
            connections: 1,
            rows: 1000,
            seed: None,
            mix: ProfileMix::default(),
        }
    }
}

impl Profile {
    /// Serialize back to the canonical YAML form. Used by the round-trip test and
    /// available for emitting a profile a run was driven from. Optional fields are
    /// emitted only when present, so a re-parse reconstructs an equal [`Profile`].
    pub fn to_yaml(&self) -> String {
        let mut s = String::new();
        if let Some(l) = &self.label {
            s.push_str(&format!("label: {l}\n"));
        }
        if let Some(u) = &self.url {
            s.push_str(&format!("url: {u}\n"));
        }
        s.push_str(&format!("duration_ms: {}\n", self.duration_ms));
        s.push_str(&format!("warmup_ms: {}\n", self.warmup_ms));
        s.push_str(&format!("connections: {}\n", self.connections));
        s.push_str(&format!("rows: {}\n", self.rows));
        if let Some(seed) = self.seed {
            s.push_str(&format!("seed: {seed}\n"));
        }
        s.push_str("mix:\n");
        s.push_str(&format!("  select: {}\n", self.mix.select));
        s.push_str(&format!("  insert: {}\n", self.mix.insert));
        s.push_str(&format!("  update: {}\n", self.mix.update));
        s.push_str(&format!("  delete: {}\n", self.mix.delete));
        s
    }

    fn mix_total(&self) -> u64 {
        self.mix.select + self.mix.insert + self.mix.update + self.mix.delete
    }
}

/// Strip a trailing `# comment` from one line, honoring single/double quotes so a
/// `#` inside a value (e.g. a URL fragment) is not mistaken for a comment. A `#`
/// starts a comment only at the line start or after whitespace (YAML's rule).
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let (mut in_single, mut in_double, mut prev_ws) = (false, false, true);
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && prev_ws => return &line[..i],
            _ => {}
        }
        prev_ws = c == b' ' || c == b'\t';
    }
    line
}

/// Strip a single layer of matching quotes from a scalar value.
fn unquote(v: &str) -> &str {
    let b = v.as_bytes();
    if v.len() >= 2
        && ((b[0] == b'"' && b[v.len() - 1] == b'"') || (b[0] == b'\'' && b[v.len() - 1] == b'\''))
    {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

/// Split a `key: value` line at the first colon, trimming both sides.
fn split_kv(s: &str) -> Result<(&str, &str), String> {
    match s.find(':') {
        Some(i) => Ok((s[..i].trim(), s[i + 1..].trim())),
        None => Err(format!("expected `key: value`, found `{s}`")),
    }
}

/// Parse an unsigned integer scalar, mapping failure (including an empty value)
/// to a clear per-field message.
fn parse_u64(val: &str, key: &str) -> Result<u64, String> {
    if val.is_empty() {
        return Err(format!("expected a number for `{key}`"));
    }
    unquote(val)
        .parse::<u64>()
        .map_err(|_| format!("invalid number for `{key}`: `{val}`"))
}

/// Parse a YAML workload profile from text. Deliberately a *subset* parser: flat
/// top-level `key: value` lines plus one indented `mix:` block. Anything outside
/// the documented schema — an unknown field, an unknown op, tabs in indentation,
/// stray nesting — is a hard error, so a typo fails loudly rather than running a
/// silently-wrong workload.
pub fn parse_profile(text: &str) -> Result<Profile, String> {
    let mut p = Profile::default();
    let mut in_mix = false;
    let mut mix_seen = false;

    for (lineno, raw) in text.lines().enumerate() {
        let line = lineno + 1;
        let content = strip_comment(raw).trim_end();
        if content.trim().is_empty() {
            continue;
        }
        let indent = content.len() - content.trim_start().len();
        if content[..indent].contains('\t') {
            return Err(format!("line {line}: tabs are not allowed in indentation"));
        }
        let body = content.trim_start();
        let (key, val) = split_kv(body).map_err(|e| format!("line {line}: {e}"))?;

        if indent == 0 {
            in_mix = false;
            match key {
                "mix" => {
                    if !val.is_empty() {
                        return Err(format!(
                            "line {line}: `mix` is a block — put the weights on indented lines below it"
                        ));
                    }
                    in_mix = true;
                    mix_seen = true;
                }
                "label" | "url" => {
                    if val.is_empty() {
                        return Err(format!("line {line}: expected a value for `{key}`"));
                    }
                    let v = unquote(val).to_string();
                    if key == "label" {
                        p.label = Some(v);
                    } else {
                        p.url = Some(v);
                    }
                }
                "duration_ms" => p.duration_ms = parse_u64(val, key).map_err(|e| pfx(line, e))?,
                "warmup_ms" => p.warmup_ms = parse_u64(val, key).map_err(|e| pfx(line, e))?,
                "connections" => {
                    p.connections = parse_u64(val, key).map_err(|e| pfx(line, e))? as usize
                }
                "rows" => p.rows = parse_u64(val, key).map_err(|e| pfx(line, e))?,
                "seed" => p.seed = Some(parse_u64(val, key).map_err(|e| pfx(line, e))?),
                other => {
                    return Err(format!(
                        "line {line}: unknown field `{other}` (expected one of: label, url, \
                         duration_ms, warmup_ms, connections, rows, seed, mix)"
                    ))
                }
            }
        } else {
            if !in_mix {
                return Err(format!(
                    "line {line}: unexpected indentation (only the `mix:` weights are nested)"
                ));
            }
            let w = parse_u64(val, key).map_err(|e| pfx(line, e))?;
            match key {
                "select" => p.mix.select = w,
                "insert" => p.mix.insert = w,
                "update" => p.mix.update = w,
                "delete" => p.mix.delete = w,
                other => {
                    return Err(format!(
                        "line {line}: unknown op `{other}` in mix (expected select/insert/update/delete)"
                    ))
                }
            }
        }
    }

    if !mix_seen {
        return Err("no `mix:` block (nothing to run)".to_string());
    }
    Ok(p)
}

/// Prefix a per-field error with its line number.
fn pfx(line: usize, e: String) -> String {
    format!("line {line}: {e}")
}

/// Reject a malformed/contradictory profile with a clear message; the caller maps
/// it to the config-error exit code (3). The parser already rejects unknown
/// fields/ops and bad syntax; this catches the *semantic* contradictions (issue
/// #81 C4: weights not summing, zero duration, no connections).
fn validate(p: &Profile) -> Result<(), String> {
    if p.duration_ms == 0 {
        return Err("duration_ms must be > 0".to_string());
    }
    if p.connections == 0 {
        return Err("connections must be >= 1".to_string());
    }
    if p.mix_total() == 0 {
        return Err("mix weights must not all be zero (nothing to run)".to_string());
    }
    Ok(())
}

/// Load, validate, and run the profile named by `--profile` (the `custom`
/// subcommand). Every failure on this path is a *configuration* error (exit code
/// 3): a missing/unreadable file, a parse error, or a semantic contradiction.
pub(crate) fn run_custom(opts: &Opts) -> Result<Report, BenchError> {
    let path = opts.profile.as_ref().ok_or_else(|| {
        BenchError::Config("`custom` requires --profile <FILE> (a YAML workload profile)".into())
    })?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| BenchError::Config(format!("reading profile {path}: {e}")))?;
    let profile =
        parse_profile(&text).map_err(|e| BenchError::Config(format!("profile {path}: {e}")))?;
    validate(&profile).map_err(|e| BenchError::Config(format!("profile {path}: {e}")))?;
    run_profile(&profile, opts)
}

/// Translate a validated [`Profile`] into the shared mix driver and run it. The
/// transport / output knobs come from the CLI `opts` (so `custom` runs on both
/// the embedded and pgwire transports like every other scenario); the workload
/// shape comes from the profile. The realized op mix is attached to the report,
/// proving the driven shape tracked the requested ratios.
pub(crate) fn run_profile(profile: &Profile, opts: &Opts) -> Result<Report, BenchError> {
    // CLI `--url` overrides the profile's url; absence of both is a config error.
    let url = if !opts.url.is_empty() {
        opts.url.clone()
    } else {
        profile.url.clone().ok_or_else(|| {
            BenchError::Config(
                "no --url given and the profile sets no `url` (nothing to connect to)".into(),
            )
        })?
    };

    let mut eff = opts.clone();
    eff.url = url;

    let mix = Mix::new(
        profile.mix.select,
        profile.mix.insert,
        profile.mix.update,
        profile.mix.delete,
    );
    let label = profile.label.clone().unwrap_or_else(|| eff.label.clone());
    let run = MixRun {
        name: "custom",
        mix,
        writers: profile.connections.max(1),
        rows: profile.rows.max(1),
        warmup: Duration::from_millis(profile.warmup_ms),
        duration: Duration::from_millis(profile.duration_ms),
        seed: profile.seed,
        label,
    };

    let (mut report, realized) = crate::workload::run_mix_core(&run, &eff)?;
    report.mix_realized = Some(MixRealized {
        configured: mix.weights(),
        realized,
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_url() -> String {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("twill-bench-prof-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        format!("file://{}", p.display())
    }

    /// Build an `Opts` carrying just a `--url` (and `--json`), the way the binary
    /// would for a `custom` run whose other knobs come from the profile.
    fn opts_for(url: &str) -> Opts {
        Opts::parse(&["--url".to_string(), url.to_string(), "--json".to_string()])
            .expect("opts parse")
    }

    /// A representative profile parses into exactly the documented fields, and
    /// comments / blank lines / quoted scalars are handled.
    #[test]
    fn parses_a_full_profile() {
        let yaml = "\
# a SaaS-shaped custom workload
label: \"saas mix\"
url: file:///tmp/x.db
duration_ms: 2500   # timed window
warmup_ms: 300
connections: 8
rows: 5000
seed: 42

mix:
  select: 70
  insert: 20
  update: 8
  delete: 2
";
        let p = parse_profile(yaml).expect("parse");
        assert_eq!(p.label.as_deref(), Some("saas mix"));
        assert_eq!(p.url.as_deref(), Some("file:///tmp/x.db"));
        assert_eq!(p.duration_ms, 2500);
        assert_eq!(p.warmup_ms, 300);
        assert_eq!(p.connections, 8);
        assert_eq!(p.rows, 5000);
        assert_eq!(p.seed, Some(42));
        assert_eq!(
            p.mix,
            ProfileMix {
                select: 70,
                insert: 20,
                update: 8,
                delete: 2
            }
        );
    }

    /// C1 round-trip: serialize → parse reconstructs an equal profile, for both a
    /// fully-populated profile and a minimal one (defaults + just a mix).
    #[test]
    fn yaml_round_trips() {
        let full = Profile {
            label: Some("rt".to_string()),
            url: Some("file:///tmp/rt.db".to_string()),
            duration_ms: 1234,
            warmup_ms: 56,
            connections: 4,
            rows: 777,
            seed: Some(99),
            mix: ProfileMix {
                select: 50,
                insert: 30,
                update: 15,
                delete: 5,
            },
        };
        assert_eq!(parse_profile(&full.to_yaml()).unwrap(), full);

        let minimal = Profile {
            mix: ProfileMix {
                select: 1,
                insert: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(parse_profile(&minimal.to_yaml()).unwrap(), minimal);
    }

    /// C4 validation: the semantic contradictions are each rejected.
    #[test]
    fn validation_rejects_contradictions() {
        let zero_mix = Profile {
            mix: ProfileMix::default(),
            ..Default::default()
        };
        assert!(
            validate(&zero_mix).is_err(),
            "all-zero mix must be rejected"
        );

        let no_conns = Profile {
            connections: 0,
            mix: ProfileMix {
                select: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(
            validate(&no_conns).is_err(),
            "connections 0 must be rejected"
        );

        let no_dur = Profile {
            duration_ms: 0,
            mix: ProfileMix {
                select: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(validate(&no_dur).is_err(), "duration 0 must be rejected");
    }

    /// The parser rejects the out-of-scope / malformed cases loudly.
    #[test]
    fn parser_rejects_malformed_input() {
        assert!(parse_profile("connections: 4\n").is_err(), "no mix block");
        assert!(
            parse_profile("bogus: 1\nmix:\n  select: 1\n").is_err(),
            "unknown field"
        );
        assert!(
            parse_profile("mix:\n  upsert: 1\n").is_err(),
            "unknown op kind"
        );
        assert!(
            parse_profile("duration_ms: notanumber\nmix:\n  select: 1\n").is_err(),
            "non-numeric scalar"
        );
        assert!(
            parse_profile("mix:\n\tselect: 1\n").is_err(),
            "tab indentation"
        );
        assert!(
            parse_profile("  select: 1\n").is_err(),
            "indentation with no mix block"
        );
    }

    /// A `#` inside a value (no preceding whitespace) is part of the value, not a
    /// comment — so a URL fragment survives.
    #[test]
    fn hash_inside_value_is_not_a_comment() {
        let p = parse_profile("url: file:///tmp/x.db#frag\nmix:\n  select: 1\n").unwrap();
        assert_eq!(p.url.as_deref(), Some("file:///tmp/x.db#frag"));
    }

    /// C5 end-to-end: load a profile, drive it against a real `file://` engine
    /// under a fixed seed, and assert the realized op mix tracks the configured
    /// ratios within tolerance (mirrors the #50 mix test, but over the full
    /// load-profile → drive path rather than the picker alone).
    #[test]
    fn realized_mix_tracks_the_profile() {
        let url = unique_url();
        let yaml = "\
duration_ms: 250
warmup_ms: 0
connections: 2
rows: 64
seed: 12345
mix:
  select: 70
  insert: 20
  update: 8
  delete: 2
";
        let profile = parse_profile(yaml).unwrap();
        validate(&profile).unwrap();
        let report = run_profile(&profile, &opts_for(&url)).expect("custom run");

        let m = report.mix_realized.expect("realized mix present");
        let total: u64 = m.realized.iter().sum();
        assert!(total > 200, "expected a healthy op count, got {total}");
        let want = [0.70, 0.20, 0.08, 0.02];
        let kinds = ["select", "insert", "update", "delete"];
        for i in 0..4 {
            let got = m.realized[i] as f64 / total as f64;
            assert!(
                (got - want[i]).abs() < 0.07,
                "{} realized {:.3}, want {:.2}",
                kinds[i],
                got,
                want[i]
            );
        }
    }
}
