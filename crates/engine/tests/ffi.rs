//! Exercises the C ABI surface directly (the same symbols `bun:ffi` binds),
//! including borrowed-pointer reads, SQL-NULL as a null pointer, prepared
//! cursors, error retrieval, and misuse handling.

use engine::ffi::*;
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

fn cs(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn rd(p: *const std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string())
    }
}

fn db_path() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!("bydesigns-ffi-{}-{n}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn c_abi_roundtrip() {
    let p = db_path();
    let url = cs(&format!("file://{}", p.display()));

    let h = engine_open(url.as_ptr());
    assert!(!h.is_null(), "open should succeed");

    assert_eq!(
        engine_exec(
            h,
            cs("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)").as_ptr()
        ),
        0
    );
    assert_eq!(
        engine_exec(h, cs("INSERT INTO t VALUES (1, 'hi')").as_ptr()),
        0
    );
    assert_eq!(engine_changes(h), 1);
    assert_eq!(
        engine_exec(h, cs("INSERT INTO t VALUES (2, NULL)").as_ptr()),
        0
    );
    assert!(engine_last_lsn(h) > 0);

    // Buffered query.
    let mut out: *mut EngineResult = ptr::null_mut();
    assert_eq!(
        engine_query(
            h,
            cs("SELECT id, body FROM t ORDER BY id").as_ptr(),
            &mut out
        ),
        0
    );
    assert!(!out.is_null());
    assert_eq!(engine_result_rows(out), 2);
    assert_eq!(engine_result_cols(out), 2);
    assert_eq!(rd(engine_result_colname(out, 0)).as_deref(), Some("id"));
    assert_eq!(rd(engine_result_value(out, 0, 0)).as_deref(), Some("1"));
    assert_eq!(rd(engine_result_value(out, 0, 1)).as_deref(), Some("hi"));
    // SQL NULL is reported as a null pointer.
    assert_eq!(rd(engine_result_value(out, 1, 1)), None);
    engine_result_free(out);
    engine_result_free(ptr::null_mut()); // idempotent on NULL

    // Error path: status non-zero, message retrievable.
    assert_ne!(engine_exec(h, cs("not valid sql").as_ptr()), 0);
    assert!(!rd(engine_last_error(h)).unwrap_or_default().is_empty());

    // Prepared statement with a bound parameter.
    let mut st: *mut EngineStmt = ptr::null_mut();
    assert_eq!(
        engine_prepare(h, cs("SELECT body FROM t WHERE id = ?").as_ptr(), &mut st),
        0
    );
    assert_eq!(engine_bind(st, 1, cs("i1").as_ptr()), 0);
    let mut done = -1;
    assert_eq!(engine_step(st, &mut done), 0);
    assert_eq!(done, 0, "a row should be available");
    assert_eq!(engine_column_count(st), 1);
    assert_eq!(rd(engine_column_name(st, 0)).as_deref(), Some("body"));
    assert_eq!(rd(engine_column_value(st, 0)).as_deref(), Some("hi"));
    assert_eq!(engine_step(st, &mut done), 0);
    assert_eq!(done, 1, "no more rows");
    assert_eq!(engine_finalize(st), 0);

    // Branching is deferred to Phase 4: NULL + a message on the handle.
    assert!(engine_branch(h, cs("b").as_ptr()).is_null());
    assert!(rd(engine_last_error(h))
        .unwrap_or_default()
        .contains("Phase 4"));

    engine_close(h);
    engine_close(ptr::null_mut()); // idempotent on NULL

    let _ = std::fs::remove_file(&p);
}

#[test]
fn misuse_on_null_handle() {
    // Operating on a NULL handle is misuse, not a crash.
    assert_eq!(engine_exec(ptr::null_mut(), cs("SELECT 1").as_ptr()), 6); // ENGINE_ERR_MISUSE
    assert_eq!(engine_changes(ptr::null_mut()), 0);
    // last_error on a NULL handle returns an empty (non-null) string.
    assert_eq!(rd(engine_last_error(ptr::null_mut())).as_deref(), Some(""));
}

#[test]
fn unknown_scheme_open_returns_null() {
    let url = cs("s3://bucket/db");
    assert!(
        engine_open(url.as_ptr()).is_null(),
        "s3 is a Phase-2 backend"
    );
    let url = cs("wat://nope");
    assert!(
        engine_open(url.as_ptr()).is_null(),
        "unknown scheme rejected"
    );
}
