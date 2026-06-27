// Next.js (App Router) integration sketch for @twilldb/node.
//
// The engine is embedded in the Node server process that renders your routes —
// no separate database service. Open ONE Database per process and reuse it
// across requests (a module-level singleton), because the engine is a single
// writer per database; opening per-request would thrash the WAL.
//
// Place this as app/api/notes/route.ts in a Next.js project, then
// `import { open } from "@twilldb/node"`.
//
// IMPORTANT: keep the DB on the Node.js runtime, not the Edge runtime — FFI is
// unavailable on Edge. Add `export const runtime = "nodejs"` to the route.

import { open, type Database } from "../src/index.ts";

export const runtime = "nodejs"; // FFI needs the Node runtime, not Edge

// Module-scope singleton: one engine handle per server process, reused across
// requests. globalThis guards against double-open under dev hot-reload.
const g = globalThis as unknown as { __twill?: Database };
function db(): Database {
  if (!g.__twill) {
    g.__twill = open(process.env.TWILLDB_URL ?? "file://./app.db");
    g.__twill.exec(`CREATE TABLE IF NOT EXISTS notes (
      id   INTEGER PRIMARY KEY,
      body TEXT NOT NULL
    )`);
  }
  return g.__twill;
}

// GET /api/notes  → list
export async function GET(): Promise<Response> {
  const rows = db().query("SELECT id, body FROM notes ORDER BY id DESC LIMIT 100");
  return Response.json(rows);
}

// POST /api/notes  { body }  → insert
export async function POST(req: Request): Promise<Response> {
  const { body } = (await req.json()) as { body: string };
  const conn = db();
  conn.transaction((tx) => {
    tx.query("INSERT INTO notes (body) VALUES (?)", [body]);
  });
  return Response.json({ ok: true, lsn: conn.lastLsn.toString() }, { status: 201 });
}
