// Astro (and, structurally, SvelteKit / Nuxt / Remix / Vite-SSR) integration
// sketch for @twilldb/node. The same one-handle-per-process rule applies: the
// engine is embedded in the Node server that renders pages, so a module-level
// singleton is reused across requests.
//
// Place as src/pages/api/notes.ts in an Astro project (server output, Node
// adapter). The pattern is identical for any Vite-based SSR framework — only the
// route-handler signature changes.

import { open, type Database } from "../src/index.ts";

const g = globalThis as unknown as { __twill?: Database };
function db(): Database {
  if (!g.__twill) {
    g.__twill = open(process.env.TWILLDB_URL ?? "file://./app.db");
    g.__twill.exec("CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)");
  }
  return g.__twill;
}

// Astro APIRoute signature: ({ request }) => Response
export function GET(): Response {
  const rows = db().query("SELECT id, body FROM notes ORDER BY id DESC LIMIT 100");
  return new Response(JSON.stringify(rows), {
    headers: { "content-type": "application/json" },
  });
}

export async function POST({ request }: { request: Request }): Promise<Response> {
  const { body } = (await request.json()) as { body: string };
  db().transaction((tx) => tx.query("INSERT INTO notes (body) VALUES (?)", [body]));
  return new Response(JSON.stringify({ ok: true }), { status: 201 });
}
