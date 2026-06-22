// @twilldb/bun — OLAP materialization glue (issue #28): publish row data to
// open columnar snapshots that a second engine (DuckDB) queries directly.
//
// This is the HTAP shape spec 12 describes: two engines over one storage floor.
// The row engine stays OLTP-only — it never goes columnar. Instead the only code
// we own, a thin materialization job, periodically snapshots a table to an open
// format (Parquet) on the shared store; DuckDB reads those snapshots unmodified.
// Possible precisely because storage is decoupled: the snapshots can live beside
// the database's own objects on the same bucket.
//
//   import { open } from "@twilldb/bun";
//   import { Materializer } from "@twilldb/bun/olap";
//
//   using db = open("file://./app.db");
//   const m = new Materializer(db, { table: "events", dir: "./olap", cadenceMs: 60_000 });
//   m.start();                       // snapshot every 60s
//   // … DuckDB: SELECT kind, sum(amount) FROM './olap/events.parquet' GROUP BY kind
//   m.stop();
//
// Parquet is the production format; DuckDB is also the off-the-shelf writer we
// shell out to (COPY … TO … (FORMAT PARQUET)). When DuckDB is absent the job
// falls back to CSV, which DuckDB also reads via read_csv_auto — so the snapshot
// is always publishable, only the encoding changes.

import { mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Database } from "./index";

export type SnapshotFormat = "parquet" | "csv";

export interface MaterializeOptions {
  /** Table to snapshot. */
  table: string;
  /** Directory the snapshot is published into (created if missing). */
  dir: string;
  /** Columns to include; defaults to all (`SELECT *`). */
  columns?: string[];
  /** Output format. Defaults to "parquet", falling back to "csv" if DuckDB is absent. */
  format?: SnapshotFormat;
}

export interface SnapshotResult {
  /** Absolute-or-relative path to the published snapshot file. */
  path: string;
  /** Number of rows written. */
  rows: number;
  /** Format actually produced (may differ from the request if DuckDB is absent). */
  format: SnapshotFormat;
}

// Table/column names are interpolated into SQL, into a DuckDB CLI SQL string,
// and into snapshot file paths. Restrict them to a plain SQL identifier so none
// of those sinks can be injected (no quotes, semicolons, whitespace, path
// separators, `..`, or leading dots). Callers pass schema identifiers, not user
// input, but validating here keeps every sink safe regardless.
const IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/;

function checkIdent(kind: string, name: string): void {
  if (!IDENT.test(name)) {
    throw new Error(`invalid ${kind} identifier ${JSON.stringify(name)}: must match ${IDENT}`);
  }
}

/** Whether the DuckDB CLI is on PATH (used as the Parquet writer and reader). */
export function duckdbAvailable(): boolean {
  try {
    const r = Bun.spawnSync(["duckdb", "--version"]);
    return r.success;
  } catch {
    return false;
  }
}

function csvCell(v: string | null): string {
  if (v === null) return "";
  return /[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v;
}

// Create a private, exclusively-owned staging directory under `parent`.
// `mkdtempSync` makes a 0700 directory with an unguessable suffix in one atomic
// syscall — the secure-temp-file pattern. We do every intermediate write inside
// it, then atomically rename only the finished artifact into `parent`; this is
// why the snapshot path stays deterministic (a known location to read from)
// while no intermediate is ever at a predictable shared path (which would invite
// a symlink/race attack — CodeQL js/insecure-temporary-file).
function stagingDir(parent: string): string {
  return mkdtempSync(join(parent, ".twill-stage-"));
}

function writeCsv(rows: Record<string, string | null>[], columns: string[], path: string): void {
  const header = columns.join(",");
  const body = rows.map((r) => columns.map((c) => csvCell(r[c])).join(",")).join("\n");
  // Exclusive create ("wx") inside the private staging dir: never follow or
  // overwrite a pre-existing file/symlink — fail instead.
  writeFileSync(path, rows.length ? `${header}\n${body}\n` : `${header}\n`, { flag: "wx" });
}

// Escape a path for embedding as a single-quoted DuckDB SQL literal. The table
// name in the path is already an allowlisted identifier; this also neutralizes a
// quote in the caller-provided `dir`.
function sqlLit(path: string): string {
  return path.replace(/'/g, "''");
}

// Convert a staged CSV to Parquet with DuckDB. Both paths live inside the
// caller's private staging dir, so we write straight to `outPath`; the caller
// renames the finished file into place atomically.
function csvToParquet(csvPath: string, outPath: string): void {
  const sql = `COPY (SELECT * FROM read_csv_auto('${sqlLit(csvPath)}', header=true)) TO '${sqlLit(outPath)}' (FORMAT PARQUET)`;
  const r = Bun.spawnSync(["duckdb", "-c", sql]);
  if (!r.success) {
    throw new Error(`duckdb parquet COPY failed: ${r.stderr.toString()}`);
  }
}

/**
 * Snapshot one table to an open columnar file on the shared store, atomically.
 * Returns the published path, row count, and the format actually written.
 */
export function materialize(db: Database, opts: MaterializeOptions): SnapshotResult {
  // Validate identifiers before they reach any SQL / CLI / path sink.
  checkIdent("table", opts.table);
  for (const c of opts.columns ?? []) {
    if (c !== "*") checkIdent("column", c);
  }
  mkdirSync(opts.dir, { recursive: true });
  const cols = opts.columns ?? ["*"];
  const rows = db.query<Record<string, string | null>>(
    `SELECT ${cols.join(", ")} FROM ${opts.table}`,
  );
  // Column order from the first row when projecting "*", else the requested set.
  const columns = cols[0] === "*" ? Object.keys(rows[0] ?? {}) : cols;

  const wantParquet = (opts.format ?? "parquet") === "parquet";
  const useParquet = wantParquet && duckdbAvailable();

  // Everything intermediate is written inside a private staging dir, then the
  // finished artifact is renamed into `opts.dir` (atomic publish — a reader only
  // ever sees a whole snapshot).
  const stage = stagingDir(opts.dir);
  try {
    if (!useParquet) {
      const path = join(opts.dir, `${opts.table}.csv`);
      const staged = join(stage, `${opts.table}.csv`);
      writeCsv(rows, columns, staged);
      renameSync(staged, path);
      return { path, rows: rows.length, format: "csv" };
    }
    // Stage to CSV, convert to Parquet (both inside the private dir), publish.
    const stagedCsv = join(stage, `${opts.table}.csv`);
    writeCsv(rows, columns, stagedCsv);
    const stagedParquet = join(stage, `${opts.table}.parquet`);
    csvToParquet(stagedCsv, stagedParquet);
    const path = join(opts.dir, `${opts.table}.parquet`);
    renameSync(stagedParquet, path);
    return { path, rows: rows.length, format: "parquet" };
  } finally {
    rmSync(stage, { recursive: true, force: true });
  }
}

/**
 * Periodic materializer: snapshots a table on a configurable cadence. The row
 * engine keeps serving OLTP while snapshots refresh in the background; DuckDB
 * always reads the last whole snapshot (publishes are atomic).
 */
export class Materializer {
  readonly #db: Database;
  readonly #opts: MaterializeOptions;
  readonly #cadenceMs: number;
  #timer: ReturnType<typeof setInterval> | null = null;

  constructor(db: Database, opts: MaterializeOptions & { cadenceMs: number }) {
    this.#db = db;
    this.#opts = opts;
    this.#cadenceMs = opts.cadenceMs;
  }

  /** Publish one snapshot immediately. */
  materializeOnce(): SnapshotResult {
    return materialize(this.#db, this.#opts);
  }

  /** Begin publishing a snapshot every `cadenceMs` (one immediately). */
  start(): void {
    if (this.#timer !== null) return;
    this.materializeOnce();
    this.#timer = setInterval(() => this.materializeOnce(), this.#cadenceMs);
    // Don't keep the process alive solely for the materializer.
    (this.#timer as { unref?: () => void }).unref?.();
  }

  /** Stop publishing. */
  stop(): void {
    if (this.#timer !== null) {
      clearInterval(this.#timer);
      this.#timer = null;
    }
  }
}
