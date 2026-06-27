<?php

declare(strict_types=1);

namespace Twill;

use FFI;

/**
 * Loads `libengine` once and exposes the raw C ABI (engine.h) via PHP's built-in
 * FFI extension. This is the PHP twin of clients/bun/src/ffi.ts and
 * clients/node/src/ffi.ts: a thin, faithful binding of the stable symbols. The
 * ergonomic surface lives in {@see Database} / {@see Statement}.
 *
 * The storage backend is chosen entirely by the URL scheme passed to
 * engine_open ("file://", "s3://", …); nothing here branches on it.
 */
final class Engine
{
    /** The ABI version this binding was written against (mirrors engine.h). */
    public const ABI_VERSION = 3;

    /**
     * Declarations for FFI::cdef — a hand-maintained subset of engine.h. Opaque
     * handles are incomplete struct types; callers only ever hold pointers.
     */
    private const CDEF = <<<'C'
        typedef struct EngineHandle EngineHandle;
        typedef struct EngineResult EngineResult;
        typedef struct EngineStmt   EngineStmt;

        EngineHandle* engine_open(const char* url);
        void          engine_close(EngineHandle* h);

        int  engine_exec(EngineHandle* h, const char* sql);
        int  engine_query(EngineHandle* h, const char* sql, EngineResult** out);

        int  engine_prepare(EngineHandle* h, const char* sql, EngineStmt** out);
        int  engine_bind(EngineStmt* s, int idx, const char* value);
        int  engine_step(EngineStmt* s, int* done);
        int  engine_finalize(EngineStmt* s);
        int  engine_reset(EngineStmt* s);

        int  engine_begin(EngineHandle* h);
        int  engine_commit(EngineHandle* h);
        int  engine_rollback(EngineHandle* h);

        EngineHandle* engine_branch(EngineHandle* h, const char* name);

        int          engine_result_rows(const EngineResult* r);
        int          engine_result_cols(const EngineResult* r);
        const char*  engine_result_colname(const EngineResult* r, int col);
        const char*  engine_result_value(const EngineResult* r, int row, int col);

        int          engine_column_count(const EngineStmt* s);
        const char*  engine_column_name(const EngineStmt* s, int col);
        const char*  engine_column_value(const EngineStmt* s, int col);

        const char*  engine_last_error(EngineHandle* h);
        long long    engine_changes(EngineHandle* h);
        long long    engine_last_lsn(EngineHandle* h);
        int          engine_abi_version(void);

        void         engine_result_free(EngineResult* r);
        C;

    private static ?FFI $ffi = null;

    /** The shared FFI instance, loaded and ABI-checked on first use. */
    public static function ffi(): FFI
    {
        if (self::$ffi !== null) {
            return self::$ffi;
        }

        if (!\extension_loaded('FFI')) {
            throw new EngineError(
                EngineError::ERR_INTERNAL,
                'the PHP FFI extension is required (enable ffi in php.ini, or run with -d ffi.enable=1)'
            );
        }

        $path = self::resolveLibraryPath();
        try {
            $ffi = FFI::cdef(self::CDEF, $path);
        } catch (\FFI\Exception $e) {
            throw new EngineError(
                EngineError::ERR_STORAGE,
                "failed to load the engine library at \"{$path}\": {$e->getMessage()}. "
                . 'Build it with `cargo build -p twill-engine --release`, install the matching '
                . 'twilldb/engine-* binary, or set TWILLDB_ENGINE_PATH.'
            );
        }

        $got = $ffi->engine_abi_version();
        if ($got !== self::ABI_VERSION) {
            throw new EngineError(
                EngineError::ERR_INTERNAL,
                "engine ABI v{$got}, binding expects v" . self::ABI_VERSION
                . ' — upgrade twilldb/twilldb or the engine binary.'
            );
        }

        return self::$ffi = $ffi;
    }

    /** Platform dynamic-library extension for libengine. */
    private static function libSuffix(): string
    {
        return match (PHP_OS_FAMILY) {
            'Darwin'  => 'dylib',
            'Windows' => 'dll',
            default   => 'so',
        };
    }

    /** Locate libengine.{so,dylib,dll}. Override with TWILLDB_ENGINE_PATH. */
    private static function resolveLibraryPath(): string
    {
        $override = \getenv('TWILLDB_ENGINE_PATH');
        if ($override !== false && $override !== '') {
            return $override;
        }

        $file = 'libengine.' . self::libSuffix();

        // Source checkout: pick up a locally built libengine from the cargo target.
        $here = __DIR__; // clients/php/src
        $candidates = [
            $here . '/../../../target/release/' . $file,
            $here . '/../../../target/debug/' . $file,
            \getcwd() . '/target/release/' . $file,
            \getcwd() . '/target/debug/' . $file,
            \getcwd() . '/' . $file,
            // Installed via Composer alongside a vendored binary package.
            $here . '/../vendor/twilldb/engine/' . $file,
        ];
        foreach ($candidates as $c) {
            if (\is_file($c)) {
                return $c;
            }
        }

        // Last resort: let the dynamic loader search its default paths.
        return $file;
    }
}
