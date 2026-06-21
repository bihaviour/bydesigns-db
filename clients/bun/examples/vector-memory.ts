// Phase 5 example — vector search as agent memory, and branching it.
//
// Run: cargo build -p twill-engine --release
//      bun run examples/vector-memory.ts
//
// Shows the in-core vector capability (spec 12): a vector(N) column, an HNSW
// index, a top-k nearest-neighbour query, and the differentiator pgvector cannot
// do — forking the database forks the vector index, so an agent can branch its
// retrieval memory, run a speculative episode on the fork, and discard it.

import { open, type Database } from "../src/index";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rmSync } from "node:fs";

const dbFile = join(tmpdir(), `agent-memory-${process.pid}.db`);
const url = `file://${dbFile}`;

// A toy 3-d "embedding" so the example needs no model. Real memories would store
// model embeddings of dimension 768/1536.
function topMemory(db: Database, query: number[]): string {
  const rows = db.query<{ note: string }>(
    "SELECT note FROM memories ORDER BY embedding <=> ? LIMIT 1",
    [query],
  );
  return rows[0]?.note ?? "(none)";
}

function main(): void {
  using db = open(url);
  db.exec("CREATE TABLE memories (id INTEGER PRIMARY KEY, note TEXT, embedding VECTOR(3))");
  // HNSW access method, cosine metric — the same Storage trait the rows ride.
  db.exec("CREATE INDEX mem_e ON memories USING hnsw (embedding) WITH (metric = 'cosine')");

  db.exec("INSERT INTO memories VALUES (1, 'likes apples', [1, 0, 0])");
  db.exec("INSERT INTO memories VALUES (2, 'likes oranges', [0, 1, 0])");
  db.exec("INSERT INTO memories VALUES (3, 'likes bananas', [0, 0, 1])");

  console.log("nearest to a citrus-ish query:", topMemory(db, [0.1, 0.9, 0.1]));

  // Fork the memory: a speculative episode learns a new fact on the branch only.
  using fork = db.branch("speculative");
  fork.exec("INSERT INTO memories VALUES (4, 'now likes grapes', [0.2, 0.2, 0.9])");
  console.log("fork, nearest to a grape-ish query:", topMemory(fork, [0.2, 0.2, 0.9]));
  console.log("base, same query (fork is isolated):", topMemory(db, [0.2, 0.2, 0.9]));

  // Discarding the fork costs near-zero storage — only its divergence existed.
  console.log("the fork's index branched with the database; the base never saw it.");
}

try {
  main();
} finally {
  rmSync(dbFile, { force: true });
}
