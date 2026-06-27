<?php

declare(strict_types=1);

namespace Twill;

/**
 * An engine failure, carrying the numeric EngineStatus (engine.h) and whether a
 * retry may help. Mirrors the `EngineError` in the Bun/Node clients.
 */
final class EngineError extends \RuntimeException
{
    public const OK             = 0;
    public const ERR_SQL        = 1;
    public const ERR_CONSTRAINT = 2;
    public const ERR_CONFLICT   = 3;
    public const ERR_STORAGE    = 4;
    public const ERR_TXN        = 5;
    public const ERR_MISUSE     = 6;
    public const ERR_INTERNAL   = 7;

    public readonly int $status;
    public readonly bool $retryable;

    public function __construct(int $status, string $message)
    {
        parent::__construct($message !== '' ? $message : "engine status {$status}", $status);
        $this->status = $status;
        // Conflicts and transient storage faults are safe to retry.
        $this->retryable = $status === self::ERR_CONFLICT || $status === self::ERR_STORAGE;
    }
}
