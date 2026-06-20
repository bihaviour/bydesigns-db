// End-to-end test of the embedded path: @yourdb/bun -> bun:ffi -> libengine,
// backed by a file:// database. Covers exec/query/prepared/transaction, MVCC
// snapshot isolation across two handles, persistence across reopen, NULL
// handling, typed errors, and deterministic disposal.
//
// Run: YOURDB_ENGINE_PATH=/abs/path/to/libengine.so bun test
// (the loader also auto-discovers target/{release,debug} in this repo).

import { test, expect, beforeEach, afterEach } from "bun:test";
import { open, EngineError, type Database } from "../src/index";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rmSync } from "node:fs";

let dbFile: string;
let url: string;

beforeEach(() => {
  dbFile = join(tmpdir(), `yourdb-bun-${process.pid}-${Math.random().toString(36).slice(2)}.db`);
  url = `file://${dbFile}`;
});

afterEach(() => {
  try {
    rmSync(dbFile, { force: true });
  } catch {}
});

test("exec, query, and rows-affected", () => {
  using db = open(url);
  db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)");
  const n = db.exec("INSERT INTO notes VALUES (1, 'hello'), (2, 'world')");
  expect(n).toBe(2);

  const rows = db.query<{ id: string; body: string }>("SELECT id, body FROM notes ORDER BY id");
  expect(rows).toEqual([
    { id: "1", body: "hello" },
    { id: "2", body: "world" },
  ]);
});

test("parameterized query and prepared statements", () => {
  using db = open(url);
  db.exec("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)");

  using ins = db.prepare("INSERT INTO u (id, name) VALUES (?, ?)");
  expect(ins.run(1, "ada")).toBe(1);
  expect(ins.run(2, "bel")).toBe(1);
  expect(ins.run(3, "cyn")).toBe(1);

  const one = db.query<{ name: string }>("SELECT name FROM u WHERE id = ?", [2]);
  expect(one).toEqual([{ name: "bel" }]);

  using sel = db.prepare<{ name: string }>("SELECT name FROM u WHERE id = ?");
  expect(sel.get(3)).toEqual({ name: "cyn" });
  expect(sel.get(999)).toBeUndefined();
  expect(sel.all().length).toBe(0); // re-bind: id = NULL matches nothing
});

test("transaction commits on return and rolls back on throw", () => {
  using db = open(url);
  db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)");

  db.transaction((tx) => {
    tx.exec("INSERT INTO t VALUES (1)");
    tx.exec("INSERT INTO t VALUES (2)");
  });
  expect(db.query("SELECT COUNT(*) AS c FROM t")[0]).toEqual({ c: "2" });
  expect(db.lastLsn > 0n).toBe(true);

  expect(() =>
    db.transaction((tx) => {
      tx.exec("INSERT INTO t VALUES (3)");
      throw new Error("boom");
    }),
  ).toThrow("boom");
  // The rolled-back insert is gone.
  expect(db.query("SELECT COUNT(*) AS c FROM t")[0]).toEqual({ c: "2" });
});

test("MVCC snapshot isolation across two handles", () => {
  {
    using setup = open(url);
    setup.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
    setup.exec("INSERT INTO t VALUES (1, 100)");
  }

  using reader = open(url);
  using writer = open(url);

  // Hold a read snapshot on `reader` across a concurrent committed write by
  // `writer`, driving BEGIN/COMMIT explicitly so the snapshot spans the write.
  const count = (db: Database) => (db.query("SELECT COUNT(*) AS c FROM t")[0] as { c: string }).c;

  reader.exec("BEGIN");
  const before = count(reader);
  writer.exec("INSERT INTO t VALUES (2, 200)"); // concurrent committed write
  const during = count(reader);
  reader.exec("COMMIT");
  const after = count(reader);

  expect(before).toBe("1");
  expect(during).toBe("1"); // snapshot is stable across the concurrent write
  expect(after).toBe("2"); // a fresh snapshot sees the committed write
});

test("persists across reopen (WAL durability + replay)", () => {
  {
    using db = open(url);
    db.exec("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)");
    db.exec("INSERT INTO kv VALUES ('a', 1), ('b', 2)");
    db.exec("UPDATE kv SET v = 20 WHERE k = 'b'");
  }
  using db = open(url);
  const rows = db.query("SELECT k, v FROM kv ORDER BY k");
  expect(rows).toEqual([
    { k: "a", v: "1" },
    { k: "b", v: "20" },
  ]);
});

test("SQL NULL maps to JS null", () => {
  using db = open(url);
  db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT)");
  db.exec("INSERT INTO t VALUES (1, NULL)");
  const rows = db.query<{ id: string; note: string | null }>("SELECT id, note FROM t");
  expect(rows[0].id).toBe("1");
  expect(rows[0].note).toBeNull();
});

test("typed errors carry status and retryable flag", () => {
  using db = open(url);
  db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)");
  db.exec("INSERT INTO t VALUES (1)");

  let caught: EngineError | null = null;
  try {
    db.exec("INSERT INTO t VALUES (1)"); // duplicate PK
  } catch (e) {
    caught = e as EngineError;
  }
  expect(caught).toBeInstanceOf(EngineError);
  expect(caught!.status).toBe(2); // ENGINE_ERR_CONSTRAINT
  expect(caught!.retryable).toBe(false);

  // A parse error is ENGINE_ERR_SQL.
  expect(() => db.query("SELEKT 1")).toThrow(EngineError);
});

test("branch is reserved for Phase 4", () => {
  using db = open(url);
  expect(() => db.branch("preview")).toThrow(/Phase 4/);
});

test("use-after-close is misuse, not a crash", () => {
  const db: Database = open(url);
  db.close();
  db.close(); // idempotent
  expect(() => db.exec("SELECT 1")).toThrow(/closed/);
});
