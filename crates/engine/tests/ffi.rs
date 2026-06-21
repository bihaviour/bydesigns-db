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

    // Branching (Phase 4): fork a copy-on-write branch, diverge it, and prove
    // isolation — the branch sees the base's rows plus its own; the base does not.
    let bh = engine_branch(h, cs("feature").as_ptr());
    assert!(!bh.is_null(), "engine_branch returns a live branch handle");

    // The branch inherits the base's committed rows.
    let mut bq: *mut EngineResult = ptr::null_mut();
    assert_eq!(
        engine_query(bh, cs("SELECT id FROM t ORDER BY id").as_ptr(), &mut bq),
        0
    );
    assert_eq!(engine_result_rows(bq), 2, "branch inherits base rows");
    engine_result_free(bq);

    // Diverge the branch with a row of its own.
    assert_eq!(
        engine_exec(bh, cs("INSERT INTO t VALUES (3, 'branch-only')").as_ptr()),
        0
    );
    let mut bq2: *mut EngineResult = ptr::null_mut();
    assert_eq!(
        engine_query(bh, cs("SELECT id FROM t ORDER BY id").as_ptr(), &mut bq2),
        0
    );
    assert_eq!(engine_result_rows(bq2), 3, "branch sees its diverged row");
    engine_result_free(bq2);

    // The base is untouched by the branch's write.
    let mut hq: *mut EngineResult = ptr::null_mut();
    assert_eq!(
        engine_query(h, cs("SELECT id FROM t ORDER BY id").as_ptr(), &mut hq),
        0
    );
    assert_eq!(
        engine_result_rows(hq),
        2,
        "base never sees the branch's write"
    );
    engine_result_free(hq);
    engine_close(bh);

    engine_close(h);
    engine_close(ptr::null_mut()); // idempotent on NULL

    let _ = std::fs::remove_file(&p);
}

#[test]
fn vector_search_via_c_abi() {
    // Phase 5: the vector type, an HNSW index, and a top-k query drive through the
    // same C ABI bun:ffi binds — vectors flow as their `[..]` text literal and as
    // the `v…` typed bind encoding; no new symbols are involved.
    let p = db_path();
    let url = cs(&format!("file://{}", p.display()));
    let h = engine_open(url.as_ptr());
    assert!(!h.is_null());

    assert_eq!(
        engine_exec(
            h,
            cs("CREATE TABLE docs (id INTEGER PRIMARY KEY, e VECTOR(3))").as_ptr()
        ),
        0
    );
    assert_eq!(
        engine_exec(
            h,
            cs("CREATE INDEX docs_e ON docs USING hnsw (e) WITH (metric='l2')").as_ptr()
        ),
        0
    );
    for (id, v) in [(1, "[1,0,0]"), (2, "[0,1,0]"), (3, "[0,0,1]")] {
        let sql = cs(&format!("INSERT INTO docs VALUES ({id}, {v})"));
        assert_eq!(engine_exec(h, sql.as_ptr()), 0);
    }

    // Top-1 nearest to [0,1,0] via a bound vector parameter (the "v…" encoding).
    let mut st: *mut EngineStmt = ptr::null_mut();
    assert_eq!(
        engine_prepare(
            h,
            cs("SELECT id FROM docs ORDER BY e <-> ? LIMIT 1").as_ptr(),
            &mut st
        ),
        0
    );
    assert_eq!(engine_bind(st, 1, cs("v[0,1,0]").as_ptr()), 0);
    let mut done = -1;
    assert_eq!(engine_step(st, &mut done), 0);
    assert_eq!(done, 0, "a nearest neighbour should be found");
    assert_eq!(rd(engine_column_value(st, 0)).as_deref(), Some("2"));
    assert_eq!(engine_finalize(st), 0);

    // A vector renders as its [..] literal across the string-only ABI.
    let mut out: *mut EngineResult = ptr::null_mut();
    assert_eq!(
        engine_query(h, cs("SELECT e FROM docs WHERE id = 1").as_ptr(), &mut out),
        0
    );
    assert_eq!(
        rd(engine_result_value(out, 0, 0)).as_deref(),
        Some("[1,0,0]")
    );
    engine_result_free(out);

    engine_close(h);
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
    // A scheme with no backend is rejected, never silently defaulted.
    let url = cs("wat://nope");
    assert!(
        engine_open(url.as_ptr()).is_null(),
        "unknown scheme rejected"
    );
}

#[test]
fn s3_scheme_opens_the_object_backend() {
    // Phase 2: the *same* engine binary opens an object-storage database with no
    // recompile — the connection string is the only thing that changed (the seam
    // never moved). The object floor is rooted under a temp dir for the test.
    let root = std::env::temp_dir().join(format!("bydesigns-ffi-obj-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::env::set_var("BYDESIGNS_OBJECT_ROOT", &root);

    let bucket = format!("s3://buck/{}", std::process::id());
    let url = cs(&bucket);
    let h = engine_open(url.as_ptr());
    assert!(!h.is_null(), "s3:// opens the ObjectStorage backend");

    assert_eq!(
        engine_exec(
            h,
            cs("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT)").as_ptr()
        ),
        0
    );
    assert_eq!(
        engine_exec(h, cs("INSERT INTO t VALUES (1, 'disaggregated')").as_ptr()),
        0
    );
    assert!(engine_last_lsn(h) > 0);

    let mut out: *mut EngineResult = ptr::null_mut();
    assert_eq!(
        engine_query(h, cs("SELECT body FROM t WHERE id = 1").as_ptr(), &mut out),
        0
    );
    assert!(!out.is_null());
    assert_eq!(engine_result_rows(out), 1);
    assert_eq!(
        rd(engine_result_value(out, 0, 0)).as_deref(),
        Some("disaggregated")
    );
    engine_result_free(out);
    engine_close(h);

    let _ = std::fs::remove_dir_all(&root);
}
