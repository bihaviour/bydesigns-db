//! A minimal, dependency-free Postgres-wire client — just enough of the simple
//! query protocol to drive the benchmark experiments over the [`twill-server`]
//! pgwire listener (spec 07 / spec 09 "server path").
//!
//! This is the *driver* side of the wire that mirrors what `pgbench` does on a
//! real host: connect, run a statement, observe success or a (retry-able)
//! serialization failure. Keeping it in-crate makes the server-mode experiments
//! offline-testable — they run against an in-process listener with no external
//! Postgres tooling — while the same experiments can point at a deployed
//! `engine-server` (or be reproduced with `pgbench`) on a real host for the
//! numbers that gate placement.
//!
//! Only the subset the benchmark needs is implemented: protocol-3.0 startup
//! (cleartext, trust auth), the simple `Query` message, and just enough result
//! parsing to classify a `40001` serialization failure (the wire form of the
//! engine's first-committer/first-toucher conflict) from a fatal error, and to
//! read a single scalar back for verification.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The outcome class a benchmark writer cares about: a clean commit, a
/// retry-able conflict (SQLSTATE `40001`, serialization_failure), or a fatal
/// error that should abort the run.
#[derive(Debug)]
pub enum ExecError {
    /// Serialization failure (`40001`): first-committer/first-toucher-wins. The
    /// driver retries, exactly as `pgbench --max-tries` would.
    Conflict,
    /// Any other error (syntax, storage, fatal) — the run cannot continue.
    Fatal(String),
}

/// A live connection to an `engine-server` over the Postgres wire protocol.
pub struct PgClient {
    stream: TcpStream,
}

impl PgClient {
    /// Connect to `addr` (`host:port`) and complete the protocol-3.0 startup
    /// handshake (cleartext, trust auth — the server's supported subset).
    pub fn connect(addr: &str) -> io::Result<PgClient> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        // A generous read timeout so a wedged server fails the run loudly rather
        // than hanging the benchmark forever.
        stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
        let mut c = PgClient { stream };
        c.startup()?;
        Ok(c)
    }

    fn startup(&mut self) -> io::Result<()> {
        // StartupMessage: protocol 3.0, then NUL-terminated key/value pairs.
        let mut body = Vec::new();
        body.extend_from_slice(&196608i32.to_be_bytes());
        for (k, v) in [("user", "bench"), ("database", "bench")] {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut msg = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        msg.extend_from_slice(&body);
        self.stream.write_all(&msg)?;
        self.stream.flush()?;
        // Consume auth + ParameterStatus + BackendKeyData up to ReadyForQuery.
        self.drain_to_ready()?;
        Ok(())
    }

    /// Run `sql` (simple query), returning `Ok` on success, `Err(Conflict)` on a
    /// `40001` serialization failure, or `Err(Fatal)` on any other error.
    pub fn exec(&mut self, sql: &str) -> Result<(), ExecError> {
        self.send_query(sql)
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        let err = self
            .collect_to_ready(&mut |_| {})
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        match err {
            None => Ok(()),
            Some((code, _)) if code == "40001" => Err(ExecError::Conflict),
            Some((_, msg)) => Err(ExecError::Fatal(msg)),
        }
    }

    /// Run a `SELECT` returning a single integer cell (used for verification).
    pub fn query_scalar_i64(&mut self, sql: &str) -> Result<i64, ExecError> {
        self.send_query(sql)
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        let mut first: Option<Option<String>> = None;
        let err = self
            .collect_to_ready(&mut |cell| {
                if first.is_none() {
                    first = Some(cell);
                }
            })
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        if let Some((code, msg)) = err {
            return Err(if code == "40001" {
                ExecError::Conflict
            } else {
                ExecError::Fatal(msg)
            });
        }
        match first {
            Some(Some(s)) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| ExecError::Fatal(format!("non-integer scalar {s:?}"))),
            _ => Err(ExecError::Fatal("query returned no rows".into())),
        }
    }

    // ---- wire plumbing ----------------------------------------------------

    fn send_query(&mut self, sql: &str) -> io::Result<()> {
        let mut payload = sql.as_bytes().to_vec();
        payload.push(0);
        let mut msg = vec![b'Q'];
        msg.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
        msg.extend_from_slice(&payload);
        self.stream.write_all(&msg)?;
        self.stream.flush()
    }

    fn read_msg(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let mut tag = [0u8; 1];
        self.stream.read_exact(&mut tag)?;
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let n = (i32::from_be_bytes(len) as usize).saturating_sub(4);
        let mut body = vec![0u8; n];
        self.stream.read_exact(&mut body)?;
        Ok((tag[0], body))
    }

    /// Read and discard until `ReadyForQuery` (used after startup).
    fn drain_to_ready(&mut self) -> io::Result<()> {
        loop {
            let (tag, _) = self.read_msg()?;
            if tag == b'Z' {
                return Ok(());
            }
        }
    }

    /// Read until `ReadyForQuery`, calling `on_cell` with the first cell of each
    /// `DataRow`, and returning the `(SQLSTATE, message)` of any `ErrorResponse`.
    fn collect_to_ready(
        &mut self,
        on_cell: &mut dyn FnMut(Option<String>),
    ) -> io::Result<Option<(String, String)>> {
        let mut err = None;
        loop {
            let (tag, body) = self.read_msg()?;
            match tag {
                b'D' => on_cell(first_cell(&body)),
                b'E' => err = Some(parse_error(&body)),
                b'Z' => return Ok(err),
                _ => {} // RowDescription, CommandComplete, NoData, etc.: ignore
            }
        }
    }
}

/// Extract the first column of a `DataRow` body as text (`None` = SQL NULL).
fn first_cell(body: &[u8]) -> Option<String> {
    if body.len() < 2 {
        return None;
    }
    let ncols = i16::from_be_bytes([body[0], body[1]]);
    if ncols < 1 || body.len() < 6 {
        return None;
    }
    let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
    if len < 0 {
        return None;
    }
    let start = 6;
    let end = start + len as usize;
    if end > body.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&body[start..end]).into_owned())
}

/// Parse an `ErrorResponse` body into `(SQLSTATE code, message)`. Fields are
/// `type:u8` then a NUL-terminated string, ending with a zero type byte.
fn parse_error(body: &[u8]) -> (String, String) {
    let mut code = String::new();
    let mut message = String::new();
    let mut i = 0;
    while i < body.len() && body[i] != 0 {
        let field = body[i];
        i += 1;
        let start = i;
        while i < body.len() && body[i] != 0 {
            i += 1;
        }
        let val = String::from_utf8_lossy(&body[start..i]).into_owned();
        i += 1; // skip NUL
        match field {
            b'C' => code = val,
            b'M' => message = val,
            _ => {}
        }
    }
    (code, message)
}
