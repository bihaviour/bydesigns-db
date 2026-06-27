//! Phase 7 — Row-Level Security: session context, `auth.*` accessors, policy
//! DDL + persistence, read-path enforcement (USING + default-deny), and
//! per-connection principal isolation (spec 17 / #88 sub-issues P7-1..P7-3).

use engine::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-rls-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn open(p: &Path) -> Connection {
    Connection::open(&format!("file://{}", p.display())).unwrap()
}

/// A `notes` table seeded with rows owned by two principals, RLS enabled, and a
/// `SELECT`/`ALL` policy restricting visibility to the row owner.
fn seeded(p: &Path) -> Connection {
    let mut db = open(p);
    db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, owner TEXT, body TEXT)")
        .unwrap();
    db.exec("INSERT INTO notes VALUES (1,'42','mine'),(2,'99','theirs'),(3,'42','also mine')")
        .unwrap();
    db.exec("ALTER TABLE notes ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.exec(
        "CREATE POLICY owner_can_read ON notes FOR ALL TO authenticated \
         USING (owner = auth.uid()) WITH CHECK (owner = auth.uid())",
    )
    .unwrap();
    db
}

/// `SET ROLE` + `SET twill.jwt.claims` flow through to `auth.uid()/role()/claim()`
/// (P7-1 acceptance).
#[test]
fn auth_accessors_read_the_session_principal() {
    let p = db_path("auth");
    let mut db = open(&p);
    db.exec("SET ROLE user").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"u_42","org":"acme"}'"#)
        .unwrap();
    let rs = db
        .query("SELECT auth.uid(), auth.role(), auth.claim('org')")
        .unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("u_42"));
    assert_eq!(rs.rows[0][1].render().as_deref(), Some("user"));
    assert_eq!(rs.rows[0][2].render().as_deref(), Some("acme"));
}

/// A connection that never ran the SETs has the default (anon) principal, and two
/// connections on the same `Database` never see each other's principal (P7-1).
#[test]
fn principal_is_per_connection() {
    let p = db_path("isolation");
    let mut a = open(&p);
    let mut b = open(&p); // same URL → shares the Database, not the session

    a.exec("SET ROLE user").unwrap();
    a.exec(r#"SET twill.jwt.claims = '{"sub":"u_42"}'"#)
        .unwrap();

    let ra = a.query("SELECT auth.role(), auth.uid()").unwrap();
    assert_eq!(ra.rows[0][0].render().as_deref(), Some("user"));
    assert_eq!(ra.rows[0][1].render().as_deref(), Some("u_42"));

    let rb = b.query("SELECT auth.role(), auth.uid()").unwrap();
    assert_eq!(rb.rows[0][0].render().as_deref(), Some("anon"));
    assert!(rb.rows[0][1].render().is_none()); // NULL uid for the anon principal
}

/// The `USING` predicate filters every read to the rows the principal owns; the
/// single-table fast path (P7-3).
#[test]
fn read_path_filters_by_using() {
    let p = db_path("read");
    let mut db = seeded(&p);
    db.exec("SET ROLE authenticated").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"42"}'"#).unwrap();

    let rs = db.query("SELECT id FROM notes ORDER BY id").unwrap();
    let ids: Vec<Option<String>> = rs.rows.iter().map(|r| r[0].render()).collect();
    assert_eq!(ids, vec![Some("1".into()), Some("3".into())]); // owner=42 only
}

/// The relational path (a self-join here) applies each base table's policy, and a
/// derived table inherits its source's policy (P7-3).
#[test]
fn read_path_filters_in_joins_and_derived_tables() {
    let p = db_path("join");
    let mut db = seeded(&p);
    db.exec("SET ROLE authenticated").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"42"}'"#).unwrap();

    // Self-join: only owner=42 rows survive on both sides.
    let rs = db
        .query(
            "SELECT a.id, b.id FROM notes a JOIN notes b ON a.owner = b.owner ORDER BY a.id, b.id",
        )
        .unwrap();
    for row in &rs.rows {
        assert!(matches!(row[0].render().as_deref(), Some("1") | Some("3")));
        assert!(matches!(row[1].render().as_deref(), Some("1") | Some("3")));
    }

    // Derived table inherits notes' policy.
    let rs = db
        .query("SELECT count(*) FROM (SELECT * FROM notes) sub")
        .unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("2"));
}

/// RLS enabled with no policy matching the active role behaves as `WHERE FALSE`
/// (default-deny, P7-3).
#[test]
fn default_deny_when_no_policy_matches() {
    let p = db_path("deny");
    let mut db = seeded(&p);
    // `anon` (no SET ROLE) has no matching policy → sees nothing.
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("0"));

    // An authenticated principal with a non-matching uid also sees nothing.
    db.exec("SET ROLE authenticated").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"nobody"}'"#)
        .unwrap();
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("0"));
}

/// Policies are durable catalog facts: they survive a process restart (WAL
/// replay) and reflect via `pg_policies` (P7-2 acceptance).
#[test]
fn policies_persist_across_restart() {
    let p = db_path("persist");
    {
        let _db = seeded(&p);
    } // drop → release fence; the Database leaves the registry

    let mut db = open(&p); // reopen → replay rebuilds the policy set
    assert!(db.rls_enabled("notes"));
    let policies = db.policies();
    assert_eq!(policies.len(), 1);
    let pol = &policies[0];
    assert_eq!(pol.name, "owner_can_read");
    assert_eq!(pol.command, "ALL");
    assert_eq!(pol.roles, vec!["authenticated".to_string()]);
    assert_eq!(pol.using.as_deref(), Some("owner = auth.uid()"));
    assert_eq!(pol.check.as_deref(), Some("owner = auth.uid()"));

    // Enforcement is still live after restart.
    db.exec("SET ROLE authenticated").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"99"}'"#).unwrap();
    let rs = db.query("SELECT id FROM notes").unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("2"));
}

/// `DROP POLICY` removes enforcement; the table is then default-deny (RLS still
/// enabled, no policy) (P7-2).
#[test]
fn drop_policy_returns_to_default_deny() {
    let p = db_path("droppol");
    let mut db = seeded(&p);
    db.exec("DROP POLICY owner_can_read ON notes").unwrap();
    assert!(db.policies().is_empty());

    db.exec("SET ROLE authenticated").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"42"}'"#).unwrap();
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("0"));
}

/// Policy DDL inside an explicit transaction is rejected (autocommit-only, P7-2).
#[test]
fn policy_ddl_is_autocommit_only() {
    let p = db_path("txn");
    let mut db = open(&p);
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, owner TEXT)")
        .unwrap();
    db.exec("BEGIN").unwrap();
    let err = db
        .exec("CREATE POLICY p ON t FOR ALL USING (owner = auth.uid())")
        .unwrap_err();
    assert_eq!(err.status, engine::EngineStatus::ErrTxn);
    db.exec("ROLLBACK").unwrap();
}
