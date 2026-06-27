// @twilldb/node — ergonomic, typed embedding of the twill-db engine via koffi
// (Phase 1 embedded path, on Node and every Node-based framework runtime).
//
// The public surface is identical to `@twilldb/bun`, by design: application code
// written against one runs unchanged on the other. Only the binding underneath
// differs (koffi here, bun:ffi there). Resources are released deterministically
// with `using` / explicit close.
//
//   import { open } from "@twilldb/node";
//   using db = open("file://./local.db");
//   db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)");
//   db.query("INSERT INTO notes VALUES (?, ?)", [1, "hello"]);
//   const rows = db.query("SELECT * FROM notes");

import { lib, isNull, STATUS, type Pointer } from "./ffi.ts";

export type Param = number | bigint | string | Uint8Array | null | number[] | boolean;
export type Row = Record<string, string | null>;

/** Error carrying the numeric EngineStatus and whether a retry may help. */
export class EngineError extends Error {
  readonly status: number;
  readonly retryable: boolean;
  constructor(status: number, message: string) {
    super(message || `engine status ${status}`);
    this.name = "EngineError";
    this.status = status;
    this.retryable = status === STATUS.ERR_CONFLICT || status === STATUS.ERR_STORAGE;
  }
}

function encodeParam(p: Param): string {
  // One-character type tag prefix (engine.h "parameter encoding").
  if (p === null || p === undefined) return "n";
  if (typeof p === "boolean") return "i" + (p ? 1 : 0);
  if (typeof p === "bigint") return "i" + p.toString();
  if (typeof p === "number") return (Number.isInteger(p) ? "i" : "f") + p.toString();
  if (typeof p === "string") return "s" + p;
  if (p instanceof Uint8Array) return "b" + Buffer.from(p).toString("base64");
  if (Array.isArray(p)) return "v" + p.join(",");
  throw new EngineError(STATUS.ERR_MISUSE, `unsupported parameter type: ${typeof p}`);
}

function errorFrom(handle: Pointer, status: number): EngineError {
  const msg = (lib.engine_last_error(handle) as string | null) ?? "";
  return new EngineError(status, msg);
}

/** Open a database. The storage backend is selected by the URL scheme. */
export function open(url: string): Database {
  const handle = lib.engine_open(url) as Pointer;
  if (isNull(handle)) {
    throw new EngineError(
      STATUS.ERR_STORAGE,
      `engine_open failed for "${url}" (unknown scheme or storage init error)`,
    );
  }
  return new Database(handle);
}

export class Database {
  #handle: Pointer | null;

  constructor(handle: Pointer) {
    this.#handle = handle;
  }

  get #h(): Pointer {
    if (isNull(this.#handle)) throw new EngineError(STATUS.ERR_MISUSE, "database is closed");
    return this.#handle as Pointer;
  }

  /** Run a statement with no result set; returns rows affected. */
  exec(sql: string): number {
    const h = this.#h;
    const st = lib.engine_exec(h, sql) as number;
    if (st !== STATUS.OK) throw errorFrom(h, st);
    return Number(lib.engine_changes(h));
  }

  /** Run a query (optionally parameterized) and buffer all rows. */
  query<T = Row>(sql: string, params?: Param[]): T[] {
    if (params && params.length > 0) return this.#preparedAll<T>(sql, params);
    return this.#bufferedQuery<T>(sql);
  }

  /** Prepare a reusable statement. */
  prepare<T = Row>(sql: string): Statement<T> {
    const h = this.#h;
    const out: [Pointer | null] = [null];
    const st = lib.engine_prepare(h, sql, out) as number;
    if (st !== STATUS.OK) throw errorFrom(h, st);
    return new Statement<T>(h, out[0] as Pointer);
  }

  /** BEGIN; run fn; COMMIT on normal return, ROLLBACK (and re-throw) on error. */
  transaction<R>(fn: (tx: Database) => R): R {
    const h = this.#h;
    let st = lib.engine_begin(h) as number;
    if (st !== STATUS.OK) throw errorFrom(h, st);
    let result: R;
    try {
      result = fn(this);
    } catch (e) {
      lib.engine_rollback(h);
      throw e;
    }
    st = lib.engine_commit(h) as number; // blocks until the WAL is durable
    if (st !== STATUS.OK) throw errorFrom(h, st);
    return result;
  }

  /**
   * Create a copy-on-write branch off this database at its current committed
   * LSN, returning a new {@link Database} bound to the branch. The branch sees
   * the base's committed data but writes in isolation — neither the base nor a
   * sibling observes a branch's writes. Close the returned handle when done.
   */
  branch(name: string): Database {
    const h = this.#h;
    const child = lib.engine_branch(h, name) as Pointer;
    if (isNull(child)) {
      throw errorFrom(h, STATUS.ERR_INTERNAL);
    }
    return new Database(child);
  }

  /** Commit LSN of the last commit on this connection. */
  get lastLsn(): bigint {
    return BigInt(lib.engine_last_lsn(this.#h) as number | bigint);
  }

  close(): void {
    if (!isNull(this.#handle)) {
      lib.engine_close(this.#handle as Pointer);
      this.#handle = null;
    }
  }

  [Symbol.dispose](): void {
    this.close();
  }

  #bufferedQuery<T>(sql: string): T[] {
    const h = this.#h;
    const out: [Pointer | null] = [null];
    const st = lib.engine_query(h, sql, out) as number;
    if (st !== STATUS.OK) throw errorFrom(h, st);
    const result = out[0] as Pointer;
    try {
      const rows = lib.engine_result_rows(result) as number;
      const cols = lib.engine_result_cols(result) as number;
      const names: string[] = [];
      for (let c = 0; c < cols; c++) {
        names.push((lib.engine_result_colname(result, c) as string | null) ?? `col${c + 1}`);
      }
      const data: T[] = [];
      for (let r = 0; r < rows; r++) {
        const row: Row = {};
        for (let c = 0; c < cols; c++) {
          row[names[c]] = lib.engine_result_value(result, r, c) as string | null;
        }
        data.push(row as T);
      }
      return data;
    } finally {
      lib.engine_result_free(result);
    }
  }

  #preparedAll<T>(sql: string, params: Param[]): T[] {
    const stmt = this.prepare<T>(sql);
    try {
      return stmt.all(...params);
    } finally {
      stmt.finalize();
    }
  }
}

export class Statement<T = Row> {
  #handle: Pointer; // owning connection
  #stmt: Pointer | null;

  constructor(handle: Pointer, stmt: Pointer) {
    this.#handle = handle;
    this.#stmt = stmt;
  }

  get #s(): Pointer {
    if (isNull(this.#stmt)) throw new EngineError(STATUS.ERR_MISUSE, "statement is finalized");
    return this.#stmt as Pointer;
  }

  #bind(params: Param[]): void {
    const s = this.#s;
    for (let i = 0; i < params.length; i++) {
      const st = lib.engine_bind(s, i + 1, encodeParam(params[i])) as number;
      if (st !== STATUS.OK) throw errorFrom(this.#handle, st);
    }
  }

  /** Execute and buffer all rows. */
  all(...params: Param[]): T[] {
    const s = this.#s;
    if (params.length) this.#bind(params);
    const done: [number] = [0];
    const rows: T[] = [];
    let names: string[] | null = null;
    while (true) {
      const st = lib.engine_step(s, done) as number;
      if (st !== STATUS.OK) throw errorFrom(this.#handle, st);
      if (done[0] === 1) break;
      if (names === null) {
        names = [];
        const cols = lib.engine_column_count(s) as number;
        for (let c = 0; c < cols; c++) {
          names.push((lib.engine_column_name(s, c) as string | null) ?? `col${c + 1}`);
        }
      }
      const row: Row = {};
      for (let c = 0; c < names.length; c++) {
        row[names[c]] = lib.engine_column_value(s, c) as string | null;
      }
      rows.push(row as T);
    }
    lib.engine_reset(s);
    return rows;
  }

  /** Execute and return the first row, or undefined. */
  get(...params: Param[]): T | undefined {
    return this.all(...params)[0];
  }

  /** Execute a write and return rows affected. */
  run(...params: Param[]): number {
    const s = this.#s;
    if (params.length) this.#bind(params);
    const done: [number] = [0];
    // Step once to execute (writes produce no rows).
    const st = lib.engine_step(s, done) as number;
    if (st !== STATUS.OK) throw errorFrom(this.#handle, st);
    const changes = Number(lib.engine_changes(this.#handle));
    lib.engine_reset(s);
    return changes;
  }

  finalize(): void {
    if (!isNull(this.#stmt)) {
      lib.engine_finalize(this.#stmt as Pointer);
      this.#stmt = null;
    }
  }

  [Symbol.dispose](): void {
    this.finalize();
  }
}
