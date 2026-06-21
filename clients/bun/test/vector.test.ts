// Phase 5 — vector search through @twilldb/bun (bun:ffi -> libengine). Covers the
// vector(N) type, an HNSW index, a top-k nearest-neighbour query (with a vector
// passed as a `number[]` parameter), and the headline payoff: branching the
// database branches the vector index, so an agent can fork its memory.
//
// Run: TWILLDB_ENGINE_PATH=/abs/path/to/libengine.so bun test
// (the engine must be a release build at ABI v3; rebuild after engine changes.)

import { test, expect, beforeEach, afterEach } from "bun:test";
import { open } from "../src/index";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rmSync } from "node:fs";

let dbFile: string;
let url: string;

beforeEach(() => {
  dbFile = join(tmpdir(), `twilldb-vec-${process.pid}-${Math.random().toString(36).slice(2)}.db`);
  url = `file://${dbFile}`;
});

afterEach(() => {
  try {
    rmSync(dbFile, { force: true });
  } catch {}
});

function seedMemories(db: ReturnType<typeof open>) {
  db.exec("CREATE TABLE memories (id INTEGER PRIMARY KEY, note TEXT, embedding VECTOR(3))");
  db.exec("CREATE INDEX mem_e ON memories USING hnsw (embedding) WITH (metric = 'cosine')");
  db.exec("INSERT INTO memories VALUES (1, 'apples', [1, 0, 0])");
  db.exec("INSERT INTO memories VALUES (2, 'oranges', [0, 1, 0])");
  db.exec("INSERT INTO memories VALUES (3, 'bananas', [0, 0, 1])");
}

test("vector(N) column round-trips as a [..] literal", () => {
  using db = open(url);
  db.exec("CREATE TABLE v (id INTEGER PRIMARY KEY, e VECTOR(3))");
  db.exec("INSERT INTO v VALUES (1, [1, 2, 3])");
  const rows = db.query<{ e: string }>("SELECT e FROM v");
  expect(rows[0].e).toBe("[1,2,3]");
});

test("HNSW top-k nearest neighbour, query vector as a parameter", () => {
  using db = open(url);
  seedMemories(db);

  // A number[] parameter is encoded as a vector and pushed into the index scan.
  const near = db.query<{ id: string }>(
    "SELECT id FROM memories ORDER BY embedding <=> ? LIMIT 2",
    [[0.9, 0.1, 0]],
  );
  expect(near.map((r) => r.id)).toEqual(["1", "2"]);
});

test("branching forks the vector index (agent memory fork)", () => {
  using db = open(url);
  seedMemories(db);

  // Fork the agent's memory; experiment on the fork without touching the base.
  using fork = db.branch("speculative-episode");
  fork.exec("INSERT INTO memories VALUES (4, 'grapes', [0.1, 0.1, 0.95])");

  // The query [0.2,0.2,0.9] leans toward 'grapes' more than the exact-axis id 3.
  const nearestInFork = fork.query<{ id: string }>(
    "SELECT id FROM memories ORDER BY embedding <=> ? LIMIT 1",
    [[0.2, 0.2, 0.9]],
  );
  expect(nearestInFork[0].id).toBe("4");

  // The base never learned 'grapes': its nearest direction is still id 3.
  const nearestInBase = db.query<{ id: string }>(
    "SELECT id FROM memories ORDER BY embedding <=> ? LIMIT 1",
    [[0.2, 0.2, 0.9]],
  );
  expect(nearestInBase[0].id).toBe("3");
});

test("vector index survives reopen (rebuilt from the WAL)", () => {
  {
    using db = open(url);
    seedMemories(db);
  }
  using db = open(url);
  const near = db.query<{ id: string }>(
    "SELECT id FROM memories ORDER BY embedding <=> ? LIMIT 1",
    [[0, 1, 0]],
  );
  expect(near[0].id).toBe("2");
});
