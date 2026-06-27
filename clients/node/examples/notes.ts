// A runnable embedded sample for @twilldb/node — the Node twin of
// clients/bun/examples/notes.ts. The engine runs in-process via koffi; there is
// no server to start. The storage backend is chosen by the URL scheme.
//
//   cargo build -p twill-engine --release
//   node clients/node/examples/notes.ts
//
// (Node >= 22.18 runs .ts directly via type stripping. Override the engine
// location with TWILLDB_ENGINE_PATH if needed.)

import { open } from "../src/index.ts";

const url = process.env.TWILLDB_URL ?? "file://./notes.db";
console.log(`opening ${url}`);

const db = open(url);
try {
  db.exec(`CREATE TABLE IF NOT EXISTS notes (
    id     INTEGER PRIMARY KEY,
    body   TEXT NOT NULL,
    weight REAL
  )`);
  db.exec("DELETE FROM notes");

  const insert = db.prepare("INSERT INTO notes (id, body, weight) VALUES (?, ?, ?)");
  try {
    insert.run(1, "buy milk", 1.0);
    insert.run(2, "write spec", 2.5);
    insert.run(3, "ship it", 9.9);
  } finally {
    insert.finalize();
  }

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
  console.log(`\nlast commit LSN: ${db.lastLsn}`);
} finally {
  db.close();
}
