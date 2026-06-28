//! `engine-server` — the twill-db engine behind a Postgres-wire listener.
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
    let mut metrics: Option<String> = None;

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
            "--metrics" | "-m" => match args.next() {
                Some(v) => metrics = Some(v),
                None => return usage("--metrics requires an address"),
            },
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => return usage(&format!("unknown argument: {other}")),
        }
    }

    // Opt-in, operator-facing metrics on a separate address (no phone-home).
    if let Some(addr) = metrics {
        let db_for_metrics = db.clone();
        match std::net::TcpListener::bind(&addr) {
            Ok(l) => {
                eprintln!("engine-server: metrics on http://{addr}/metrics");
                std::thread::spawn(move || {
                    if let Err(e) = twill_server::metrics::serve_listener(l, &db_for_metrics) {
                        eprintln!("engine-server: metrics exporter stopped: {e}");
                    }
                });
            }
            Err(e) => return usage(&format!("--metrics bind {addr}: {e}")),
        }
    }

    eprintln!("engine-server: listening on {listen}, serving {db}");
    match twill_server::run(&listen, &db) {
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
        "usage: engine-server [--listen HOST:PORT] [--db URL] [--metrics HOST:PORT]\n\
         \n\
         \x20 --listen,  -l   address to bind (default 127.0.0.1:5433)\n\
         \x20 --db,      -d   engine URL: file://… (embedded) or s3://|r2://|gs://… (disaggregated)\n\
         \x20                 default file://./engine-server.db\n\
         \x20 --metrics, -m   serve Prometheus /metrics + /healthz on this address (off by default)"
    );
}
