//! Phase 7 — Row-Level Security: write-path enforcement (P7-4). `WITH CHECK` on
//! INSERT, `USING` + `WITH CHECK` on UPDATE/DELETE, RLS-filtered `RETURNING`, and
//! the explicit privileged bypass (spec 17 / #88).

use engine::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-rlsw-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn open(p: &Path) -> Connection {
    Connection::open(&format!("file://{}", p.display())).unwrap()
}

/// A `notes` table with RLS on and an owner-scoped `ALL` policy; the connection is
/// authenticated as uid `42`.
fn as_owner_42(p: &Path) -> Connection {
    let mut db = open(p);
    db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, owner TEXT, body TEXT)")
        .unwrap();
    db.exec("ALTER TABLE notes ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.exec(
        "CREATE POLICY p ON notes FOR ALL TO authenticated \
         USING (owner = auth.uid()) WITH CHECK (owner = auth.uid())",
    )
    .unwrap();
    db.exec("SET ROLE authenticated").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"42"}'"#).unwrap();
    db
}

/// `WITH CHECK` admits a row the principal owns and rejects one it does not (P7-4).
#[test]
fn with_check_gates_insert() {
    let p = db_path("insert");
    let mut db = as_owner_42(&p);

    db.exec("INSERT INTO notes VALUES (1,'42','mine')").unwrap();

    let err = db
        .exec("INSERT INTO notes VALUES (2,'99','not mine')")
        .unwrap_err();
    assert_eq!(err.status, engine::EngineStatus::ErrConstraint);

    // Only the admitted row exists.
    db.exec("SET twill.rls.bypass = on").unwrap();
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("1"));
}

/// A write denied by `WITH CHECK` emits no WAL record — it is absent after
/// recovery (P7-4 / P7-5 denied-write durability).
#[test]
fn denied_insert_never_reaches_the_wal() {
    let p = db_path("durable");
    {
        let mut db = as_owner_42(&p);
        let err = db
            .exec("INSERT INTO notes VALUES (7,'99','nope')")
            .unwrap_err();
        assert_eq!(err.status, engine::EngineStatus::ErrConstraint);
    } // drop → release fence

    // Reopen: replay the WAL. The denied row must be absent (bypass to see all).
    let mut db = open(&p);
    db.exec("SET twill.rls.bypass = on").unwrap();
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("0"));
}

/// UPDATE only touches rows the `USING` predicate admits, and the new row must
/// pass `WITH CHECK` (P7-4).
#[test]
fn update_respects_using_and_check() {
    let p = db_path("update");
    let mut db = as_owner_42(&p);
    // Seed two owners via the bypass, then enforce as uid 42.
    db.exec("SET twill.rls.bypass = on").unwrap();
    db.exec("INSERT INTO notes VALUES (1,'42','a'),(2,'99','b')")
        .unwrap();
    db.exec("SET twill.rls.bypass = off").unwrap();

    // Updating someone else's row is a no-op (USING excludes it).
    db.exec("UPDATE notes SET body = 'hax' WHERE id = 2")
        .unwrap();
    assert_eq!(db.last_changes, 0);

    // Updating my own row works.
    db.exec("UPDATE notes SET body = 'edited' WHERE id = 1")
        .unwrap();
    assert_eq!(db.last_changes, 1);

    // Re-assigning ownership away from myself violates WITH CHECK → rejected.
    let err = db
        .exec("UPDATE notes SET owner = '99' WHERE id = 1")
        .unwrap_err();
    assert_eq!(err.status, engine::EngineStatus::ErrConstraint);
}

/// DELETE only removes rows the `USING` predicate admits (P7-4).
#[test]
fn delete_respects_using() {
    let p = db_path("delete");
    let mut db = as_owner_42(&p);
    db.exec("SET twill.rls.bypass = on").unwrap();
    db.exec("INSERT INTO notes VALUES (1,'42','a'),(2,'99','b')")
        .unwrap();
    db.exec("SET twill.rls.bypass = off").unwrap();

    db.exec("DELETE FROM notes WHERE id = 2").unwrap();
    assert_eq!(db.last_changes, 0); // not mine → untouched

    db.exec("DELETE FROM notes WHERE id = 1").unwrap();
    assert_eq!(db.last_changes, 1);
}

/// `RETURNING` is filtered by the SELECT policy, so an insert cannot surface a row
/// the principal could not read — even though the row was written (P7-4).
#[test]
fn returning_is_rls_filtered() {
    let p = db_path("returning");
    let mut db = open(&p);
    db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, owner TEXT)")
        .unwrap();
    db.exec("ALTER TABLE notes ENABLE ROW LEVEL SECURITY")
        .unwrap();
    // A SELECT policy that restricts visibility, and a permissive INSERT policy so
    // both rows are admitted.
    db.exec("CREATE POLICY r ON notes FOR SELECT TO authenticated USING (owner = auth.uid())")
        .unwrap();
    db.exec("CREATE POLICY w ON notes FOR INSERT TO authenticated WITH CHECK (true)")
        .unwrap();
    db.exec("SET ROLE authenticated").unwrap();
    db.exec(r#"SET twill.jwt.claims = '{"sub":"42"}'"#).unwrap();

    let rs = db
        .query("INSERT INTO notes VALUES (1,'99'),(2,'42') RETURNING id")
        .unwrap();
    // Both rows written, but only the owned one is returned.
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("2"));
    assert_eq!(db.last_changes, 2);
}

/// The explicit, off-by-default bypass sees and writes every row; turning it off
/// returns to the filtered view (P7-4). It is never inferred from a role name.
#[test]
fn explicit_bypass_sees_all_then_filters_again() {
    let p = db_path("bypass");
    let mut db = as_owner_42(&p);
    db.exec("SET twill.rls.bypass = on").unwrap();
    db.exec("INSERT INTO notes VALUES (1,'42','a'),(2,'99','b')")
        .unwrap();
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("2"));

    db.exec("SET twill.rls.bypass = off").unwrap();
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("1")); // only owner=42

    // Merely naming a role does NOT bypass — service_role with no explicit flag
    // is still filtered (bypass is never inferred from a role name).
    db.exec("SET ROLE service_role").unwrap();
    let rs = db.query("SELECT count(*) FROM notes").unwrap();
    assert_eq!(rs.rows[0][0].render().as_deref(), Some("0")); // no policy for service_role
}
