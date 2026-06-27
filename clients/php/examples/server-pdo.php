<?php

declare(strict_types=1);

/**
 * The OTHER way to reach Twill DB from PHP: server mode. Run `engine-server`
 * (the same engine behind a Postgres-wire listener) and connect with the
 * standard PDO pgsql driver — no FFI, no native library in the PHP process. This
 * is the path most PHP frameworks (Laravel, Symfony, CodeIgniter) take, because
 * they already speak to Postgres through PDO.
 *
 *   # 1. start the server (file:// or s3://):
 *   cargo run -p twill-server -- --listen 127.0.0.1:5433 --db file://./srv.db
 *
 *   # 2. run this client (cleartext — sslmode=disable):
 *   php examples/server-pdo.php
 *
 * In a framework, you don't write this by hand — you point the framework's
 * Postgres connection at the listener. For example, Laravel's config/database.php
 * 'pgsql' connection with host=127.0.0.1, port=5433, sslmode=disable; CodeIgniter
 * uses the Postgre driver with the same DSN.
 */

$dsn = \getenv('TWILLDB_DSN') ?: 'pgsql:host=127.0.0.1;port=5433;dbname=main;sslmode=disable';

$pdo = new PDO($dsn, 'postgres', '', [
    PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION,
]);

$pdo->exec('CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)');

// Parameterized — values bind as protocol parameters, never string-interpolated.
$stmt = $pdo->prepare('INSERT INTO notes (id, body) VALUES (?, ?)');
$stmt->execute([1, 'hello from PDO']);

$rows = $pdo->query('SELECT id, body FROM notes ORDER BY id')->fetchAll(PDO::FETCH_ASSOC);
foreach ($rows as $row) {
    echo "  #{$row['id']}  {$row['body']}\n";
}
