// @twilldb/bun — ergonomic, typed embedding of the twill-db engine via
// bun:ffi (Phase 1, embedded path). Application code never touches raw pointers;
// resources are released deterministically with `using` / explicit close.
//
//   import { open } from "@twilldb/bun";
//   using db = open("file://./local.db");
//   db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)");
//   db.query("INSERT INTO notes VALUES (?, ?)", [1, "hello"]);
//   const rows = db.query("SELECT * FROM notes");

import { lib, cstr, readCString, ptr, STATUS, type Pointer } from "./ffi";

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

function encodeParam(p: Param): Buffer {
  // One-character type tag prefix (engine.h "parameter encoding").
  if (p === null || p === undefined) return cstr("n");
  if (typeof p === "boolean") return cstr("i" + (p ? 1 : 0));
  if (typeof p === "bigint") return cstr("i" + p.toString());
  if (typeof p === "number") return cstr((Number.isInteger(p) ? "i" : "f") + p.toString());
  if (typeof p === "string") return cstr("s" + p);
  if (p instanceof Uint8Array) return cstr("b" + Buffer.from(p).toString("base64"));
  if (Array.isArray(p)) return cstr("v" + p.join(","));
  throw new EngineError(STATUS.ERR_MISUSE, `unsupported parameter type: ${typeof p}`);
}

function errorFrom(handle: Pointer, status: number): EngineError {
  const msg = readCString(lib.engine_last_error(handle)) ?? "";
  return new EngineError(status, msg);
}

/** Open a database. The storage backend is selected by the URL scheme. */
export function open(url: string): Database {
  const handle = lib.engine_open(cstr(url));
  if (handle === null || (handle as unknown as number) === 0) {
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
    if (this.#handle === null) throw new EngineError(STATUS.ERR_MISUSE, "database is closed");
    return this.#handle;
  }

  /** Run a statement with no result set; returns rows affected. */
  exec(sql: string): number {
    const h = this.#h;
    const st = lib.engine_exec(h, cstr(sql));
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
    const out = new BigUint64Array(1);
    const st = lib.engine_prepare(h, cstr(sql), ptr(out));
    if (st !== STATUS.OK) throw errorFrom(h, st);
    return new Statement<T>(h, Number(out[0]) as Pointer);
  }

  /** BEGIN; run fn; COMMIT on normal return, ROLLBACK (and re-throw) on error. */
  transaction<R>(fn: (tx: Database) => R): R {
    const h = this.#h;
    let st = lib.engine_begin(h);
    if (st !== STATUS.OK) throw errorFrom(h, st);
    let result: R;
    try {
      result = fn(this);
    } catch (e) {
      lib.engine_rollback(h);
      throw e;
    }
    st = lib.engine_commit(h); // blocks until the WAL is durable
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
    const child = lib.engine_branch(h, cstr(name));
    if (child === null || (child as unknown as number) === 0) {
      throw errorFrom(h, STATUS.ERR_INTERNAL);
    }
    return new Database(child);
  }

  /** Commit LSN of the last commit on this connection. */
  get lastLsn(): bigint {
    return BigInt(lib.engine_last_lsn(this.#h));
  }

  close(): void {
    if (this.#handle !== null) {
      lib.engine_close(this.#handle);
      this.#handle = null;
    }
  }

  [Symbol.dispose](): void {
    this.close();
  }

  #bufferedQuery<T>(sql: string): T[] {
    const h = this.#h;
    const out = new BigUint64Array(1);
    const st = lib.engine_query(h, cstr(sql), ptr(out));
    if (st !== STATUS.OK) throw errorFrom(h, st);
    const result = Number(out[0]) as Pointer;
    try {
      const rows = lib.engine_result_rows(result);
      const cols = lib.engine_result_cols(result);
      const names: string[] = [];
      for (let c = 0; c < cols; c++) {
        names.push(readCString(lib.engine_result_colname(result, c)) ?? `col${c + 1}`);
      }
      const data: T[] = [];
      for (let r = 0; r < rows; r++) {
        const row: Row = {};
        for (let c = 0; c < cols; c++) {
          row[names[c]] = readCString(lib.engine_result_value(result, r, c));
        }
        data.push(row as T);
      }
      return data;
    } finally {
      lib.engine_result_free(result);
    }
  }

  #preparedAll<T>(sql: string, params: Param[]): T[] {
    using stmt = this.prepare<T>(sql);
    return stmt.all(...params);
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
    if (this.#stmt === null) throw new EngineError(STATUS.ERR_MISUSE, "statement is finalized");
    return this.#s_unchecked;
  }
  get #s_unchecked(): Pointer {
    return this.#stmt as Pointer;
  }

  #bind(params: Param[]): void {
    const s = this.#s;
    for (let i = 0; i < params.length; i++) {
      const st = lib.engine_bind(s, i + 1, encodeParam(params[i]));
      if (st !== STATUS.OK) throw errorFrom(this.#handle, st);
    }
  }

  /** Execute and buffer all rows. */
  all(...params: Param[]): T[] {
    const s = this.#s;
    if (params.length) this.#bind(params);
    const done = new Int32Array(1);
    const rows: T[] = [];
    let names: string[] | null = null;
    while (true) {
      const st = lib.engine_step(s, ptr(done));
      if (st !== STATUS.OK) throw errorFrom(this.#handle, st);
      if (done[0] === 1) break;
      if (names === null) {
        names = [];
        const cols = lib.engine_column_count(s);
        for (let c = 0; c < cols; c++) {
          names.push(readCString(lib.engine_column_name(s, c)) ?? `col${c + 1}`);
        }
      }
      const row: Row = {};
      for (let c = 0; c < names.length; c++) {
        row[names[c]] = readCString(lib.engine_column_value(s, c));
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
    const done = new Int32Array(1);
    // Step once to execute (writes produce no rows).
    const st = lib.engine_step(s, ptr(done));
    if (st !== STATUS.OK) throw errorFrom(this.#handle, st);
    const changes = Number(lib.engine_changes(this.#handle));
    lib.engine_reset(s);
    return changes;
  }

  finalize(): void {
    if (this.#stmt !== null) {
      lib.engine_finalize(this.#stmt);
      this.#stmt = null;
    }
  }

  [Symbol.dispose](): void {
    this.finalize();
  }
}
