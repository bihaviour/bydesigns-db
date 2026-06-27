//! `twilldb` binary — a thin shim over [`twilldb_cli`], which holds the
//! subcommand dispatch, argument parsing, and the embedded starter templates.
//! See the library docs for usage.

fn main() {
    twilldb_cli::cli_main();
}
