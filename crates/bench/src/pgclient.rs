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

/// The full result of a simple query: the column names, every row's text cells
/// (`None` = SQL NULL), and the `CommandComplete` command tag (e.g. `INSERT 0 3`,
/// `SELECT 2`). The management CLI rebuilds an engine result set from this.
#[derive(Default, Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub tag: String,
}

impl QueryResult {
    /// The trailing integer of the `CommandComplete` tag — the affected-row count
    /// for `INSERT`/`UPDATE`/`DELETE` (`INSERT 0 3` → 3, `UPDATE 5` → 5). `0` when
    /// the tag has no count (e.g. `CREATE TABLE`, `BEGIN`).
    pub fn affected(&self) -> i64 {
        self.tag
            .rsplit(' ')
            .next()
            .and_then(|n| n.parse::<i64>().ok())
            .unwrap_or(0)
    }
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

    /// Run a statement and collect the *full* result: the column names (from
    /// `RowDescription`), every row's text cells, and the `CommandComplete` tag.
    /// This is the form the management CLI (spec 19 Milestone 3) drives over the
    /// wire — it renders the column header, and reads the tag's trailing count for
    /// the affected-row report on an INSERT/UPDATE/DELETE. The benchmark hot loop
    /// uses the lighter scalar/exec paths instead.
    pub fn query_full(&mut self, sql: &str) -> Result<QueryResult, ExecError> {
        self.send_query(sql)
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        let mut result = QueryResult::default();
        let err = self
            .collect_full_to_ready(&mut result)
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        match err {
            None => Ok(result),
            Some((code, _)) if code == "40001" => Err(ExecError::Conflict),
            Some((_, msg)) => Err(ExecError::Fatal(msg)),
        }
    }

    /// Run a query and collect every row as a vector of text cells (`None` =
    /// SQL NULL). Used to read multi-column results such as the `twill.stats`
    /// observability surface (#53); the benchmark's hot loop uses the scalar
    /// path instead.
    pub fn query_rows(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>, ExecError> {
        self.send_query(sql)
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        let mut rows = Vec::new();
        let err = self
            .collect_rows_to_ready(&mut |row| rows.push(row))
            .map_err(|e| ExecError::Fatal(e.to_string()))?;
        match err {
            None => Ok(rows),
            Some((code, _)) if code == "40001" => Err(ExecError::Conflict),
            Some((_, msg)) => Err(ExecError::Fatal(msg)),
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

    /// Read until `ReadyForQuery`, filling `result` with the `RowDescription`
    /// column names, every `DataRow`'s cells, and the `CommandComplete` tag, and
    /// returning any `ErrorResponse`'s `(SQLSTATE, message)`.
    fn collect_full_to_ready(
        &mut self,
        result: &mut QueryResult,
    ) -> io::Result<Option<(String, String)>> {
        let mut err = None;
        loop {
            let (tag, body) = self.read_msg()?;
            match tag {
                b'T' => result.columns = row_description_names(&body),
                b'D' => result.rows.push(all_cells(&body)),
                b'C' => result.tag = command_tag(&body),
                b'E' => err = Some(parse_error(&body)),
                b'Z' => return Ok(err),
                _ => {} // ParameterStatus, NoData, etc.: ignore
            }
        }
    }

    /// Read until `ReadyForQuery`, calling `on_row` with all cells of each
    /// `DataRow`, returning any `ErrorResponse`'s `(SQLSTATE, message)`.
    fn collect_rows_to_ready(
        &mut self,
        on_row: &mut dyn FnMut(Vec<Option<String>>),
    ) -> io::Result<Option<(String, String)>> {
        let mut err = None;
        loop {
            let (tag, body) = self.read_msg()?;
            match tag {
                b'D' => on_row(all_cells(&body)),
                b'E' => err = Some(parse_error(&body)),
                b'Z' => return Ok(err),
                _ => {}
            }
        }
    }
}

/// Parse every column of a `DataRow` body into text cells (`None` = SQL NULL).
fn all_cells(body: &[u8]) -> Vec<Option<String>> {
    let mut cells = Vec::new();
    if body.len() < 2 {
        return cells;
    }
    let ncols = i16::from_be_bytes([body[0], body[1]]).max(0) as usize;
    let mut pos = 2;
    for _ in 0..ncols {
        if pos + 4 > body.len() {
            break;
        }
        let len = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        if len < 0 {
            cells.push(None);
            continue;
        }
        let end = pos + len as usize;
        if end > body.len() {
            break;
        }
        cells.push(Some(String::from_utf8_lossy(&body[pos..end]).into_owned()));
        pos = end;
    }
    cells
}

/// Parse a `RowDescription` ('T') body into the field names. Layout: int16 field
/// count, then per field a NUL-terminated name followed by 18 fixed bytes
/// (tableOID, colno, typeOID, typlen, typmod, format) we don't need.
fn row_description_names(body: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    if body.len() < 2 {
        return names;
    }
    let nfields = i16::from_be_bytes([body[0], body[1]]).max(0) as usize;
    let mut pos = 2;
    for _ in 0..nfields {
        let start = pos;
        while pos < body.len() && body[pos] != 0 {
            pos += 1;
        }
        if pos >= body.len() {
            break;
        }
        names.push(String::from_utf8_lossy(&body[start..pos]).into_owned());
        pos += 1; // skip NUL
        pos += 18; // tableOID(4) + colno(2) + typeOID(4) + typlen(2) + typmod(4) + format(2)
        if pos > body.len() {
            break;
        }
    }
    names
}

/// Decode a `CommandComplete` ('C') body — a single NUL-terminated command tag.
fn command_tag(body: &[u8]) -> String {
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    String::from_utf8_lossy(&body[..end]).into_owned()
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
