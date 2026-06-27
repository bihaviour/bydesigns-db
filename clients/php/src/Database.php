<?php

declare(strict_types=1);

namespace Twill;

use FFI;
use FFI\CData;

/**
 * An ergonomic, typed embedding of the Twill DB engine for PHP, over the C ABI
 * bound in {@see Engine}. The public surface mirrors the Bun/Node clients so the
 * mental model is identical across runtimes.
 *
 *   $db = Twill\Database::open('file://./local.db');
 *   $db->exec('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)');
 *   $db->query('INSERT INTO notes (id, body) VALUES (?, ?)', [1, 'hello']);
 *   $rows = $db->query('SELECT * FROM notes');
 *   $db->close();
 */
final class Database
{
    private FFI $ffi;
    private ?CData $handle;

    private function __construct(CData $handle)
    {
        $this->ffi = Engine::ffi();
        $this->handle = $handle;
    }

    /** Open a database. The storage backend is selected by the URL scheme. */
    public static function open(string $url): self
    {
        $ffi = Engine::ffi();
        $handle = $ffi->engine_open($url);
        if (FFI::isNull($handle)) {
            throw new EngineError(
                EngineError::ERR_STORAGE,
                "engine_open failed for \"{$url}\" (unknown scheme or storage init error)"
            );
        }
        return new self($handle);
    }

    private function h(): CData
    {
        if ($this->handle === null) {
            throw new EngineError(EngineError::ERR_MISUSE, 'database is closed');
        }
        return $this->handle;
    }

    /** Run a statement with no result set; returns rows affected. */
    public function exec(string $sql): int
    {
        $h = $this->h();
        $st = $this->ffi->engine_exec($h, $sql);
        if ($st !== EngineError::OK) {
            throw $this->errorFrom($st);
        }
        return (int) $this->ffi->engine_changes($h);
    }

    /**
     * Run a query (optionally parameterized) and buffer all rows.
     *
     * @param list<mixed> $params
     * @return list<array<string, ?string>>
     */
    public function query(string $sql, array $params = []): array
    {
        if ($params !== []) {
            $stmt = $this->prepare($sql);
            try {
                return $stmt->all(...$params);
            } finally {
                $stmt->finalize();
            }
        }
        return $this->bufferedQuery($sql);
    }

    /** Prepare a reusable statement. */
    public function prepare(string $sql): Statement
    {
        $h = $this->h();
        $out = $this->ffi->new('EngineStmt*');
        $st = $this->ffi->engine_prepare($h, $sql, FFI::addr($out));
        if ($st !== EngineError::OK) {
            throw $this->errorFrom($st);
        }
        return new Statement($this->ffi, $h, $out);
    }

    /**
     * BEGIN; run $fn($this); COMMIT on normal return, ROLLBACK (and re-throw) on
     * a thrown exception. The COMMIT blocks until the WAL is durable.
     *
     * @template R
     * @param callable(self):R $fn
     * @return R
     */
    public function transaction(callable $fn): mixed
    {
        $h = $this->h();
        $st = $this->ffi->engine_begin($h);
        if ($st !== EngineError::OK) {
            throw $this->errorFrom($st);
        }
        try {
            $result = $fn($this);
        } catch (\Throwable $e) {
            $this->ffi->engine_rollback($h);
            throw $e;
        }
        $st = $this->ffi->engine_commit($h);
        if ($st !== EngineError::OK) {
            throw $this->errorFrom($st);
        }
        return $result;
    }

    /**
     * Create a copy-on-write branch off this database at its current committed
     * LSN, returning a new Database bound to the branch. The branch sees the
     * base's committed data but writes in isolation. Close it when done.
     */
    public function branch(string $name): self
    {
        $h = $this->h();
        $child = $this->ffi->engine_branch($h, $name);
        if (FFI::isNull($child)) {
            throw $this->errorFrom(EngineError::ERR_INTERNAL);
        }
        return new self($child);
    }

    /** Commit LSN of the last commit on this connection. */
    public function lastLsn(): int
    {
        return (int) $this->ffi->engine_last_lsn($this->h());
    }

    /** Release the handle. Idempotent. */
    public function close(): void
    {
        if ($this->handle !== null) {
            $this->ffi->engine_close($this->handle);
            $this->handle = null;
        }
    }

    /** @return list<array<string, ?string>> */
    private function bufferedQuery(string $sql): array
    {
        $h = $this->h();
        $out = $this->ffi->new('EngineResult*');
        $st = $this->ffi->engine_query($h, $sql, FFI::addr($out));
        if ($st !== EngineError::OK) {
            throw $this->errorFrom($st);
        }
        try {
            $rows = $this->ffi->engine_result_rows($out);
            $cols = $this->ffi->engine_result_cols($out);
            $names = [];
            for ($c = 0; $c < $cols; $c++) {
                $names[$c] = self::readCString($this->ffi->engine_result_colname($out, $c)) ?? ('col' . ($c + 1));
            }
            $data = [];
            for ($r = 0; $r < $rows; $r++) {
                $row = [];
                for ($c = 0; $c < $cols; $c++) {
                    $row[$names[$c]] = self::readCString($this->ffi->engine_result_value($out, $r, $c));
                }
                $data[] = $row;
            }
            return $data;
        } finally {
            $this->ffi->engine_result_free($out);
        }
    }

    private function errorFrom(int $status): EngineError
    {
        $msg = '';
        if ($this->handle !== null) {
            $msg = self::readCString($this->ffi->engine_last_error($this->handle)) ?? '';
        }
        return new EngineError($status, $msg);
    }

    /**
     * Normalize an engine `const char*` return. PHP's FFI already decodes a
     * `const char*` result into a copied PHP string (and a NULL pointer — a SQL
     * NULL cell — into PHP `null`), so this is a typed passthrough that keeps the
     * borrowed-pointer contract explicit at the call sites.
     */
    public static function readCString(?string $v): ?string
    {
        return $v;
    }
}
