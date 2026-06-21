// A runnable sample of the embedded path — the Phase 1 payoff: a working,
// persistent, in-process database in a Bun app with zero infrastructure.
//
//   cargo build -p twill-engine --release
//   cd clients/bun && bun run examples/notes.ts
//
// (The library is auto-discovered from ../../../target/{release,debug}; or set
//  TWILLDB_ENGINE_PATH to a built libengine.*)

import { open } from "../src/index";

const url = "file://./notes.db";
console.log(`opening ${url}`);

// `using` ⇒ db.close() runs deterministically at scope exit.
using db = open(url);

db.exec(`CREATE TABLE IF NOT EXISTS notes (
  id     INTEGER PRIMARY KEY,
  body   TEXT NOT NULL,
  weight REAL
)`);

// Start fresh each run for a deterministic demo.
db.exec("DELETE FROM notes");

// Parameterized inserts (values bind positionally; no string interpolation).
using insert = db.prepare("INSERT INTO notes (id, body, weight) VALUES (?, ?, ?)");
insert.run(1, "buy milk", 1.0);
insert.run(2, "write spec", 2.5);
insert.run(3, "ship phase 1", 9.9);

// A transaction: commit on normal return; the commit blocks until durable.
db.transaction((tx) => {
  tx.exec("UPDATE notes SET weight = weight + 1 WHERE id = 1");
  tx.exec("INSERT INTO notes (id, body, weight) VALUES (4, 'tidy up', 0.5)");
});

console.log("\nall notes (by weight desc):");
for (const row of db.query<{ id: string; body: string; weight: string }>(
  "SELECT id, body, weight FROM notes ORDER BY weight DESC",
)) {
  console.log(`  #${row.id}  ${row.body.padEnd(16)} ${row.weight}`);
}

const [{ n, total }] = db.query<{ n: string; total: string }>(
  "SELECT COUNT(*) AS n, SUM(weight) AS total FROM notes",
);
console.log(`\n${n} notes, total weight ${total}`);
console.log(`last commit LSN: ${db.lastLsn}`);
console.log(`\npersisted to ${url} — re-run to see it reload from the WAL.`);
