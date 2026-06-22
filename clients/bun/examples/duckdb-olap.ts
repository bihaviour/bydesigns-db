// Phase 5 example (issue #28) — DuckDB composed over the same storage floor.
//
// Run: cargo build -p twill-engine --release
//      cd clients/bun && bun run examples/duckdb-olap.ts
//      (install DuckDB for the Parquet path: `brew install duckdb`)
//
// HTAP by composition, not by making the row engine columnar (spec 12): the
// Twill engine stays OLTP, and a SECOND engine — DuckDB — runs analytics over
// the SAME data. The only code we own is the thin materialization job
// (`../src/olap`): it snapshots a table to an open columnar file (Parquet) on
// the shared store, atomically, on a configurable cadence. DuckDB reads those
// snapshots unmodified. Possible precisely because storage is decoupled.

import { open } from "../src/index";
import { materialize, duckdbAvailable } from "../src/olap";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rmSync } from "node:fs";

const dbFile = join(tmpdir(), `olap-${process.pid}.db`);
const url = `file://${dbFile}`;
const snapshotDir = join(tmpdir(), `olap-snap-${process.pid}`);

function main(): void {
  using db = open(url);

  // OLTP side: the row engine serves transactional writes as usual.
  db.exec("CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, amount REAL)");
  db.exec(
    "INSERT INTO events VALUES (1,'click',1.5),(2,'view',0.0),(3,'click',2.5),(4,'view',4.0),(5,'click',1.0)",
  );

  // Materialization job (the thin glue we own): publish an open columnar
  // snapshot. In production call this on a cadence (see Materializer); the
  // snapshot lands on the same object store the database syncs to.
  const snap = materialize(db, { table: "events", dir: snapshotDir, format: "parquet" });
  console.log(`published ${snap.format} snapshot: ${snap.path} (${snap.rows} rows)`);

  // OLAP side: DuckDB runs an aggregation over the snapshot — a second engine
  // over the same dataset, never touching the row engine.
  const agg = `SELECT kind, count(*) AS n, sum(amount) AS total
               FROM '${snap.path}' GROUP BY kind ORDER BY kind`;
  if (duckdbAvailable()) {
    const r = Bun.spawnSync(["duckdb", "-c", agg]);
    console.log("DuckDB aggregation over the snapshot:\n" + r.stdout.toString().trimEnd());
  } else {
    console.log("DuckDB not installed — run this to query the snapshot:\n  duckdb -c \"" + agg + "\"");
  }
}

try {
  main();
} finally {
  rmSync(dbFile, { force: true });
  rmSync(snapshotDir, { recursive: true, force: true });
}
