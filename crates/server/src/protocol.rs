//! Postgres frontend/backend protocol 3.0 codec (the supported subset, spec 07).
//!
//! Hand-rolled framing, deliberately: it keeps the server dependency-free (the
//! project rule — hand-rolled WAL codec, base64, object codecs) and the subset
//! is small. The spec's SHOULD ("start from `pgwire`") is a guidance to not
//! hand-roll *blindly*; the message shapes below follow the protocol exactly.
//!
//! Wire shape: every backend/most frontend messages are `type:u8` + `len:i32`
//! (length includes the 4 length bytes) + payload. The startup packet is the one
//! exception — it has no type byte.

use std::io::{self, Read, Write};

// ---- frontend (client → server) -------------------------------------------

/// What the very first bytes on a connection asked for.
pub enum Startup {
    /// `SSLRequest` — reply one byte `N` (we do not terminate TLS in-process)
    /// and read the real startup packet next.
    SslRequest,
    /// `GSSENCRequest` — same handling as SSL: decline, then read startup.
    GssRequest,
    /// The real `StartupMessage` with its parameter map (`user`, `database`, …).
    Params(Vec<(String, String)>),
    /// A `CancelRequest`; we accept and ignore it (no out-of-band cancel yet).
    Cancel,
}

/// A decoded frontend message from the steady-state command loop.
pub enum Frontend {
    Query(String),
    Parse {
        name: String,
        sql: String,
        param_oids: Vec<i32>,
    },
    Bind {
        portal: String,
        stmt: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    Describe {
        kind: u8, // b'S' statement or b'P' portal
        name: String,
    },
    Execute {
        portal: String,
        max_rows: i32,
    },
    Sync,
    Flush,
    Close {
        kind: u8,
        name: String,
    },
    Terminate,
    /// A message type we do not implement; the session replies with an error.
    Unsupported(u8),
}

/// Read the startup packet (or SSL/GSS/cancel probe).
pub fn read_startup(r: &mut impl Read) -> io::Result<Startup> {
    let len = read_i32(r)?;
    if !(8..=1 << 20).contains(&len) {
        return Err(invalid("implausible startup length"));
    }
    let mut body = vec![0u8; (len - 4) as usize];
    r.read_exact(&mut body)?;
    let code = i32::from_be_bytes(body[0..4].try_into().unwrap());
    match code {
        80877103 => Ok(Startup::SslRequest),
        80877104 => Ok(Startup::Cancel),
        80877105 => Ok(Startup::GssRequest),
        196608 => {
            // protocol 3.0: NUL-terminated key/value pairs after the version.
            let mut params = Vec::new();
            let kv = &body[4..];
            let mut it = kv.split(|b| *b == 0);
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                if k.is_empty() {
                    break;
                }
                params.push((
                    String::from_utf8_lossy(k).into_owned(),
                    String::from_utf8_lossy(v).into_owned(),
                ));
            }
            Ok(Startup::Params(params))
        }
        other => Err(invalid(&format!(
            "unsupported protocol/startup code {other}"
        ))),
    }
}

/// Read one steady-state frontend message. `Ok(None)` on clean EOF.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<Frontend>> {
    let mut tag = [0u8; 1];
    match r.read_exact(&mut tag) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = read_i32(r)?;
    if !(4..=1 << 30).contains(&len) {
        return Err(invalid("implausible message length"));
    }
    let mut body = vec![0u8; (len - 4) as usize];
    r.read_exact(&mut body)?;
    let mut c = Cursor::new(&body);

    let msg = match tag[0] {
        b'Q' => Frontend::Query(c.cstr()?),
        b'P' => {
            let name = c.cstr()?;
            let sql = c.cstr()?;
            let n = c.i16()? as usize;
            let mut param_oids = Vec::with_capacity(n);
            for _ in 0..n {
                param_oids.push(c.i32()?);
            }
            Frontend::Parse {
                name,
                sql,
                param_oids,
            }
        }
        b'B' => {
            let portal = c.cstr()?;
            let stmt = c.cstr()?;
            let nf = c.i16()? as usize;
            let mut param_formats = Vec::with_capacity(nf);
            for _ in 0..nf {
                param_formats.push(c.i16()?);
            }
            let np = c.i16()? as usize;
            let mut params = Vec::with_capacity(np);
            for _ in 0..np {
                let plen = c.i32()?;
                if plen < 0 {
                    params.push(None);
                } else {
                    params.push(Some(c.take(plen as usize)?));
                }
            }
            let nr = c.i16()? as usize;
            let mut result_formats = Vec::with_capacity(nr);
            for _ in 0..nr {
                result_formats.push(c.i16()?);
            }
            Frontend::Bind {
                portal,
                stmt,
                param_formats,
                params,
                result_formats,
            }
        }
        b'D' => {
            let kind = c.u8()?;
            let name = c.cstr()?;
            Frontend::Describe { kind, name }
        }
        b'E' => {
            let portal = c.cstr()?;
            let max_rows = c.i32()?;
            Frontend::Execute { portal, max_rows }
        }
        b'S' => Frontend::Sync,
        b'H' => Frontend::Flush,
        b'C' => {
            let kind = c.u8()?;
            let name = c.cstr()?;
            Frontend::Close { kind, name }
        }
        b'X' => Frontend::Terminate,
        other => Frontend::Unsupported(other),
    };
    Ok(Some(msg))
}

// ---- backend (server → client) builders -----------------------------------

/// Accumulates backend messages, flushed to the socket in one write.
#[derive(Default)]
pub struct Out {
    buf: Vec<u8>,
}

impl Out {
    pub fn new() -> Out {
        Out::default()
    }

    /// Append a framed message: `tag` + length + `payload`.
    fn msg(&mut self, tag: u8, payload: &[u8]) {
        self.buf.push(tag);
        self.buf
            .extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
        self.buf.extend_from_slice(payload);
    }

    pub fn auth_ok(&mut self) {
        self.msg(b'R', &0i32.to_be_bytes());
    }

    pub fn parameter_status(&mut self, name: &str, value: &str) {
        let mut p = Vec::new();
        push_cstr(&mut p, name);
        push_cstr(&mut p, value);
        self.msg(b'S', &p);
    }

    pub fn backend_key_data(&mut self, pid: i32, key: i32) {
        let mut p = Vec::new();
        p.extend_from_slice(&pid.to_be_bytes());
        p.extend_from_slice(&key.to_be_bytes());
        self.msg(b'K', &p);
    }

    /// `ReadyForQuery` with transaction status (`I` idle / `T` in-txn / `E` failed).
    pub fn ready_for_query(&mut self, status: u8) {
        self.msg(b'Z', &[status]);
    }

    /// `RowDescription`. Each field: `(name, type_oid, type_len, format)`.
    pub fn row_description(&mut self, fields: &[(String, i32, i16, i16)]) {
        let mut p = Vec::new();
        p.extend_from_slice(&(fields.len() as i16).to_be_bytes());
        for (name, oid, len, fmt) in fields {
            push_cstr(&mut p, name);
            p.extend_from_slice(&0i32.to_be_bytes()); // table OID
            p.extend_from_slice(&0i16.to_be_bytes()); // column attr number
            p.extend_from_slice(&oid.to_be_bytes());
            p.extend_from_slice(&len.to_be_bytes());
            p.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
            p.extend_from_slice(&fmt.to_be_bytes());
        }
        self.msg(b'T', &p);
    }

    /// `DataRow`. `None` encodes a SQL NULL (length -1).
    pub fn data_row(&mut self, cols: &[Option<Vec<u8>>]) {
        let mut p = Vec::new();
        p.extend_from_slice(&(cols.len() as i16).to_be_bytes());
        for col in cols {
            match col {
                None => p.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(bytes) => {
                    p.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    p.extend_from_slice(bytes);
                }
            }
        }
        self.msg(b'D', &p);
    }

    pub fn command_complete(&mut self, tag: &str) {
        let mut p = Vec::new();
        push_cstr(&mut p, tag);
        self.msg(b'C', &p);
    }

    pub fn empty_query_response(&mut self) {
        self.msg(b'I', &[]);
    }

    pub fn parse_complete(&mut self) {
        self.msg(b'1', &[]);
    }
    pub fn bind_complete(&mut self) {
        self.msg(b'2', &[]);
    }
    pub fn close_complete(&mut self) {
        self.msg(b'3', &[]);
    }
    pub fn no_data(&mut self) {
        self.msg(b'n', &[]);
    }

    /// `ParameterDescription`: the OID of each parameter.
    pub fn parameter_description(&mut self, oids: &[i32]) {
        let mut p = Vec::new();
        p.extend_from_slice(&(oids.len() as i16).to_be_bytes());
        for oid in oids {
            p.extend_from_slice(&oid.to_be_bytes());
        }
        self.msg(b't', &p);
    }

    /// `ErrorResponse` with the minimum field set: severity, SQLSTATE, message.
    pub fn error(&mut self, severity: &str, code: &str, message: &str) {
        let mut p = Vec::new();
        p.push(b'S');
        push_cstr(&mut p, severity);
        p.push(b'V');
        push_cstr(&mut p, severity);
        p.push(b'C');
        push_cstr(&mut p, code);
        p.push(b'M');
        push_cstr(&mut p, message);
        p.push(0); // field terminator
        self.msg(b'E', &p);
    }

    pub fn flush_to(&mut self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&self.buf)?;
        w.flush()?;
        self.buf.clear();
        Ok(())
    }
}

// ---- small read helpers ----------------------------------------------------

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Cursor<'a> {
        Cursor { buf, pos: 0 }
    }
    fn take(&mut self, n: usize) -> io::Result<Vec<u8>> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.buf.len())
            .ok_or_else(|| invalid("message truncated"))?;
        let v = self.buf[self.pos..end].to_vec();
        self.pos = end;
        Ok(v)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i16(&mut self) -> io::Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn cstr(&mut self) -> io::Result<String> {
        let start = self.pos;
        while self.pos < self.buf.len() && self.buf[self.pos] != 0 {
            self.pos += 1;
        }
        if self.pos >= self.buf.len() {
            return Err(invalid("unterminated string"));
        }
        let s = String::from_utf8_lossy(&self.buf[start..self.pos]).into_owned();
        self.pos += 1; // skip NUL
        Ok(s)
    }
}

fn read_i32(r: &mut impl Read) -> io::Result<i32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

fn push_cstr(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}
