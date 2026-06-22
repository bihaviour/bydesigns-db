# Connection pooling for engine-server (issue #20)

Serverless clients open many short-lived connections; `engine-server`'s write
path is a single lane per database. A **transaction-mode** pooler
([PgBouncer](https://www.pgbouncer.org/) or [pgcat](https://github.com/postgresml/pgcat))
sits in front of `engine-server`, absorbs the connection burst on its client
edge, and multiplexes it onto a small, stable pool of backend connections —
returning a backend to the pool at each `COMMIT`/`ROLLBACK`. The pooler is a
separate process; it is composed in front, never bundled into the server.

See the user guide: [`pages/docs/connection-pooling.html`](../../pages/docs/connection-pooling.html).

## Files

| File | Pooler | Notes |
|------|--------|-------|
| [`pgbouncer.ini`](pgbouncer.ini) | PgBouncer | Minimal transaction-mode config. |
| [`pgcat.toml`](pgcat.toml) | pgcat | Same pooling; use when you also want load-balancing/sharding. |

Both listen on `:6432` and forward to an `engine-server` on `:5433`. They keep
`min_pool_size = 0` so idle backends drain and the engine can scale-to-zero.

## Run it

```bash
# 1. Start engine-server (either backend; file:// shown for a local run).
cargo run -p twill-server -- --listen 127.0.0.1:5433 --db file://./srv.db

# 2. Start the pooler in front (pick one).
pgbouncer deploy/pooler/pgbouncer.ini
#   or
pgcat deploy/pooler/pgcat.toml

# 3. Point any Postgres client at the pooler (port 6432), not the server.
psql "host=127.0.0.1 port=6432 user=postgres dbname=srv sslmode=disable"
```

## Bun.sql against the pooled endpoint

Bun's built-in `Bun.sql` is a Postgres client, so it connects to the pooler with
an ordinary connection string. Transaction mode returns a backend at each
transaction boundary, so wrap related statements in `sql.begin()`:

```ts
import { SQL } from "bun";

// Point at the POOLER (6432), not engine-server (5433). Cleartext: sslmode=disable.
const sql = new SQL("postgres://postgres@127.0.0.1:6432/srv?sslmode=disable");

await sql`CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT)`;

// One transaction = one borrowed backend, returned to the pool at commit.
await sql.begin(async (tx) => {
  await tx`INSERT INTO notes VALUES (1, 'hello from Bun.sql')`;
});

const rows = await sql`SELECT body FROM notes WHERE id = ${1}`;
console.log(rows); // [{ body: "hello from Bun.sql" }]

await sql.end();
```

## Verifying burst absorption

The engine-side correctness that pooling relies on — a bounded backend pool
carrying the whole transaction load with no lost or duplicated commits — is
covered automatically by the server-mode test
`crates/bench/tests/pgwire.rs::pgwire_transaction_mode_pooling_preserves_correctness`
(runs in `cargo test`, no external pooler needed).

To soak the **real pooler** end-to-end, drive the pooled endpoint with `pgbench`
in transaction-heavy, short-connection mode (`-C` opens a fresh connection per
transaction — the serverless burst the pooler is there to absorb):

```bash
pgbench -h 127.0.0.1 -p 6432 -U postgres -d srv \
  --no-vacuum -C -c 200 -j 8 -T 30 -M extended
```

A healthy run shows the pooler holding `default_pool_size` backend connections
open to `engine-server` while serving the much larger `-c 200` client fan-in,
and `engine-server`'s own connection count staying at the pool size rather than
tracking the client count.
