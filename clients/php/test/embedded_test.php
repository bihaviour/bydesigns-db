<?php

declare(strict_types=1);

/**
 * End-to-end embedded tests for the PHP client — the PHP twin of
 * clients/bun/test/embedded.test.ts. A dependency-free runner (no PHPUnit): each
 * check asserts and the script exits non-zero on the first failure.
 *
 *   cargo build -p twill-engine --release
 *   php -d ffi.enable=1 clients/php/test/embedded_test.php
 *
 * The engine library is auto-discovered from target/{release,debug}; override
 * with TWILLDB_ENGINE_PATH.
 */

require __DIR__ . '/../src/autoload.php';

use Twill\Database;
use Twill\EngineError;

$failures = 0;
$count = 0;

function check(string $name, callable $fn): void
{
    global $failures, $count;
    $count++;
    try {
        $fn();
        echo "ok   - {$name}\n";
    } catch (\Throwable $e) {
        $failures++;
        echo "FAIL - {$name}: {$e->getMessage()}\n";
    }
}

function assertTrue(bool $cond, string $msg = 'assertion failed'): void
{
    if (!$cond) {
        throw new \RuntimeException($msg);
    }
}

function assertSame(mixed $expected, mixed $actual, string $msg = ''): void
{
    if ($expected !== $actual) {
        throw new \RuntimeException(
            ($msg !== '' ? $msg . ': ' : '')
            . 'expected ' . \var_export($expected, true) . ', got ' . \var_export($actual, true)
        );
    }
}

function withDb(callable $fn): void
{
    $dir = \sys_get_temp_dir() . '/twilldb-php-' . \bin2hex(\random_bytes(6));
    \mkdir($dir, 0700, true);
    $db = Database::open('file://' . $dir . '/t.db');
    try {
        $fn($db);
    } finally {
        $db->close();
        @\array_map('unlink', \glob($dir . '/*') ?: []);
        @\rmdir($dir);
    }
}

check('exec / parameterized insert / query round-trip', static function () {
    withDb(static function (Database $db) {
        $db->exec('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT, weight REAL)');
        $db->query('INSERT INTO notes (id, body, weight) VALUES (?, ?, ?)', [1, 'hello', 1.5]);
        $rows = $db->query('SELECT id, body, weight FROM notes WHERE id = ?', [1]);
        assertSame(1, \count($rows));
        assertSame('hello', $rows[0]['body']);
        assertSame('1.5', $rows[0]['weight']);
    });
});

check('prepared statement reused across iterations', static function () {
    withDb(static function (Database $db) {
        $db->exec('CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT)');
        $ins = $db->prepare('INSERT INTO kv (k, v) VALUES (?, ?)');
        for ($i = 0; $i < 5; $i++) {
            $ins->run($i, "v{$i}");
        }
        $ins->finalize();

        $sel = $db->prepare('SELECT v FROM kv WHERE k = ?');
        assertSame('v3', $sel->get(3)['v']);
        assertSame(1, \count($sel->all(4)));
        $sel->finalize();
    });
});

check('transaction commits on return, rolls back on throw', static function () {
    withDb(static function (Database $db) {
        $db->exec('CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)');
        $db->exec('INSERT INTO t (id, n) VALUES (1, 0)');

        $db->transaction(static function (Database $tx) {
            $tx->exec('UPDATE t SET n = 1 WHERE id = 1');
        });
        assertSame('1', $db->query('SELECT n FROM t WHERE id = 1')[0]['n']);

        $threw = false;
        try {
            $db->transaction(static function (Database $tx) {
                $tx->exec('UPDATE t SET n = 99 WHERE id = 1');
                throw new \RuntimeException('boom');
            });
        } catch (\RuntimeException $e) {
            $threw = true;
        }
        assertTrue($threw, 'transaction should re-throw');
        assertSame('1', $db->query('SELECT n FROM t WHERE id = 1')[0]['n'], 'rolled back');
    });
});

check('SQL NULL maps to PHP null', static function () {
    withDb(static function (Database $db) {
        $db->exec('CREATE TABLE n (id INTEGER PRIMARY KEY, v TEXT)');
        $db->query('INSERT INTO n (id, v) VALUES (?, ?)', [1, null]);
        $row = $db->query('SELECT v FROM n WHERE id = 1')[0];
        assertSame(null, $row['v']);
    });
});

check('a typed EngineError is thrown on bad SQL', static function () {
    withDb(static function (Database $db) {
        $caught = null;
        try {
            $db->exec('SELECT * FROM does_not_exist');
        } catch (EngineError $e) {
            $caught = $e;
        }
        assertTrue($caught instanceof EngineError, 'should throw EngineError');
        assertTrue(\is_int($caught->status), 'carries a numeric status');
    });
});

check('copy-on-write branch isolates writes', static function () {
    withDb(static function (Database $db) {
        $db->exec('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)');
        $db->exec("INSERT INTO notes (id, body) VALUES (1, 'base')");

        $preview = $db->branch('pr-1');
        try {
            $preview->exec("UPDATE notes SET body = 'changed' WHERE id = 1");
            assertSame('changed', $preview->query('SELECT body FROM notes WHERE id = 1')[0]['body']);
            assertSame('base', $db->query('SELECT body FROM notes WHERE id = 1')[0]['body']);
        } finally {
            $preview->close();
        }
    });
});

check('lastLsn advances after a commit', static function () {
    withDb(static function (Database $db) {
        $db->exec('CREATE TABLE t (id INTEGER PRIMARY KEY)');
        $before = $db->lastLsn();
        $db->transaction(static fn (Database $tx) => $tx->exec('INSERT INTO t (id) VALUES (1)'));
        assertTrue($db->lastLsn() > $before, 'LSN should advance');
    });
});

echo "\n{$count} checks, {$failures} failed\n";
exit($failures === 0 ? 0 : 1);
