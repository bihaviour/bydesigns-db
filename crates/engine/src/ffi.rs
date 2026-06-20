//! The stable C ABI (`engine.h`). Opaque handles, error-code driven, no Rust
//! types and no panics across the boundary: every exported function is wrapped
//! in `catch_unwind`, so a caught panic becomes `ENGINE_ERR_INTERNAL` and leaves
//! the handle in a defined, queryable state — never undefined behaviour.
//!
//! Ownership (spec 02): handles are created by `engine_open` and destroyed only
//! by `engine_close`; results by `engine_result_free`; statements by
//! `engine_finalize`. All returned `const char*` are borrowed into the owning
//! object's storage and must be copied out before it advances or is freed.
//!
//! These are `extern "C"` entry points called from C/FFI, so dereferencing the
//! opaque handle pointers is the contract, not a smell — hence the module-wide
//! allow below (each access is null-checked and `catch_unwind`-wrapped).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::conn::{Connection, Statement};
use crate::error::EngineStatus;
use crate::exec::ResultSet;
use crate::value::Value;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_longlong};
use std::panic::{catch_unwind, AssertUnwindSafe};

const OK: c_int = EngineStatus::Ok as c_int;
const MISUSE: c_int = EngineStatus::ErrMisuse as c_int;
const INTERNAL: c_int = EngineStatus::ErrInternal as c_int;

/// A buffered query result: owned, NUL-terminated cells; `None` cells are SQL
/// NULL (returned as a null pointer).
pub struct EngineResult {
    names: Vec<CString>,
    cells: Vec<Vec<Option<CString>>>,
    nrows: c_int,
    ncols: c_int,
}

/// A prepared statement plus its rendered current-row cell cache (so borrowed
/// column pointers stay valid until the next step/reset/finalize).
pub struct EngineStmt {
    stmt: Statement,
    names: Vec<CString>,
    cells: Vec<Option<CString>>,
}

// ---- helpers --------------------------------------------------------------

unsafe fn as_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        None
    } else {
        CStr::from_ptr(p).to_str().ok()
    }
}

fn cstring(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

fn render_cell(v: &Value) -> Option<CString> {
    v.render().map(|s| cstring(&s))
}

fn from_result(rs: ResultSet) -> EngineResult {
    let names = rs.columns.iter().map(|c| cstring(c)).collect();
    let nrows = rs.rows.len() as c_int;
    let ncols = rs.columns.len() as c_int;
    let cells = rs
        .rows
        .iter()
        .map(|row| row.iter().map(render_cell).collect())
        .collect();
    EngineResult {
        names,
        cells,
        nrows,
        ncols,
    }
}

/// Decode the string-only typed bind encoding (spec 02 "parameter encoding"):
/// `i` int, `f` float, `s` text, `b` base64-bytes, `n` NULL, `v` vector.
fn decode_param(s: &str) -> Value {
    let mut it = s.chars();
    match it.next() {
        Some('i') => Value::Int(s[1..].trim().parse().unwrap_or(0)),
        Some('f') => Value::Real(s[1..].trim().parse().unwrap_or(0.0)),
        Some('s') => Value::Text(s[1..].to_string()),
        Some('b') => Value::Blob(crate::value::base64_decode(&s[1..]).unwrap_or_default()),
        Some('n') => Value::Null,
        // Vector type is a Phase 5 capability; carry the literal as text for now.
        Some('v') => Value::Text(s[1..].to_string()),
        _ => Value::Text(s.to_string()),
    }
}

fn handle<F: FnOnce(&mut Connection) -> c_int>(h: *mut Connection, f: F) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let c = match unsafe { h.as_mut() } {
            Some(c) => c,
            None => return MISUSE,
        };
        f(c)
    }))
    .unwrap_or(INTERNAL)
}

fn finish(c: &mut Connection, r: crate::error::Result<()>) -> c_int {
    match r {
        Ok(()) => OK,
        Err(e) => {
            c.set_last_error(&e.message);
            e.status as c_int
        }
    }
}

// ---- lifecycle ------------------------------------------------------------

/// Open a database. `url` selects the backend by scheme (`file://` in Phase 1).
/// Returns NULL on failure (no handle exists to query for the error).
#[no_mangle]
pub extern "C" fn engine_open(url: *const c_char) -> *mut Connection {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(url) = (unsafe { as_str(url) }) else {
            return std::ptr::null_mut();
        };
        match Connection::open(url) {
            Ok(c) => Box::into_raw(Box::new(c)),
            Err(_) => std::ptr::null_mut(),
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

/// Close a handle. Idempotent on NULL.
#[no_mangle]
pub extern "C" fn engine_close(h: *mut Connection) {
    if h.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(h));
    }));
}

// ---- one-shot execution ---------------------------------------------------

#[no_mangle]
pub extern "C" fn engine_exec(h: *mut Connection, sql: *const c_char) -> c_int {
    handle(h, |c| {
        let Some(sql) = (unsafe { as_str(sql) }) else {
            c.set_last_error("invalid UTF-8 in SQL");
            return MISUSE;
        };
        let r = c.exec(sql);
        finish(c, r)
    })
}

#[no_mangle]
pub extern "C" fn engine_query(
    h: *mut Connection,
    sql: *const c_char,
    out: *mut *mut EngineResult,
) -> c_int {
    handle(h, |c| {
        if out.is_null() {
            c.set_last_error("null out pointer");
            return MISUSE;
        }
        unsafe { *out = std::ptr::null_mut() };
        let Some(sql) = (unsafe { as_str(sql) }) else {
            c.set_last_error("invalid UTF-8 in SQL");
            return MISUSE;
        };
        match c.query(sql) {
            Ok(rs) => {
                let boxed = Box::new(from_result(rs));
                unsafe { *out = Box::into_raw(boxed) };
                OK
            }
            Err(e) => {
                c.set_last_error(&e.message);
                e.status as c_int
            }
        }
    })
}

// ---- prepared statements --------------------------------------------------

#[no_mangle]
pub extern "C" fn engine_prepare(
    h: *mut Connection,
    sql: *const c_char,
    out: *mut *mut EngineStmt,
) -> c_int {
    handle(h, |c| {
        if out.is_null() {
            c.set_last_error("null out pointer");
            return MISUSE;
        }
        unsafe { *out = std::ptr::null_mut() };
        let Some(sql) = (unsafe { as_str(sql) }) else {
            c.set_last_error("invalid UTF-8 in SQL");
            return MISUSE;
        };
        match c.prepare(sql) {
            Ok(stmt) => {
                let boxed = Box::new(EngineStmt {
                    stmt,
                    names: Vec::new(),
                    cells: Vec::new(),
                });
                unsafe { *out = Box::into_raw(boxed) };
                OK
            }
            Err(e) => {
                c.set_last_error(&e.message);
                e.status as c_int
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn engine_bind(s: *mut EngineStmt, idx: c_int, value: *const c_char) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let f = match unsafe { s.as_mut() } {
            Some(f) => f,
            None => return MISUSE,
        };
        let v = match unsafe { as_str(value) } {
            Some(s) => decode_param(s),
            None => Value::Null,
        };
        match f.stmt.bind(idx as usize, v) {
            Ok(()) => OK,
            Err(e) => {
                f.stmt.record_error(&e.message);
                e.status as c_int
            }
        }
    }))
    .unwrap_or(INTERNAL)
}

#[no_mangle]
pub extern "C" fn engine_step(s: *mut EngineStmt, done: *mut c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let f = match unsafe { s.as_mut() } {
            Some(f) => f,
            None => return MISUSE,
        };
        match f.stmt.step() {
            Ok(has_row) => {
                if f.names.is_empty() {
                    for i in 0..f.stmt.column_count() {
                        f.names.push(cstring(f.stmt.column_name(i).unwrap_or("")));
                    }
                }
                f.cells.clear();
                if has_row {
                    for i in 0..f.stmt.column_count() {
                        f.cells.push(f.stmt.column_value(i).and_then(render_cell));
                    }
                }
                if !done.is_null() {
                    unsafe { *done = if has_row { 0 } else { 1 } };
                }
                OK
            }
            Err(e) => {
                f.stmt.record_error(&e.message);
                e.status as c_int
            }
        }
    }))
    .unwrap_or(INTERNAL)
}

#[no_mangle]
pub extern "C" fn engine_finalize(s: *mut EngineStmt) -> c_int {
    if s.is_null() {
        return MISUSE;
    }
    catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(s));
        OK
    }))
    .unwrap_or(INTERNAL)
}

#[no_mangle]
pub extern "C" fn engine_reset(s: *mut EngineStmt) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let f = match unsafe { s.as_mut() } {
            Some(f) => f,
            None => return MISUSE,
        };
        f.stmt.reset();
        f.cells.clear();
        f.names.clear();
        OK
    }))
    .unwrap_or(INTERNAL)
}

// ---- transactions ---------------------------------------------------------

#[no_mangle]
pub extern "C" fn engine_begin(h: *mut Connection) -> c_int {
    handle(h, |c| {
        let r = c.begin();
        finish(c, r)
    })
}

#[no_mangle]
pub extern "C" fn engine_commit(h: *mut Connection) -> c_int {
    handle(h, |c| {
        let r = c.commit();
        finish(c, r)
    })
}

#[no_mangle]
pub extern "C" fn engine_rollback(h: *mut Connection) -> c_int {
    handle(h, |c| {
        let r = c.rollback();
        finish(c, r)
    })
}

// ---- branching (Phase 4) --------------------------------------------------

/// Reserved in the frozen ABI; copy-on-write branching lands in Phase 4. Sets
/// the handle's last error and returns NULL.
#[no_mangle]
pub extern "C" fn engine_branch(h: *mut Connection, _name: *const c_char) -> *mut Connection {
    catch_unwind(AssertUnwindSafe(|| {
        if let Some(c) = unsafe { h.as_mut() } {
            c.set_last_error(
                "branching (engine_branch) is a Phase 4 feature; not available in Phase 1",
            );
        }
        std::ptr::null_mut()
    }))
    .unwrap_or(std::ptr::null_mut())
}

// ---- result / row access --------------------------------------------------

#[no_mangle]
pub extern "C" fn engine_result_rows(r: *const EngineResult) -> c_int {
    unsafe { r.as_ref() }.map(|r| r.nrows).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn engine_result_cols(r: *const EngineResult) -> c_int {
    unsafe { r.as_ref() }.map(|r| r.ncols).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn engine_result_colname(r: *const EngineResult, col: c_int) -> *const c_char {
    let Some(r) = (unsafe { r.as_ref() }) else {
        return std::ptr::null();
    };
    match r.names.get(col as usize) {
        Some(c) => c.as_ptr(),
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn engine_result_value(
    r: *const EngineResult,
    row: c_int,
    col: c_int,
) -> *const c_char {
    let Some(r) = (unsafe { r.as_ref() }) else {
        return std::ptr::null();
    };
    match r
        .cells
        .get(row as usize)
        .and_then(|row| row.get(col as usize))
    {
        Some(Some(c)) => c.as_ptr(),
        _ => std::ptr::null(), // out of range or SQL NULL
    }
}

#[no_mangle]
pub extern "C" fn engine_result_free(r: *mut EngineResult) {
    if r.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(Box::from_raw(r));
    }));
}

// ---- statement cursor column access ---------------------------------------

#[no_mangle]
pub extern "C" fn engine_column_count(s: *const EngineStmt) -> c_int {
    unsafe { s.as_ref() }
        .map(|f| f.names.len() as c_int)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn engine_column_name(s: *const EngineStmt, col: c_int) -> *const c_char {
    let Some(f) = (unsafe { s.as_ref() }) else {
        return std::ptr::null();
    };
    match f.names.get(col as usize) {
        Some(c) => c.as_ptr(),
        None => std::ptr::null(),
    }
}

#[no_mangle]
pub extern "C" fn engine_column_value(s: *const EngineStmt, col: c_int) -> *const c_char {
    let Some(f) = (unsafe { s.as_ref() }) else {
        return std::ptr::null();
    };
    match f.cells.get(col as usize) {
        Some(Some(c)) => c.as_ptr(),
        _ => std::ptr::null(),
    }
}

// ---- errors / metadata ----------------------------------------------------

#[no_mangle]
pub extern "C" fn engine_last_error(h: *mut Connection) -> *const c_char {
    match unsafe { h.as_ref() } {
        Some(c) => c.last_error_ptr(),
        None => c"".as_ptr(),
    }
}

#[no_mangle]
pub extern "C" fn engine_changes(h: *mut Connection) -> c_longlong {
    unsafe { h.as_ref() }
        .map(|c| c.last_changes as c_longlong)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn engine_last_lsn(h: *mut Connection) -> c_longlong {
    unsafe { h.as_ref() }
        .map(|c| c.last_lsn as c_longlong)
        .unwrap_or(0)
}

/// ABI version for the binding to verify at load time.
#[no_mangle]
pub extern "C" fn engine_abi_version() -> c_int {
    crate::ENGINE_ABI_VERSION as c_int
}
