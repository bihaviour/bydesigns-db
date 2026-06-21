//! `engine-server` — the bydesigns-db engine behind a Postgres-wire listener.
//!
//! ```text
//! engine-server [--listen HOST:PORT] [--db URL]
//!   --listen   address to bind         (default 127.0.0.1:5433)
//!   --db       engine connection URL    (default file://./engine-server.db)
//!              file://… embedded · s3://|r2://|gs://… disaggregated
//! ```

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut listen = "127.0.0.1:5433".to_string();
    let mut db = "file://./engine-server.db".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" | "-l" => match args.next() {
                Some(v) => listen = v,
                None => return usage("--listen requires an address"),
            },
            "--db" | "-d" => match args.next() {
                Some(v) => db = v,
                None => return usage("--db requires a URL"),
            },
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => return usage(&format!("unknown argument: {other}")),
        }
    }

    eprintln!("engine-server: listening on {listen}, serving {db}");
    match bydesigns_server::run(&listen, &db) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engine-server: fatal: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("engine-server: {msg}");
    print_help();
    ExitCode::FAILURE
}

fn print_help() {
    eprintln!(
        "usage: engine-server [--listen HOST:PORT] [--db URL]\n\
         \n\
         \x20 --listen, -l   address to bind (default 127.0.0.1:5433)\n\
         \x20 --db, -d       engine URL: file://… (embedded) or s3://|r2://|gs://… (disaggregated)\n\
         \x20                default file://./engine-server.db"
    );
}
