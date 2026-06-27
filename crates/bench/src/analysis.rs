//! Curve analysis and acceptance gates for the spec-09 validation campaign
//! (issue #91 — V-1..V-5). These are the *interpretation* layer: pure,
//! dependency-free functions that turn a raw experiment result (a percentile
//! pair, a throughput-vs-concurrency curve, an aggregate-vs-shard-count curve)
//! into the falsifiable verdicts spec 09 asks for. They live apart from the
//! drivers so the verdict logic is unit-testable without spinning up an engine
//! — every gate below is pinned by a test that fires it on a synthetic stall and
//! passes it on a clean curve, which is exactly the acceptance criterion the
//! sub-issues state.
//!
//! Three families, one per W-axis lever (spec 09):
//!   * **stall gate** (V-1) — the Exp-1 `p999/p50` ratio. A small multiple is
//!     network jitter; orders-of-magnitude is a commit-path stall (sync
//!     compaction, CAS-retry storm) to investigate before trusting any
//!     downstream curve.
//!   * **knee detection** (V-2 / V-4) — where a saturating curve levels off. The
//!     Exp-2 plateau knee (throughput vs concurrency) and the Exp-5 thundering-
//!     herd knee (cold-start latency vs N) are the same shape problem.
//!   * **plateau gain & scaling slope** (V-2 / V-3) — the W1 lever's payoff
//!     (plateau ÷ Exp-1 ceiling) and the W2 lever's payoff (how near-linear the
//!     N-database aggregate scales).

/// The Exp-1 stall ratio `p999 / p50` (V-1). A clean, jitter-only distribution
/// keeps this a small multiple; a value orders of magnitude high is the
/// commit-path stall signal spec 09 flags. `p50 == 0` (an empty run) yields `0`
/// so the gate never divides by zero into a NaN.
pub fn stall_ratio(p50: u64, p999: u64) -> f64 {
    if p50 == 0 {
        0.0
    } else {
        p999 as f64 / p50 as f64
    }
}

/// V-1 acceptance gate: the Exp-1 `p999/p50` ratio against an upper bound. The
/// gate *trips* (returns `true`) when the observed ratio exceeds `max` — a
/// pathological tail that must be investigated before the downstream Exp-2/Exp-3
/// curves can be trusted. spec 09 suggests ~50× as the threshold for a real
/// network object store.
pub fn stall_gate_tripped(p50: u64, p999: u64, max: f64) -> bool {
    max > 0.0 && stall_ratio(p50, p999) > max
}

/// Least-squares slope of `ys` over `xs` (the same fit `longrun` uses for its
/// drift trend). `0.0` for a degenerate input (fewer than two points, mismatched
/// lengths, or zero variance in `xs`).
pub fn slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 || n != ys.len() {
        return 0.0;
    }
    let nf = n as f64;
    let mean_x = xs.iter().sum::<f64>() / nf;
    let mean_y = ys.iter().sum::<f64>() / nf;
    let mut num = 0.0;
    let mut den = 0.0;
    for (x, y) in xs.iter().zip(ys.iter()) {
        num += (x - mean_x) * (y - mean_y);
        den += (x - mean_x) * (x - mean_x);
    }
    if den.abs() < f64::EPSILON {
        0.0
    } else {
        num / den
    }
}

/// Detect the **knee** of a saturating curve — the x-index past which `ys` stops
/// rising meaningfully (V-2 plateau, V-4 herd saturation). This is the Kneedle
/// "point of maximum distance from the chord" construction: normalise both axes
/// to `[0, 1]`, draw the straight chord from the first to the last point, and
/// return the index whose value sits farthest *off* that chord (above it for a
/// concave-increasing throughput curve; the same magnitude works for the
/// convex-increasing latency curve via [`knee_convex`]). Returns `None` for a
/// curve too short (< 3 points) or perfectly flat/straight to have a knee.
pub fn knee(xs: &[f64], ys: &[f64]) -> Option<usize> {
    knee_inner(xs, ys, true)
}

/// The knee of a **convex-increasing** curve (cold-start latency vs concurrency,
/// V-4): the elbow where it bends *upward*, i.e. the point farthest *below* the
/// chord. Same construction as [`knee`] with the distance sign flipped.
pub fn knee_convex(xs: &[f64], ys: &[f64]) -> Option<usize> {
    knee_inner(xs, ys, false)
}

fn knee_inner(xs: &[f64], ys: &[f64], concave: bool) -> Option<usize> {
    let n = xs.len();
    if n < 3 || n != ys.len() {
        return None;
    }
    let (x0, x1) = (xs[0], xs[n - 1]);
    let (y0, y1) = (ys[0], ys[n - 1]);
    let xspan = x1 - x0;
    let yspan = y1 - y0;
    if xspan.abs() < f64::EPSILON || yspan.abs() < f64::EPSILON {
        return None;
    }
    let mut best_idx = 0usize;
    let mut best_dist = 0.0f64;
    for i in 1..n - 1 {
        let nx = (xs[i] - x0) / xspan;
        let ny = (ys[i] - y0) / yspan;
        // Signed vertical gap between the normalised point and the chord (the
        // chord on normalised axes is simply y = x). Positive = above the chord.
        let gap = ny - nx;
        let dist = if concave { gap } else { -gap };
        if dist > best_dist {
            best_dist = dist;
            best_idx = i;
        }
    }
    // A knee that never rises off the chord (best_dist ≈ 0) is no knee at all.
    if best_dist > 1e-9 {
        Some(best_idx)
    } else {
        None
    }
}

/// The W1-lever payoff (V-2): the Exp-2 plateau throughput as a multiple of the
/// Exp-1 single-commit ceiling (`1 / latency`). spec 09 expects 10–100× when
/// group commit engages; a ratio at or below `1.5×` means batching is *not*
/// coalescing — every commit is paying its own round-trip.
pub fn plateau_gain(plateau_throughput: f64, exp1_ceiling: f64) -> f64 {
    if exp1_ceiling <= 0.0 {
        0.0
    } else {
        plateau_throughput / exp1_ceiling
    }
}

/// V-2 gate: group commit is *not* engaging when the plateau gain is `<= 1.5×`
/// the Exp-1 ceiling (the "Exp-2 plateau ≈ Exp-1 ceiling" red flag in spec 09's
/// acceptance table).
pub fn group_commit_not_engaging(plateau_throughput: f64, exp1_ceiling: f64) -> bool {
    plateau_gain(plateau_throughput, exp1_ceiling) <= 1.5
}

/// The Exp-1 single-commit ceiling `1 / latency` in commits/sec, from the
/// median single-commit latency in microseconds. `0` for a zero latency.
pub fn exp1_ceiling_from_p50_us(p50_us: u64) -> f64 {
    if p50_us == 0 {
        0.0
    } else {
        1_000_000.0 / p50_us as f64
    }
}

/// The W2-lever payoff (V-3): how near-linear the N-database aggregate throughput
/// scales. Defined as the *speedup efficiency* — the achieved speedup
/// (`agg(N_max) / agg(N_min)`) divided by the ideal linear speedup
/// (`N_max / N_min`). `1.0` is perfectly linear; spec 09 / the V-3 acceptance
/// asks for `>= 0.8` (within ~80% of linear). Anything well below that, with the
/// per-DB work unchanged, points at a shared serialization resource — the open
/// question of whether the S3-CAS commit log is a cross-DB bottleneck.
pub fn scaling_efficiency(shard_counts: &[f64], aggregate_throughput: &[f64]) -> f64 {
    let n = shard_counts.len();
    if n < 2 || n != aggregate_throughput.len() {
        return 0.0;
    }
    let (n_min, n_max) = (shard_counts[0], shard_counts[n - 1]);
    let (t_min, t_max) = (aggregate_throughput[0], aggregate_throughput[n - 1]);
    if n_min <= 0.0 || t_min <= 0.0 {
        return 0.0;
    }
    let ideal = n_max / n_min;
    let achieved = t_max / t_min;
    if ideal <= 0.0 {
        0.0
    } else {
        achieved / ideal
    }
}

/// V-3 gate: the sharding lever recovers near-linear scaling when the efficiency
/// is `>= 0.8`. A `false` here is the documented cross-DB serialization finding
/// (lanes are not independent — investigate the shared CAS commit log).
pub fn sharding_scales_linearly(shard_counts: &[f64], aggregate_throughput: &[f64]) -> bool {
    scaling_efficiency(shard_counts, aggregate_throughput) >= 0.8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_ratio_and_gate() {
        // A clean distribution: p999 a small multiple of p50 — gate stays shut.
        assert!((stall_ratio(100, 400) - 4.0).abs() < 1e-9);
        assert!(!stall_gate_tripped(100, 400, 50.0));
        // A commit-path stall: p999 orders of magnitude above p50 — gate fires.
        assert!(stall_gate_tripped(100, 8_000, 50.0));
        // An empty run never divides by zero, and a zero ceiling disables.
        assert_eq!(stall_ratio(0, 500), 0.0);
        assert!(!stall_gate_tripped(100, 8_000, 0.0));
    }

    #[test]
    fn knee_finds_the_plateau_of_a_saturating_curve() {
        // A throughput-vs-concurrency curve that rises then plateaus around x=8.
        let xs = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
        let ys = [1_000.0, 2_000.0, 3_800.0, 6_000.0, 6_300.0, 6_400.0];
        let k = knee(&xs, &ys).expect("a saturating curve has a knee");
        // The knee should land in the bend (around the x=4..8 region), not at the
        // ends and not out on the flat tail.
        assert!((2..=3).contains(&k), "knee landed at index {k}");
    }

    #[test]
    fn knee_is_none_for_a_straight_or_flat_curve() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        let straight = [10.0, 20.0, 30.0, 40.0];
        let flat = [5.0, 5.0, 5.0, 5.0];
        assert_eq!(knee(&xs, &straight), None);
        assert_eq!(knee(&xs, &flat), None);
        assert_eq!(knee(&[1.0, 2.0], &[1.0, 2.0]), None); // too short
    }

    #[test]
    fn convex_knee_finds_the_latency_elbow() {
        // Cold-start latency flat-then-rising: the herd saturation elbow is where
        // the curve bends up out of the flat region (the x=8..16 knee).
        let xs = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
        let ys = [10.0, 11.0, 12.0, 14.0, 60.0, 200.0];
        let k = knee_convex(&xs, &ys).expect("a convex-rising curve has an elbow");
        assert!((3..=4).contains(&k), "elbow landed at index {k}");
    }

    #[test]
    fn plateau_gain_and_engagement_gate() {
        // Exp-1 ceiling 1000/s; a plateau of 40_000/s is a 40× win — engaged.
        assert!((plateau_gain(40_000.0, 1_000.0) - 40.0).abs() < 1e-9);
        assert!(!group_commit_not_engaging(40_000.0, 1_000.0));
        // A plateau barely above the ceiling means no coalescing — gate fires.
        assert!(group_commit_not_engaging(1_400.0, 1_000.0));
        assert!(group_commit_not_engaging(1_500.0, 1_000.0)); // boundary is inclusive
        assert_eq!(exp1_ceiling_from_p50_us(1_000), 1_000.0);
    }

    #[test]
    fn scaling_efficiency_and_linear_gate() {
        // 1→8 databases, throughput 1000→7600: 7.6× of an ideal 8× = 0.95.
        let n = [1.0, 2.0, 4.0, 8.0];
        let linear = [1_000.0, 2_000.0, 3_900.0, 7_600.0];
        assert!((scaling_efficiency(&n, &linear) - 0.95).abs() < 1e-9);
        assert!(sharding_scales_linearly(&n, &linear));
        // A shared serializer caps aggregate throughput: 1000→2000 over 8× is
        // only 0.25 efficiency — the cross-DB bottleneck finding.
        let capped = [1_000.0, 1_500.0, 1_800.0, 2_000.0];
        assert!(!sharding_scales_linearly(&n, &capped));
        assert!(scaling_efficiency(&n, &capped) < 0.8);
    }

    #[test]
    fn slope_tracks_a_line_and_is_zero_on_degenerate_input() {
        assert!((slope(&[0.0, 1.0, 2.0], &[0.0, 2.0, 4.0]) - 2.0).abs() < 1e-9);
        assert_eq!(slope(&[1.0], &[1.0]), 0.0);
        assert_eq!(slope(&[1.0, 1.0], &[2.0, 5.0]), 0.0); // zero x-variance
    }
}
