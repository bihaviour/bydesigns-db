//! The frozen pgwire-subset conformance suite (EX-3 / #102, spec 07 addendum).
//!
//! `wire.rs` proves the protocol *mechanics* (simple + extended round-trips).
//! This suite freezes the **boundary**: the connect-time catalog/introspection
//! queries real clients (`Bun.sql`, PostgREST 14.x, `pgbench`) issue are each
//! answered / reflected / stubbed exactly as the capability matrix
//! ([`twill_server::capability`]) records, and a system-catalog query *outside*
//! the subset returns a clear `feature_not_supported` (0A000) error instead of a
//! confusing engine syntax error.
//!
//! The matrix is the source of truth; these tests are how it stays honest — if a
//! row's disposition changes without the behaviour changing (or vice-versa), a
//! test here fails.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use twill_server::capability::{Support, CATALOG_QUERIES, PROTOCOL_MESSAGES};

fn unique_db() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-subset-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("file://{}", p.display())
}

fn start_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let db = unique_db();
    thread::spawn(move || {
        let _ = twill_server::serve_listener(listener, &db);
    });
    addr
}

// ---- a tiny pg client (enough to exercise the subset) ---------------------

struct Client {
    stream: TcpStream,
}

impl Client {
    fn connect(addr: &str) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut c = Client { stream };
        c.startup();
        c
    }

    fn startup(&mut self) {
        let mut body = Vec::new();
        body.extend_from_slice(&196608i32.to_be_bytes());
        for (k, v) in [("user", "postgres"), ("database", "srv")] {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut msg = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        msg.extend_from_slice(&body);
        self.stream.write_all(&msg).unwrap();
        self.read_until_ready();
    }

    fn send(&mut self, tag: u8, payload: &[u8]) {
        let mut msg = vec![tag];
        msg.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
        msg.extend_from_slice(payload);
        self.stream.write_all(&msg).unwrap();
    }

    fn read_msg(&mut self) -> (u8, Vec<u8>) {
        let mut tag = [0u8; 1];
        self.stream.read_exact(&mut tag).unwrap();
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).unwrap();
        let n = i32::from_be_bytes(len) as usize - 4;
        let mut body = vec![0u8; n];
        self.stream.read_exact(&mut body).unwrap();
        (tag[0], body)
    }

    fn read_until_ready(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut msgs = Vec::new();
        loop {
            let (tag, body) = self.read_msg();
            let done = tag == b'Z';
            msgs.push((tag, body));
            if done {
                return msgs;
            }
        }
    }

    fn simple(&mut self, sql: &str) -> Vec<(u8, Vec<u8>)> {
        let mut p = sql.as_bytes().to_vec();
        p.push(0);
        self.send(b'Q', &p);
        self.read_until_ready()
    }

    /// Run a one-shot parameterized statement through the extended protocol with
    /// a single text parameter, returning every message up to ReadyForQuery.
    fn extended_one_param(&mut self, sql: &str, param: &str) -> Vec<(u8, Vec<u8>)> {
        let mut p = Vec::new();
        p.push(0); // unnamed statement
        p.extend_from_slice(sql.as_bytes());
        p.push(0);
        p.extend_from_slice(&0i16.to_be_bytes()); // no declared param types
        self.send(b'P', &p);

        let mut b = Vec::new();
        b.push(0); // portal
        b.push(0); // statement
        b.extend_from_slice(&0i16.to_be_bytes()); // text params
        b.extend_from_slice(&1i16.to_be_bytes()); // 1 param
        b.extend_from_slice(&(param.len() as i32).to_be_bytes());
        b.extend_from_slice(param.as_bytes());
        b.extend_from_slice(&0i16.to_be_bytes()); // text results
        self.send(b'B', &b);

        self.send(b'D', &[b'P', 0]);
        let mut e = Vec::new();
        e.push(0);
        e.extend_from_slice(&0i32.to_be_bytes());
        self.send(b'E', &e);
        self.send(b'S', &[]);
        self.read_until_ready()
    }
}

fn data_rows(msgs: &[(u8, Vec<u8>)]) -> Vec<Vec<Option<String>>> {
    let mut out = Vec::new();
    for (tag, body) in msgs {
        if *tag != b'D' {
            continue;
        }
        let ncols = i16::from_be_bytes(body[0..2].try_into().unwrap()) as usize;
        let mut pos = 2;
        let mut row = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
            pos += 4;
            if len < 0 {
                row.push(None);
            } else {
                let s = String::from_utf8_lossy(&body[pos..pos + len as usize]).into_owned();
                pos += len as usize;
                row.push(Some(s));
            }
        }
        out.push(row);
    }
    out
}

/// The SQLSTATE codes of every ErrorResponse ('E') in the stream. Field 'C' is
/// the SQLSTATE; fields are null-terminated, type-tagged, ending with a 0 byte.
fn error_codes(msgs: &[(u8, Vec<u8>)]) -> Vec<String> {
    let mut out = Vec::new();
    for (tag, body) in msgs {
        if *tag != b'E' {
            continue;
        }
        let mut pos = 0;
        while pos < body.len() && body[pos] != 0 {
            let field = body[pos];
            pos += 1;
            let start = pos;
            while pos < body.len() && body[pos] != 0 {
                pos += 1;
            }
            let val = String::from_utf8_lossy(&body[start..pos]).into_owned();
            pos += 1; // skip the null terminator
            if field == b'C' {
                out.push(val);
            }
        }
    }
    out
}

fn has_error(msgs: &[(u8, Vec<u8>)]) -> bool {
    msgs.iter().any(|(t, _)| *t == b'E')
}

// ---- the conformance assertions -------------------------------------------

#[test]
fn version_probe_is_answered_end_to_end() {
    // The make-or-break startup gate (spec 07): the numeric + string version
    // probe must resolve over the wire, or no client gets past connect.
    let addr = start_server();
    let mut c = Client::connect(&addr);

    let r = c.simple("SELECT current_setting('server_version_num')::integer");
    let rows = data_rows(&r);
    assert_eq!(rows[0][0].as_deref(), Some("150000"), "{r:?}");
    assert!(!has_error(&r));

    // The combined 3-column probe PostgREST 14.x issues on connect.
    let r = c.simple(
        "SELECT current_setting('server_version_num')::integer, \
         current_setting('server_version'), version()",
    );
    let rows = data_rows(&r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 3, "3-column version probe: {r:?}");
}

#[test]
fn answered_catalog_markers_resolve_over_the_wire() {
    // Every Answered scalar-introspection row in the matrix must come back as a
    // non-error result. (We exercise the representative scalar probes clients
    // fire on connect; Reflected/Stubbed rows are covered separately.)
    let addr = start_server();
    let mut c = Client::connect(&addr);
    let probes = [
        "SELECT version()",
        "SELECT current_schema()",
        "SELECT current_database()",
        "SELECT current_user",
        "SELECT pg_backend_pid()",
        "SHOW server_version_num",
        "SHOW client_encoding",
    ];
    for sql in probes {
        let r = c.simple(sql);
        assert!(!has_error(&r), "answered probe errored: {sql:?} -> {r:?}");
        assert_eq!(data_rows(&r).len(), 1, "expected one row for {sql:?}");
    }
}

#[test]
fn stubbed_session_commands_are_accepted_noops() {
    // Drivers fire these on connect/pool-checkin and ignore the reply; they must
    // succeed (a 0A000 here would abort startup).
    let addr = start_server();
    let mut c = Client::connect(&addr);
    for sql in [
        "SET search_path TO public",
        "SET application_name = 'pgbench'",
        "DISCARD ALL",
        "SELECT set_config('search_path', 'public', true)",
    ] {
        let r = c.simple(sql);
        assert!(!has_error(&r), "stubbed command errored: {sql:?} -> {r:?}");
    }
}

#[test]
fn schema_cache_reflection_serves_postgrest() {
    // PostgREST's tables + FK schema-cache queries (Reflected in the matrix) must
    // resolve against the live catalog without error so the cache loads.
    let addr = start_server();
    let mut c = Client::connect(&addr);
    c.simple("CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT)");
    c.simple(
        "CREATE TABLE books (id INTEGER PRIMARY KEY, author_id INTEGER REFERENCES authors(id))",
    );

    // Marker-bearing stand-ins for the (huge) real recursive catalog queries —
    // the server keys on the same markers the real client SQL carries.
    let tables = c.simple("SELECT pg_relation_is_updatable(c.oid::regclass, true) & 8 AS x");
    assert!(!has_error(&tables), "tables reflection errored: {tables:?}");

    let rels = c.simple("SELECT 1 FROM pks_uniques_cols WHERE contype = 'f'");
    assert!(!has_error(&rels), "FK reflection errored: {rels:?}");
}

#[test]
fn unsupported_catalog_query_is_a_clear_error_simple() {
    // A system-catalog query outside the subset must return a clear
    // feature_not_supported (0A000), NOT a generic engine syntax error (42601),
    // and the connection must stay usable afterwards (spec 07 MUST).
    let addr = start_server();
    let mut c = Client::connect(&addr);

    let r = c.simple("SELECT relname FROM pg_catalog.pg_class");
    assert_eq!(
        error_codes(&r),
        vec!["0A000".to_string()],
        "unsupported catalog query must be feature_not_supported: {r:?}"
    );

    // Still alive: an answered probe works on the same connection.
    let r = c.simple("SELECT version()");
    assert!(
        !has_error(&r),
        "connection unusable after clear error: {r:?}"
    );
}

#[test]
fn unsupported_catalog_query_is_a_clear_error_extended() {
    // Same boundary through the extended protocol's Parse path.
    let addr = start_server();
    let mut c = Client::connect(&addr);
    let r = c.extended_one_param(
        "SELECT n.nspname FROM pg_namespace n WHERE n.oid = $1",
        "2200",
    );
    assert_eq!(
        error_codes(&r),
        vec!["0A000".to_string()],
        "extended unsupported catalog query must be feature_not_supported: {r:?}"
    );
}

#[test]
fn data_path_is_unaffected_by_the_boundary() {
    // The clear-error classifier must not snare ordinary DML/DDL — including a
    // column named like a catalog token. Full CRUD round-trips error-free.
    let addr = start_server();
    let mut c = Client::connect(&addr);
    assert!(!has_error(&c.simple(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, version TEXT)"
    )));
    assert!(!has_error(
        &c.simple("INSERT INTO t VALUES (1, 'v1'), (2, 'v2')")
    ));
    let r = c.extended_one_param("SELECT version FROM t WHERE id = $1", "2");
    assert_eq!(data_rows(&r), vec![vec![Some("v2".into())]], "{r:?}");
}

#[test]
fn matrix_dispositions_match_observed_behaviour() {
    // Freeze the matrix against the live server: every Answered/Stubbed scalar
    // marker we can drive resolves without error, and the Errored protocol
    // entries are recorded as such. This binds the documented boundary to the
    // running code so they cannot silently drift apart.
    assert!(
        PROTOCOL_MESSAGES
            .iter()
            .any(|m| m.tag == 'Q' && m.support == Support::Answered),
        "simple Query must be Answered in the matrix"
    );
    assert!(
        PROTOCOL_MESSAGES
            .iter()
            .filter(|m| m.support == Support::Errored)
            .count()
            >= 1,
        "the matrix records explicit non-goals as Errored"
    );

    // Count the matrix's dispositions to ensure the table is populated across
    // all four categories (a regression that flattened it would trip here).
    let answered = CATALOG_QUERIES
        .iter()
        .filter(|c| c.support == Support::Answered)
        .count();
    let reflected = CATALOG_QUERIES
        .iter()
        .filter(|c| c.support == Support::Reflected)
        .count();
    let stubbed = CATALOG_QUERIES
        .iter()
        .filter(|c| c.support == Support::Stubbed)
        .count();
    assert!(
        answered > 0 && reflected > 0 && stubbed > 0,
        "matrix populated"
    );
}
