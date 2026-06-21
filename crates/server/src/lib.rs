//! # bydesigns-db · engine-server (spec 07, Phase 3)
//!
//! The embedded engine wrapped in a Postgres-wire listener. The listener is the
//! *only* new thing on the inbound edge: it frames protocol messages and turns
//! them into the same [`engine::Connection`] calls the embedded
//! (`bun:ffi`) path makes in-process. Nothing about SQL, MVCC, WAL, or the
//! storage seam changes — "the same library, two front doors."
//!
//! A `file://` URL serves the embedded backend; an `s3://`/`r2://`/`gs://` URL
//! serves the Phase-2 disaggregated backend — the server is oblivious to which,
//! because it only ever calls the engine.
//!
//! ## Supported protocol subset
//!
//! Protocol 3.0 startup (SSL/GSS probes declined → cleartext), trust auth, the
//! simple query protocol, and the full extended query protocol (Parse / Bind /
//! Describe / Execute / Sync / Close), with text and binary parameter/result
//! formats and field-tagged `ErrorResponse`s. SCRAM auth, TLS termination, and
//! `CancelRequest` are deliberate non-goals for this phase (see `docs/PHASE3.md`).

mod introspect;
mod protocol;
mod session;
mod types;

pub use session::serve;

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

/// Run the listener on `addr`, serving every connection against `db_url`.
///
/// One OS thread per connection (the engine is synchronous and single-writer;
/// a transaction-mode pooler in front absorbs serverless connection bursts —
/// spec 07). Blocks forever, or until the listener errors.
pub fn run(addr: &str, db_url: &str) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    serve_listener(listener, db_url)
}

/// Serve an already-bound listener (lets tests bind an ephemeral port first).
pub fn serve_listener(listener: TcpListener, db_url: &str) -> io::Result<()> {
    let db_url: Arc<str> = Arc::from(db_url);
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(_) => continue,
        };
        let db_url = db_url.clone();
        thread::spawn(move || handle(stream, &db_url));
    }
    Ok(())
}

fn handle(stream: TcpStream, db_url: &str) {
    let _ = stream.set_nodelay(true);
    if let Err(e) = serve(stream, db_url) {
        // A dropped/aborted client connection is normal; log only unexpected I/O.
        if e.kind() != io::ErrorKind::UnexpectedEof && e.kind() != io::ErrorKind::BrokenPipe {
            eprintln!("engine-server: connection error: {e}");
        }
    }
}
