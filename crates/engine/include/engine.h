/* engine.h — bydesigns-db stable C ABI (spec 02 — Engine Core).
 *
 * The single boundary every runtime binds to (bun:ffi, NAPI, static link). All
 * functions are thread-safe across distinct handles; a single handle must not be
 * used concurrently from multiple threads. Opaque handles only — callers never
 * see Rust layout. No Rust panic crosses this boundary: a caught panic becomes
 * ENGINE_ERR_INTERNAL and the handle stays defined and queryable.
 *
 * Frozen in Phase 1. Later phases ADD backends/listeners behind the same
 * symbols; they do not change these signatures.
 */
#ifndef BYDESIGNS_ENGINE_H
#define BYDESIGNS_ENGINE_H

#ifdef __cplusplus
extern "C" {
#endif

#include <stdint.h>

#define ENGINE_ABI_VERSION 2

typedef struct EngineHandle EngineHandle; /* a connection */
typedef struct EngineResult EngineResult; /* a buffered query result */
typedef struct EngineStmt   EngineStmt;   /* a prepared statement */

typedef enum {
    ENGINE_OK             = 0,
    ENGINE_ERR_SQL        = 1, /* parse / plan / type error */
    ENGINE_ERR_CONSTRAINT = 2, /* unique / fk / check violation */
    ENGINE_ERR_CONFLICT   = 3, /* serialization / write conflict (retryable) */
    ENGINE_ERR_STORAGE    = 4, /* backend I/O, CAS rejected, S3 fault */
    ENGINE_ERR_TXN        = 5, /* illegal state-machine transition */
    ENGINE_ERR_MISUSE     = 6, /* null handle, use-after-free, bad arg */
    ENGINE_ERR_INTERNAL   = 7  /* bug; engine remains defined, never UB */
} EngineStatus;

/* ---- lifecycle ------------------------------------------------------ */
/* url selects the storage backend by scheme: "file://./local.db" (Phase 1)
   or "s3://bucket/mydb?region=..." (Phase 2). NUL-terminated.
   Returns NULL on failure; the caller then has no handle to query. */
EngineHandle* engine_open(const char* url);
void          engine_close(EngineHandle* h); /* idempotent on NULL */

/* ---- one-shot execution -------------------------------------------- */
/* DDL/DML with no result set. Row count via engine_changes(). */
EngineStatus  engine_exec(EngineHandle* h, const char* sql);

/* Buffered query. *out receives an owned EngineResult on ENGINE_OK, else NULL.
   Values are NUL-terminated text; a SQL NULL is reported as a NULL pointer. */
EngineStatus  engine_query(EngineHandle* h, const char* sql, EngineResult** out);

/* ---- prepared statements ------------------------------------------- */
EngineStatus  engine_prepare(EngineHandle* h, const char* sql, EngineStmt** out);
/* bind by 1-based positional index; value is a NUL-terminated typed literal:
   "i42" int, "f3.5" float, "shello" text, "b<base64>" bytes, "n" NULL. */
EngineStatus  engine_bind(EngineStmt* s, int idx, const char* value);
/* step: ENGINE_OK with *done = 0 means a row is current; *done = 1 means no
   more rows. Column values for the current row via engine_column_value. */
EngineStatus  engine_step(EngineStmt* s, int* done);
EngineStatus  engine_finalize(EngineStmt* s); /* frees the statement */
EngineStatus  engine_reset(EngineStmt* s);    /* re-execute, keep bindings */

/* ---- transactions -------------------------------------------------- */
EngineStatus  engine_begin(EngineHandle* h);
EngineStatus  engine_commit(EngineHandle* h);   /* blocks until WAL durable */
EngineStatus  engine_rollback(EngineHandle* h);

/* ---- branching (Phase 4) ------------------------------------------- */
/* Create a copy-on-write branch off the database `h` is connected to, at its
   current committed LSN, and return a NEW connection handle bound to that
   branch. The branch shares the base's immutable history but writes in
   isolation: neither the base nor any sibling sees a branch's writes. The
   returned handle is owned by the caller and freed with engine_close().
   Returns NULL on failure (e.g. inside an active transaction or branch-of-
   branch); the reason is available via engine_last_error(h). */
EngineHandle* engine_branch(EngineHandle* h, const char* name);

/* ---- result / row access (borrowed pointers into the result) -------- */
int           engine_result_rows(const EngineResult* r);
int           engine_result_cols(const EngineResult* r);
const char*   engine_result_colname(const EngineResult* r, int col);
/* Borrowed; valid until engine_result_free(r). NULL for a SQL NULL cell. */
const char*   engine_result_value(const EngineResult* r, int row, int col);

/* ---- statement cursor column access -------------------------------- */
int           engine_column_count(const EngineStmt* s);
const char*   engine_column_name(const EngineStmt* s, int col);
/* Borrowed; valid only until the next engine_step/reset/finalize on s.
   NULL for a SQL NULL cell. */
const char*   engine_column_value(const EngineStmt* s, int col);

/* ---- errors / metadata -------------------------------------------- */
/* Borrowed, per-handle C string for the LAST error on h; valid until the next
   call on h. Empty string if none. */
const char*   engine_last_error(EngineHandle* h);
long long     engine_changes(EngineHandle* h);  /* rows affected, last stmt */
long long     engine_last_lsn(EngineHandle* h); /* commit LSN of last commit */
int           engine_abi_version(void);

/* ---- freeing ------------------------------------------------------- */
void          engine_result_free(EngineResult* r); /* idempotent on NULL */
/* statements are freed by engine_finalize; handles by engine_close. */

#ifdef __cplusplus
}
#endif
#endif /* BYDESIGNS_ENGINE_H */
