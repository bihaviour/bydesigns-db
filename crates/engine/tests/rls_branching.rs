//! Phase 7 — Row-Level Security security gate (P7-5): bypass-resistance across a
//! second engine handle and a debug branch, branch policy divergence, and
//! PITR-style restoration across a policy's creation LSN. RLS rides the same WAL
//! the rows do, so policies branch and restore for free (spec 17).

use engine::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn db_path(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("twill-rlsb-{tag}-{}-{n}.db", std::process::id()));
    let _ = fs::remove_file(&p);
    p
}

fn open(p: &Path) -> Connection {
    Connection::open(&format!("file://{}", p.display())).unwrap()
}

fn seed(db: &mut Connection) {
    db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, owner TEXT)")
        .unwrap();
    db.exec("INSERT INTO notes VALUES (1,'42'),(2,'99')")
        .unwrap();
    db.exec("ALTER TABLE notes ENABLE ROW LEVEL SECURITY")
        .unwrap();
    db.exec("CREATE POLICY p ON notes FOR ALL TO authenticated USING (owner = auth.uid())")
        .unwrap();
}

fn count(db: &mut Connection) -> i64 {
    db.query("SELECT count(*) FROM notes").unwrap().rows[0][0]
        .render()
        .unwrap()
        .parse()
        .unwrap()
}

/// Enforcement is at the executor, the lowest chokepoint: a *second* engine
/// handle to the same database (a different "client") is still filtered — it does
/// not inherit the first connection's principal and cannot read past the policy
/// (P7-5 bypass-resistance).
#[test]
fn a_second_handle_cannot_bypass() {
    let p = db_path("second");
    let mut a = open(&p);
    seed(&mut a);
    a.exec("SET ROLE authenticated").unwrap();
    a.exec(r#"SET twill.jwt.claims = '{"sub":"42"}'"#).unwrap();
    assert_eq!(count(&mut a), 1); // a sees only its own row

    // A fresh handle (the "attacker" with no SETs) is anon → default-deny.
    let mut b = open(&p);
    assert_eq!(count(&mut b), 0);
}

/// A debug-opened branch enforces RLS too — the policy rode the WAL into the
/// branch, and the branch handle has its own (default) principal, so an
/// unfiltered read via the branch stays filtered (P7-5 bypass-resistance).
#[test]
fn a_debug_branch_cannot_bypass() {
    let p = db_path("branchbypass");
    let mut main = open(&p);
    seed(&mut main);

    let mut branch = main.branch("debug").unwrap();
    // The branch inherited the policy; its anon principal sees nothing.
    assert!(branch.rls_enabled("notes"));
    assert_eq!(count(&mut branch), 0);

    // With a matching principal on the branch, the policy still filters.
    branch.exec("SET ROLE authenticated").unwrap();
    branch
        .exec(r#"SET twill.jwt.claims = '{"sub":"99"}'"#)
        .unwrap();
    assert_eq!(count(&mut branch), 1);
}

/// A branch can diverge its policy set without touching the parent (P7-5).
#[test]
fn branch_policy_divergence() {
    let p = db_path("divergence");
    let mut main = open(&p);
    seed(&mut main);

    let mut branch = main.branch("b").unwrap();
    assert_eq!(branch.policies().len(), 1);

    // Diverge: drop the policy on the branch only.
    branch.exec("DROP POLICY p ON notes").unwrap();
    assert!(branch.policies().is_empty());

    // The parent is unchanged.
    assert_eq!(main.policies().len(), 1);
    assert_eq!(main.policies()[0].name, "p");
}

/// PITR: a branch forked *before* the policy's creation LSN does not have the
/// policy; one forked *after* does — the policy is an LSN-stamped catalog fact, so
/// rewinding to a point-in-time restores the policy set in effect then (P7-5).
#[test]
fn pitr_restores_policy_set_at_lsn() {
    let p = db_path("pitr");
    let mut main = open(&p);
    main.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, owner TEXT)")
        .unwrap();
    main.exec("INSERT INTO notes VALUES (1,'42'),(2,'99')")
        .unwrap();

    // Fork before any policy exists.
    let mut before = main.branch("before").unwrap();

    // Now enable RLS + create the policy on main.
    main.exec("ALTER TABLE notes ENABLE ROW LEVEL SECURITY")
        .unwrap();
    main.exec("CREATE POLICY p ON notes FOR ALL TO authenticated USING (owner = auth.uid())")
        .unwrap();

    // Fork after the policy exists.
    let mut after = main.branch("after").unwrap();

    // The "before" branch has no policy and RLS off → all rows visible.
    assert!(!before.rls_enabled("notes"));
    assert!(before.policies().is_empty());
    assert_eq!(count(&mut before), 2);

    // The "after" branch carries the policy → default-deny for anon.
    assert!(after.rls_enabled("notes"));
    assert_eq!(after.policies().len(), 1);
    assert_eq!(count(&mut after), 0);
}
