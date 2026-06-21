// Phase 5 example — composing capabilities AROUND the engine (spec 12).
//
// Run: cargo build -p twill-engine --release
//      bun run examples/compose.ts
//
// The deciding rule (spec 12): storage/execution capabilities go INTO the engine
// (that is vector search, see vector-memory.ts); interface/service capabilities
// are COMPOSED AROUND it and stay optional. This script demonstrates two of the
// three composed layers with the only code we own — the thin glue — and points at
// where the unmodified third parties attach:
//
//   * better-auth  -> in-process library; its state lives in the embedded engine,
//                     so it branches and recovers with the database (shown below).
//   * DuckDB (OLAP) -> a second engine over the SAME storage floor; we publish an
//                     open columnar snapshot it reads directly (the materialization
//                     job — shown below; here CSV, which DuckDB reads via
//                     read_csv_auto; Parquet/Iceberg is the production format).
//   * PostgREST    -> attaches over SERVER MODE (pgwire, Phase 3) with zero engine
//                     changes; not embedded, so it is simply absent here.

import { open, type Database } from "../src/index";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { mkdirSync, renameSync, rmSync, writeFileSync } from "node:fs";

const dbFile = join(tmpdir(), `compose-${process.pid}.db`);
const url = `file://${dbFile}`;
const snapshotDir = join(tmpdir(), `compose-olap-${process.pid}`);

// ---- better-auth shape: auth state persisted IN the embedded engine ----------
// No external auth service; a session lookup is a local function call. Because
// the state is ordinary rows, branching the database branches the auth state too.
function setupAuth(db: Database): void {
  db.exec("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT, name TEXT)");
  db.exec("CREATE TABLE sessions (token TEXT PRIMARY KEY, user_id INTEGER)");
  db.exec("INSERT INTO users VALUES (1, 'ada@example.com', 'Ada')");
  db.exec("INSERT INTO sessions VALUES ('sess-abc', 1)");
}

function whoami(db: Database, token: string): string {
  // Two-step lookup (subqueries are outside the SQL subset) — which is exactly
  // what an in-process auth library does: resolve the session, then the user.
  const sess = db.query<{ user_id: string }>(
    "SELECT user_id FROM sessions WHERE token = ?",
    [token],
  );
  if (sess.length === 0) return "(no session)";
  const user = db.query<{ name: string }>("SELECT name FROM users WHERE id = ?", [
    Number(sess[0].user_id),
  ]);
  return user[0]?.name ?? "(no user)";
}

// ---- DuckDB shape: publish an open columnar snapshot it reads directly --------
// The materialization "thin glue": snapshot a table to the shared store in an
// open format. Published atomically (temp + rename) so a reader only ever sees a
// whole snapshot (spec 12 failure-mode mitigation).
function materializeCsv(db: Database, table: string, columns: string[], dir: string): string {
  mkdirSync(dir, { recursive: true });
  const rows = db.query<Record<string, string | null>>(
    `SELECT ${columns.join(", ")} FROM ${table}`,
  );
  const header = columns.join(",");
  const body = rows.map((r) => columns.map((c) => csvCell(r[c])).join(",")).join("\n");
  const finalPath = join(dir, `${table}.csv`);
  const tmpPath = `${finalPath}.tmp`;
  writeFileSync(tmpPath, rows.length ? `${header}\n${body}\n` : `${header}\n`);
  renameSync(tmpPath, finalPath); // atomic publish
  return finalPath;
}

function csvCell(v: string | null): string {
  if (v === null) return "";
  return /[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v;
}

function main(): void {
  using db = open(url);
  setupAuth(db);
  console.log("auth (in-process):", whoami(db, "sess-abc"));

  // Auth state branches with the database — a staging branch gets its own users.
  using staging = db.branch("staging");
  staging.exec("INSERT INTO users VALUES (2, 'bel@example.com', 'Bel')");
  staging.exec("INSERT INTO sessions VALUES ('sess-xyz', 2)");
  console.log("auth on staging branch:", whoami(staging, "sess-xyz"));
  console.log("base never saw the staging user:", whoami(db, "sess-xyz"));

  // Application data the OLAP side will read.
  db.exec("CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, amount REAL)");
  db.exec("INSERT INTO events VALUES (1, 'click', 1.5), (2, 'view', 0.0), (3, 'click', 2.5)");

  const path = materializeCsv(db, "events", ["id", "kind", "amount"], snapshotDir);
  console.log("OLAP snapshot published:", path);
  console.log(
    "DuckDB reads it unmodified, e.g.:\n",
    `  duckdb -c "SELECT kind, sum(amount) FROM read_csv_auto('${path}') GROUP BY kind"`,
  );
}

try {
  main();
} finally {
  rmSync(dbFile, { force: true });
  rmSync(snapshotDir, { recursive: true, force: true });
}
