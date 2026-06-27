<?php

declare(strict_types=1);

namespace Twill;

use FFI;
use FFI\CData;

/**
 * A prepared statement. Parameters bind positionally (`?`) and are encoded with
 * the ABI's one-character type tag (engine.h "parameter encoding").
 */
final class Statement
{
    private FFI $ffi;
    private CData $handle; // owning connection (for changes / errors)
    private ?CData $stmt;

    public function __construct(FFI $ffi, CData $handle, CData $stmt)
    {
        $this->ffi = $ffi;
        $this->handle = $handle;
        $this->stmt = $stmt;
    }

    private function s(): CData
    {
        if ($this->stmt === null) {
            throw new EngineError(EngineError::ERR_MISUSE, 'statement is finalized');
        }
        return $this->stmt;
    }

    /**
     * Execute and buffer all rows.
     *
     * @return list<array<string, ?string>>
     */
    public function all(mixed ...$params): array
    {
        $s = $this->s();
        if ($params !== []) {
            $this->bind($params);
        }
        $done = $this->ffi->new('int');
        $rows = [];
        $names = null;
        while (true) {
            $st = $this->ffi->engine_step($s, FFI::addr($done));
            if ($st !== EngineError::OK) {
                throw $this->errorFrom($st);
            }
            if ($done->cdata === 1) {
                break;
            }
            if ($names === null) {
                $names = [];
                $cols = $this->ffi->engine_column_count($s);
                for ($c = 0; $c < $cols; $c++) {
                    $names[$c] = Database::readCString($this->ffi->engine_column_name($s, $c)) ?? ('col' . ($c + 1));
                }
            }
            $row = [];
            foreach ($names as $c => $name) {
                $row[$name] = Database::readCString($this->ffi->engine_column_value($s, $c));
            }
            $rows[] = $row;
        }
        $this->ffi->engine_reset($s);
        return $rows;
    }

    /**
     * Execute and return the first row, or null.
     *
     * @return array<string, ?string>|null
     */
    public function get(mixed ...$params): ?array
    {
        return $this->all(...$params)[0] ?? null;
    }

    /** Execute a write and return rows affected. */
    public function run(mixed ...$params): int
    {
        $s = $this->s();
        if ($params !== []) {
            $this->bind($params);
        }
        $done = $this->ffi->new('int');
        $st = $this->ffi->engine_step($s, FFI::addr($done));
        if ($st !== EngineError::OK) {
            throw $this->errorFrom($st);
        }
        $changes = (int) $this->ffi->engine_changes($this->handle);
        $this->ffi->engine_reset($s);
        return $changes;
    }

    public function finalize(): void
    {
        if ($this->stmt !== null) {
            $this->ffi->engine_finalize($this->stmt);
            $this->stmt = null;
        }
    }

    /** @param list<mixed> $params */
    private function bind(array $params): void
    {
        $s = $this->s();
        foreach ($params as $i => $p) {
            $st = $this->ffi->engine_bind($s, $i + 1, self::encodeParam($p));
            if ($st !== EngineError::OK) {
                throw $this->errorFrom($st);
            }
        }
    }

    /** Encode a PHP value into the ABI's tagged-literal bind form. */
    private static function encodeParam(mixed $p): string
    {
        if ($p === null) {
            return 'n';
        }
        if (\is_bool($p)) {
            return 'i' . ($p ? '1' : '0');
        }
        if (\is_int($p)) {
            return 'i' . $p;
        }
        if (\is_float($p)) {
            return 'f' . $p;
        }
        if (\is_string($p)) {
            return 's' . $p;
        }
        if (\is_array($p)) {
            // A list of numbers is a vector literal (Phase 5): v1,2,3
            return 'v' . \implode(',', \array_map(static fn ($x) => (string) $x, $p));
        }
        throw new EngineError(EngineError::ERR_MISUSE, 'unsupported parameter type: ' . \get_debug_type($p));
    }

    private function errorFrom(int $status): EngineError
    {
        $msg = Database::readCString($this->ffi->engine_last_error($this->handle)) ?? '';
        return new EngineError($status, $msg);
    }
}
