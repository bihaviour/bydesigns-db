# @twilldb/node

Embed the [Twill DB](https://github.com/bihaviour/twill-db) engine **in-process
on Node** — and on every Node-based framework runtime (Next.js, Astro, Nuxt,
Remix, SvelteKit, Vite SSR). It is the Node twin of
[`@twilldb/bun`](../bun): the *same* public API, the *same* native engine, bound
through [koffi](https://koffi.dev) instead of `bun:ffi`.

```ts
import { open } from "@twilldb/node";

const db = open("file://./local.db");
db.exec("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)");
db.query("INSERT INTO notes (id, body) VALUES (?, ?)", [1, "hello"]);
const rows = db.query("SELECT * FROM notes");
db.close();
```

The storage backend is chosen entirely by the connection-string scheme:
`file://` is pure-embedded; `s3://` / `r2://` / `gs://` is storage-disaggregated.
Your code is identical for both — only the URL changes.

## Install

```bash
npm install @twilldb/node      # or pnpm / yarn / bun add
```

The native engine ships as a per-platform optional dependency
(`@twilldb/engine-<os>-<arch>` — the *same* binaries `@twilldb/bun` uses), so
there is no `cargo build`. To point at a libengine you built yourself:

```bash
TWILLDB_ENGINE_PATH=/abs/path/libengine.so node app.ts
```

## Runtime requirements

- **Node ≥ 22.18** to run `.ts` directly (type stripping). On older Node, build
  with `tsc`/`tsx` first.
- Keep the engine on the **Node.js runtime**, not the **Edge runtime** — FFI is
  unavailable on Edge. In Next.js, add `export const runtime = "nodejs"` to any
  route that touches the database.
- `using db = open(...)` (explicit resource management) needs a runtime with
  `using` support; on Node today, prefer `try { … } finally { db.close() }`.

## API

Identical to `@twilldb/bun` — see that package's docs. In brief:

| Call | Effect |
| --- | --- |
| `open(url)` | open a database (backend chosen by URL scheme) |
| `db.exec(sql)` | run DDL/DML, returns rows affected |
| `db.query(sql, params?)` | buffered rows; params bind positionally (`?`) |
| `db.prepare(sql)` | reusable prepared statement (`.all` / `.get` / `.run`) |
| `db.transaction(fn)` | BEGIN/COMMIT, rollback on throw; commit blocks until durable |
| `db.branch(name)` | copy-on-write branch at the current LSN |
| `db.close()` | release the handle (idempotent) |

Errors throw `EngineError` carrying the numeric `EngineStatus` and a `retryable`
flag (set for conflict / transient-storage failures).

## Frameworks

Open **one** `Database` per server process and reuse it across requests (the
engine is a single writer per database). The `examples/` folder shows the
pattern:

- `examples/notes.ts` — a plain embedded script.
- `examples/nextjs-route.ts` — a Next.js App Router route handler.
- `examples/astro-endpoint.ts` — an Astro API route (same shape as SvelteKit /
  Nuxt / Vite SSR).

## Test

```bash
cargo build -p twill-engine --release
node --test clients/node/test/embedded.test.ts
```
