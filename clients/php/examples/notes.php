<?php

declare(strict_types=1);

/**
 * A runnable embedded sample for the PHP client — the PHP twin of
 * clients/bun/examples/notes.ts. The engine runs in-process via PHP's FFI
 * extension; there is no server to start. The storage backend is chosen by the
 * URL scheme.
 *
 *   cargo build -p twill-engine --release
 *   php -d ffi.enable=1 clients/php/examples/notes.php
 *
 * Override the engine location with TWILLDB_ENGINE_PATH if needed.
 */

require __DIR__ . '/../src/autoload.php';

use Twill\Database;

$url = \getenv('TWILLDB_URL') ?: 'file://./notes.db';
echo "opening {$url}\n";

$db = Database::open($url);
try {
    $db->exec('CREATE TABLE IF NOT EXISTS notes (
        id     INTEGER PRIMARY KEY,
        body   TEXT NOT NULL,
        weight REAL
    )');
    $db->exec('DELETE FROM notes');

    $insert = $db->prepare('INSERT INTO notes (id, body, weight) VALUES (?, ?, ?)');
    try {
        $insert->run(1, 'buy milk', 1.0);
        $insert->run(2, 'write spec', 2.5);
        $insert->run(3, 'ship it', 9.9);
    } finally {
        $insert->finalize();
    }

    $db->transaction(static function (Database $tx) {
        $tx->exec('UPDATE notes SET weight = weight + 1 WHERE id = 1');
        $tx->exec("INSERT INTO notes (id, body, weight) VALUES (4, 'tidy up', 0.5)");
    });

    echo "\nall notes (by weight desc):\n";
    foreach ($db->query('SELECT id, body, weight FROM notes ORDER BY weight DESC') as $row) {
        \printf("  #%s  %-16s %s\n", $row['id'], $row['body'], $row['weight']);
    }
    echo "\nlast commit LSN: {$db->lastLsn()}\n";
} finally {
    $db->close();
}
