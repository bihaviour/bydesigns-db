//! `twill-bench` binary — a thin shim over the [`twill_bench`] driver library,
//! which holds the experiments, the HDR-style histogram, and the embedded +
//! pgwire transports (spec 09; issue #6 / #29). See the library docs for usage.

fn main() {
    twill_bench::cli_main();
}
