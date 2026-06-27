//! End-to-end protocol tests: bind an ephemeral port, run the real listener, and
//! drive it with a minimal in-test Postgres client over a TCP socket. Exercises
//! both the simple and the extended (parameterized) query paths against the live
//! engine, plus an error round-trip.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

fn unique_db() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-srv-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    format!("file://{}", p.display())
}

/// Start the server on an ephemeral port; return the bound address.
fn start_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let db = unique_db();
    thread::spawn(move || {
        let _ = twill_server::serve_listener(listener, &db);
    });
    addr
}

// ---- a tiny pg client (enough to test our subset) -------------------------

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
        // StartupMessage: len, protocol 3.0, "user"\0"test"\0 "database"\0"db"\0 \0
        let mut body = Vec::new();
        body.extend_from_slice(&196608i32.to_be_bytes());
        for (k, v) in [("user", "test"), ("database", "db")] {
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut msg = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        msg.extend_from_slice(&body);
        self.stream.write_all(&msg).unwrap();
        self.read_until_ready(); // consume auth + params + ReadyForQuery
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

    /// Collect messages until `ReadyForQuery`, returning them all.
    fn read_until_ready(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut msgs = Vec::new();
        loop {
            let (tag, body) = self.read_msg();
            if tag == b'Z' {
                msgs.push((tag, body));
                return msgs;
            }
            msgs.push((tag, body));
        }
    }

    fn simple(&mut self, sql: &str) -> Vec<(u8, Vec<u8>)> {
        let mut p = sql.as_bytes().to_vec();
        p.push(0);
        self.send(b'Q', &p);
        self.read_until_ready()
    }
}

/// Extract the text cells of every DataRow ('D') in a message stream.
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

fn command_tags(msgs: &[(u8, Vec<u8>)]) -> Vec<String> {
    msgs.iter()
        .filter(|(t, _)| *t == b'C')
        .map(|(_, b)| String::from_utf8_lossy(&b[..b.len() - 1]).into_owned())
        .collect()
}

fn has_error(msgs: &[(u8, Vec<u8>)]) -> bool {
    msgs.iter().any(|(t, _)| *t == b'E')
}

#[test]
fn simple_query_protocol_roundtrip() {
    let addr = start_server();
    let mut c = Client::connect(&addr);

    let r = c.simple("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)");
    assert_eq!(command_tags(&r), vec!["CREATE TABLE"], "{:?}", r);

    let r = c.simple("INSERT INTO t VALUES (1, 'ada'), (2, 'bel')");
    assert_eq!(command_tags(&r), vec!["INSERT 0 2"]);

    let r = c.simple("SELECT id, name FROM t ORDER BY id");
    let rows = data_rows(&r);
    assert_eq!(
        rows,
        vec![
            vec![Some("1".into()), Some("ada".into())],
            vec![Some("2".into()), Some("bel".into())],
        ]
    );
    assert_eq!(command_tags(&r), vec!["SELECT 2"]);
}

#[test]
fn null_is_a_null_column() {
    let addr = start_server();
    let mut c = Client::connect(&addr);
    c.simple("CREATE TABLE n (id INTEGER PRIMARY KEY, body TEXT)");
    c.simple("INSERT INTO n VALUES (1, NULL)");
    let r = c.simple("SELECT body FROM n");
    assert_eq!(data_rows(&r), vec![vec![None]]);
}

#[test]
fn introspection_version_is_answered() {
    let addr = start_server();
    let mut c = Client::connect(&addr);
    let r = c.simple("select version()");
    let rows = data_rows(&r);
    assert_eq!(rows.len(), 1);
    assert!(rows[0][0].as_deref().unwrap().contains("twill-db"));
}

#[test]
fn error_response_then_recovers() {
    let addr = start_server();
    let mut c = Client::connect(&addr);
    let r = c.simple("this is not valid sql");
    assert!(has_error(&r), "expected ErrorResponse, got {r:?}");
    // The connection is still usable after an error + ReadyForQuery.
    let r = c.simple("select version()");
    assert!(!has_error(&r));
}

#[test]
fn extended_query_with_parameter() {
    let addr = start_server();
    let mut c = Client::connect(&addr);
    c.simple("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)");
    c.simple("INSERT INTO u VALUES (1, 'ada'), (2, 'bel')");

    // Parse: SELECT name FROM u WHERE id = $1  (Postgres placeholder syntax)
    let mut p = Vec::new();
    p.push(0); // unnamed statement
    p.extend_from_slice(b"SELECT name FROM u WHERE id = $1\0");
    p.extend_from_slice(&0i16.to_be_bytes()); // 0 declared param types
    c.send(b'P', &p);

    // Bind: param "2" (text format), default result formats
    let mut b = Vec::new();
    b.push(0); // portal
    b.push(0); // statement
    b.extend_from_slice(&0i16.to_be_bytes()); // 0 param format codes => text
    b.extend_from_slice(&1i16.to_be_bytes()); // 1 param
    let val = b"2";
    b.extend_from_slice(&(val.len() as i32).to_be_bytes());
    b.extend_from_slice(val);
    b.extend_from_slice(&0i16.to_be_bytes()); // 0 result format codes => text
    c.send(b'B', &b);

    // Describe portal, Execute, Sync
    c.send(b'D', &[b'P', 0]);
    let mut e = Vec::new();
    e.push(0); // portal
    e.extend_from_slice(&0i32.to_be_bytes()); // no row limit
    c.send(b'E', &e);
    c.send(b'S', &[]);

    let msgs = c.read_until_ready();
    let rows = data_rows(&msgs);
    assert_eq!(rows, vec![vec![Some("bel".into())]], "{msgs:?}");
    assert_eq!(command_tags(&msgs), vec!["SELECT 1"]);
}

/// Phase 7 (P7-5 composition): row-level security is enforced over the wire. The
/// principal is set by the client via `SET ROLE` / `SET twill.jwt.claims` (identity
/// composed at the boundary) and the engine — the chokepoint — filters rows. A
/// fresh connection that never set the principal is default-denied.
#[test]
fn rls_enforced_over_pgwire() {
    let addr = start_server();
    let mut c = Client::connect(&addr);

    c.simple("CREATE TABLE notes (id INTEGER PRIMARY KEY, owner TEXT)");
    c.simple("INSERT INTO notes VALUES (1,'42'),(2,'99'),(3,'42')");
    c.simple("ALTER TABLE notes ENABLE ROW LEVEL SECURITY");
    let r =
        c.simple("CREATE POLICY p ON notes FOR ALL TO authenticated USING (owner = auth.uid())");
    assert!(!has_error(&r), "CREATE POLICY failed: {r:?}");

    // Still anon on this connection → default-deny.
    let r = c.simple("SELECT id FROM notes ORDER BY id");
    assert_eq!(data_rows(&r), Vec::<Vec<Option<String>>>::new());

    // Set the principal over the wire; the SET must reach the engine.
    let r = c.simple("SET ROLE authenticated");
    assert_eq!(command_tags(&r), vec!["SET"], "{r:?}");
    assert!(!has_error(&r));
    let r = c.simple(r#"SET twill.jwt.claims = '{"sub":"42"}'"#);
    assert!(!has_error(&r), "{r:?}");

    // Now only the principal's own rows are visible.
    let r = c.simple("SELECT id FROM notes ORDER BY id");
    assert_eq!(
        data_rows(&r),
        vec![vec![Some("1".into())], vec![Some("3".into())]],
        "{r:?}"
    );

    // A second connection that never set the principal is still default-denied —
    // enforcement is in the engine, not a layer the first client configured.
    let mut c2 = Client::connect(&addr);
    let r = c2.simple("SELECT id FROM notes");
    assert_eq!(data_rows(&r), Vec::<Vec<Option<String>>>::new(), "{r:?}");
}
