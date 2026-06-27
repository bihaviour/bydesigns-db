# twilldb/twilldb (PHP)

Use [Twill DB](https://github.com/bihaviour/twill-db) from PHP — two ways:

1. **Embedded** — bind the engine's C ABI in-process through PHP's built-in
   **FFI extension**. Function-call latency, no server, no socket. This is the
   PHP twin of [`@twilldb/bun`](../bun) / [`@twilldb/node`](../node): the *same*
   engine and the *same* mental model.
2. **Server** — run [`engine-server`](../../crates/server) and connect with the
   standard **PDO pgsql** driver. This is what frameworks (Laravel, Symfony,
   CodeIgniter) use, because they already speak Postgres.

```php
use Twill\Database;

$db = Twill\Database::open('file://./local.db');
$db->exec('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)');
$db->query('INSERT INTO notes (id, body) VALUES (?, ?)', [1, 'hello']);
$rows = $db->query('SELECT * FROM notes');
$db->close();
```

The storage backend is chosen entirely by the connection-string scheme:
`file://` is pure-embedded; `s3://` / `r2://` / `gs://` is storage-disaggregated.

## Install

```bash
composer require twilldb/twilldb
```

Requires PHP ≥ 8.1 with the **FFI** extension enabled (`extension=ffi` and
`ffi.enable=1` in `php.ini`, or `php -d ffi.enable=1`). The embedded path also
needs the native `libengine` for your platform: build it with
`cargo build -p twill-engine --release`, or set `TWILLDB_ENGINE_PATH` to a built
`libengine.{so,dylib,dll}`.

## Embedded API

| Call | Effect |
| --- | --- |
| `Database::open($url)` | open a database (backend chosen by URL scheme) |
| `$db->exec($sql)` | run DDL/DML, returns rows affected |
| `$db->query($sql, $params = [])` | buffered rows; params bind positionally (`?`) |
| `$db->prepare($sql)` | reusable `Statement` (`->all()` / `->get()` / `->run()`) |
| `$db->transaction($fn)` | BEGIN/COMMIT, rollback on throw; commit blocks until durable |
| `$db->branch($name)` | copy-on-write branch at the current LSN |
| `$db->lastLsn()` | commit LSN of the last commit |
| `$db->close()` | release the handle (idempotent) |

Failures throw `Twill\EngineError` carrying the numeric `EngineStatus`
(`->status`) and a `->retryable` flag (set for conflict / transient-storage
faults).

## Frameworks

- **Embedded**: open one `Database` in a singleton/service-provider and reuse it
  across requests — the engine is a single writer per database. Keep it on a
  long-lived worker (FrankenPHP, RoadRunner, Swoole, `php-fpm` with a shared
  service) rather than re-opening per request.
- **Server (PDO)**: point the framework's Postgres connection at the listener
  (`host=127.0.0.1 port=5433 sslmode=disable`). See `examples/server-pdo.php`.
  In Laravel that's a `pgsql` connection in `config/database.php`; in CodeIgniter
  it's the Postgre driver with the same DSN.

## Examples

- `examples/notes.php` — embedded (FFI).
- `examples/server-pdo.php` — server mode (PDO pgsql against `engine-server`).

## Test

```bash
cargo build -p twill-engine --release
composer test        # or: php -d ffi.enable=1 test/embedded_test.php
```
