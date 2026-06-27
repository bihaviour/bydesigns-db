// End-to-end embedded tests for @twilldb/node, run on the Node test runner.
// Mirrors clients/bun/test/embedded.test.ts: the public surface is identical, so
// the same scenarios must pass over koffi as over bun:ffi.
//
//   cargo build -p twill-engine --release
//   node --test clients/node/test/
//
// The engine library is auto-discovered from target/{release,debug}; override
// with TWILLDB_ENGINE_PATH.

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { open, EngineError, type Database } from "../src/index.ts";

function withDb(fn: (db: Database, dir: string) => void): void {
  const dir = mkdtempSync(join(tmpdir(), "twilldb-node-"));
  const db = open(`file://${join(dir, "t.db")}`);
  try {
    fn(db, dir);
  } finally {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
}

test("exec / parameterized insert / query round-trip", () => {
  withDb((db) => {
    db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, weight REAL)");
    const n = db.query("INSERT INTO notes (id, body, weight) VALUES (?, ?, ?)", [1, "hello", 1.5]);
    assert.equal(Array.isArray(n), true);

    const rows = db.query<{ id: string; body: string; weight: string }>(
      "SELECT id, body, weight FROM notes WHERE id = ?",
      [1],
    );
    assert.equal(rows.length, 1);
    assert.equal(rows[0].body, "hello");
    assert.equal(rows[0].weight, "1.5");
  });
});

test("prepared statement reused across iterations", () => {
  withDb((db) => {
    db.exec("CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT)");
    const ins = db.prepare("INSERT INTO kv (k, v) VALUES (?, ?)");
    for (let i = 0; i < 5; i++) ins.run(i, `v${i}`);
    ins.finalize();

    const sel = db.prepare<{ v: string }>("SELECT v FROM kv WHERE k = ?");
    assert.equal(sel.get(3)?.v, "v3");
    assert.equal(sel.all(4).length, 1);
    sel.finalize();
  });
});

test("transaction commits on return, rolls back on throw", () => {
  withDb((db) => {
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)");
    db.exec("INSERT INTO t (id, n) VALUES (1, 0)");

    db.transaction((tx) => {
      tx.exec("UPDATE t SET n = 1 WHERE id = 1");
    });
    assert.equal(db.query<{ n: string }>("SELECT n FROM t WHERE id = 1")[0].n, "1");

    assert.throws(() => {
      db.transaction((tx) => {
        tx.exec("UPDATE t SET n = 99 WHERE id = 1");
        throw new Error("boom");
      });
    });
    // rolled back: still 1
    assert.equal(db.query<{ n: string }>("SELECT n FROM t WHERE id = 1")[0].n, "1");
  });
});

test("SQL NULL maps to JS null", () => {
  withDb((db) => {
    db.exec("CREATE TABLE n (id INTEGER PRIMARY KEY, v TEXT)");
    db.query("INSERT INTO n (id, v) VALUES (?, ?)", [1, null]);
    const row = db.query<{ v: string | null }>("SELECT v FROM n WHERE id = 1")[0];
    assert.equal(row.v, null);
  });
});

test("a typed EngineError is thrown on bad SQL", () => {
  withDb((db) => {
    assert.throws(
      () => db.exec("SELECT * FROM does_not_exist"),
      (e: unknown) => e instanceof EngineError && typeof (e as EngineError).status === "number",
    );
  });
});

test("copy-on-write branch isolates writes", () => {
  withDb((db) => {
    db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)");
    db.exec("INSERT INTO notes (id, body) VALUES (1, 'base')");

    const preview = db.branch("pr-1");
    try {
      preview.exec("UPDATE notes SET body = 'changed' WHERE id = 1");
      assert.equal(
        preview.query<{ body: string }>("SELECT body FROM notes WHERE id = 1")[0].body,
        "changed",
      );
      // base is untouched
      assert.equal(db.query<{ body: string }>("SELECT body FROM notes WHERE id = 1")[0].body, "base");
    } finally {
      preview.close();
    }
  });
});

test("lastLsn advances after a commit", () => {
  withDb((db) => {
    db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY)");
    const before = db.lastLsn;
    db.transaction((tx) => tx.exec("INSERT INTO t (id) VALUES (1)"));
    assert.equal(db.lastLsn > before, true);
  });
});
