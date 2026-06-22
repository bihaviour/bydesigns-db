// Composition test (issue #28): DuckDB OLAP over the same storage floor.
// Verifies the materialization glue (`../src/olap`): the row engine snapshots a
// table to an open columnar file, atomically, and a second engine (DuckDB) runs
// aggregations over it — the row engine staying OLTP throughout.
//
// The CSV-export and Materializer assertions run everywhere. The Parquet +
// DuckDB round-trip runs only where the DuckDB CLI is installed; it is reported
// as skipped (not silently passed) otherwise.
//
// Run: cargo build -p twill-engine --release && (cd clients/bun && bun test olap)

import { test, expect, beforeEach, afterEach } from "bun:test";
import { open, type Database } from "../src/index";
import { materialize, duckdbAvailable, Materializer } from "../src/olap";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { existsSync, readFileSync, readdirSync, rmSync } from "node:fs";

let dbFile: string;
let url: string;
let dir: string;

function seed(db: Database): void {
  db.exec("CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, amount REAL)");
  db.exec(
    "INSERT INTO events VALUES (1,'click',1.5),(2,'view',0.0),(3,'click',2.5),(4,'view',4.0),(5,'click',1.0)",
  );
}

beforeEach(() => {
  const tag = `${process.pid}-${Math.random().toString(36).slice(2)}`;
  dbFile = join(tmpdir(), `twilldb-olap-${tag}.db`);
  url = `file://${dbFile}`;
  dir = join(tmpdir(), `twilldb-olap-snap-${tag}`);
});

afterEach(() => {
  try {
    rmSync(dbFile, { force: true });
    rmSync(dir, { recursive: true, force: true });
  } catch {}
});

test("CSV materialization exports the table content faithfully", () => {
  using db = open(url);
  seed(db);

  const snap = materialize(db, { table: "events", dir, format: "csv" });
  expect(snap.format).toBe("csv");
  expect(snap.rows).toBe(5);
  expect(existsSync(snap.path)).toBe(true);

  const lines = readFileSync(snap.path, "utf8").trimEnd().split("\n");
  expect(lines[0].split(",").sort()).toEqual(["amount", "id", "kind"].sort());
  expect(lines.length).toBe(6); // header + 5 rows
  // A known row is present (column order follows the first row's keys).
  expect(readFileSync(snap.path, "utf8")).toContain("click");
});

test("publish is atomic — no staging or .tmp files remain", () => {
  using db = open(url);
  seed(db);
  const snap = materialize(db, { table: "events", dir, format: "csv" });
  const leftovers = readdirSync(dir).filter((f) => f.endsWith(".tmp") || f.includes(".staging."));
  expect(leftovers).toEqual([]);
  expect(snap.path.endsWith(".csv")).toBe(true);
});

test("Materializer.materializeOnce publishes a snapshot on demand", () => {
  using db = open(url);
  seed(db);
  // cadenceMs is configurable; we exercise the one-shot path so the test needs
  // no timers. start()/stop() drive the same materializeOnce on an interval.
  const m = new Materializer(db, { table: "events", dir, cadenceMs: 60_000, format: "csv" });
  const snap = m.materializeOnce();
  expect(snap.rows).toBe(5);
  expect(existsSync(snap.path)).toBe(true);
  m.stop(); // no-op when not started — must not throw
});

test("rejects unsafe table/column identifiers (SQL/CLI/path injection guard)", () => {
  using db = open(url);
  seed(db);
  for (const bad of ["events; DROP TABLE events", "../escape", "ev'ents", "a b", ".hidden"]) {
    expect(() => materialize(db, { table: bad, dir, format: "csv" })).toThrow(/invalid table/);
  }
  expect(() =>
    materialize(db, { table: "events", dir, columns: ["id", "amount); --"], format: "csv" }),
  ).toThrow(/invalid column/);
});

test.skipIf(!duckdbAvailable())(
  "DuckDB aggregates over the Parquet snapshot the row engine wrote",
  () => {
    using db = open(url);
    seed(db);

    const snap = materialize(db, { table: "events", dir, format: "parquet" });
    expect(snap.format).toBe("parquet");
    expect(snap.path.endsWith(".parquet")).toBe(true);
    expect(existsSync(snap.path)).toBe(true);

    // A second engine runs an aggregation over the same dataset. CSV output is
    // stable to assert against.
    const sql = `COPY (SELECT kind, count(*) AS n, sum(amount) AS total
                 FROM '${snap.path}' GROUP BY kind ORDER BY kind) TO '/dev/stdout' (FORMAT CSV, HEADER)`;
    const r = Bun.spawnSync(["duckdb", "-c", sql]);
    expect(r.success).toBe(true);
    const out = r.stdout.toString().trim().split("\n");
    expect(out[0]).toBe("kind,n,total");
    expect(out).toContain("click,3,5.0");
    expect(out).toContain("view,2,4.0");
  },
);
