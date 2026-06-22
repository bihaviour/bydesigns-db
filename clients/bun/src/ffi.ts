// Raw bun:ffi bindings over the engine's C ABI (engine.h). This is a faithful,
// thin shim: it loads libengine, declares every exported symbol, and provides
// small marshaling helpers. The ergonomic API lives in ./index.ts.
//
// The storage backend is chosen entirely by the URL scheme passed to
// engine_open ("file://..." in Phase 1); nothing here branches on it.

import { dlopen, FFIType, suffix, CString, ptr, type Pointer } from "bun:ffi";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

/**
 * The published per-platform binary package for the current host, e.g.
 * `@twilldb/engine-linux-x64`. These are the optionalDependencies of
 * `@twilldb/bun`; npm/bun installs only the one matching `os`+`cpu`, so an
 * end user gets a working engine with no `cargo build` and no postinstall.
 */
function platformPackage(): string | undefined {
  const arches: Record<string, string> = { x64: "x64", arm64: "arm64" };
  const oses: Record<string, string> = { linux: "linux", darwin: "darwin", win32: "win32" };
  const arch = arches[process.arch];
  const os = oses[process.platform];
  if (!arch || !os) return undefined;
  return `@twilldb/engine-${os}-${arch}`;
}

/** Locate libengine.{so,dylib,dll}. Override with TWILLDB_ENGINE_PATH. */
function resolveLibraryPath(): string {
  const override = process.env.TWILLDB_ENGINE_PATH;
  if (override) return override;

  const file = `libengine.${suffix}`;

  // 1) Installed from npm: resolve the matching per-platform binary package.
  const pkg = platformPackage();
  if (pkg) {
    try {
      const req = createRequire(import.meta.url);
      const binary = join(dirname(req.resolve(`${pkg}/package.json`)), file);
      if (existsSync(binary)) return binary;
    } catch {
      // package not installed (e.g. running from a source checkout) — fall through.
    }
  }

  // 2) Source checkout: pick up a locally built libengine from the cargo target dir.
  const here = import.meta.dir; // clients/bun/src
  const candidates = [
    join(here, "..", "..", "..", "target", "release", file),
    join(here, "..", "..", "..", "target", "debug", file),
    join(process.cwd(), "target", "release", file),
    join(process.cwd(), "target", "debug", file),
    join(process.cwd(), file),
  ];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  // Last resort: let the dynamic loader search its default paths.
  return file;
}

const LIB_PATH = resolveLibraryPath();

const ABI = {
  // lifecycle
  engine_open: { args: [FFIType.cstring], returns: FFIType.ptr },
  engine_close: { args: [FFIType.ptr], returns: FFIType.void },
  // one-shot execution
  engine_exec: { args: [FFIType.ptr, FFIType.cstring], returns: FFIType.i32 },
  engine_query: { args: [FFIType.ptr, FFIType.cstring, FFIType.ptr], returns: FFIType.i32 },
  // prepared statements
  engine_prepare: { args: [FFIType.ptr, FFIType.cstring, FFIType.ptr], returns: FFIType.i32 },
  engine_bind: { args: [FFIType.ptr, FFIType.i32, FFIType.cstring], returns: FFIType.i32 },
  engine_step: { args: [FFIType.ptr, FFIType.ptr], returns: FFIType.i32 },
  engine_finalize: { args: [FFIType.ptr], returns: FFIType.i32 },
  engine_reset: { args: [FFIType.ptr], returns: FFIType.i32 },
  // transactions
  engine_begin: { args: [FFIType.ptr], returns: FFIType.i32 },
  engine_commit: { args: [FFIType.ptr], returns: FFIType.i32 },
  engine_rollback: { args: [FFIType.ptr], returns: FFIType.i32 },
  // branching (Phase 4)
  engine_branch: { args: [FFIType.ptr, FFIType.cstring], returns: FFIType.ptr },
  // result / row access (return ptr so we can detect a NULL = SQL NULL)
  engine_result_rows: { args: [FFIType.ptr], returns: FFIType.i32 },
  engine_result_cols: { args: [FFIType.ptr], returns: FFIType.i32 },
  engine_result_colname: { args: [FFIType.ptr, FFIType.i32], returns: FFIType.ptr },
  engine_result_value: { args: [FFIType.ptr, FFIType.i32, FFIType.i32], returns: FFIType.ptr },
  // statement cursor
  engine_column_count: { args: [FFIType.ptr], returns: FFIType.i32 },
  engine_column_name: { args: [FFIType.ptr, FFIType.i32], returns: FFIType.ptr },
  engine_column_value: { args: [FFIType.ptr, FFIType.i32], returns: FFIType.ptr },
  // errors / metadata
  engine_last_error: { args: [FFIType.ptr], returns: FFIType.ptr },
  engine_changes: { args: [FFIType.ptr], returns: FFIType.i64 },
  engine_last_lsn: { args: [FFIType.ptr], returns: FFIType.i64 },
  engine_abi_version: { args: [], returns: FFIType.i32 },
  // freeing
  engine_result_free: { args: [FFIType.ptr], returns: FFIType.void },
} as const;

let opened;
try {
  opened = dlopen(LIB_PATH, ABI);
} catch (e) {
  throw new Error(
    `@twilldb/bun: failed to load the engine library at "${LIB_PATH}". ` +
      `If you installed from npm, the per-platform binary package (${platformPackage() ?? "unsupported platform"}) ` +
      `is missing or your platform is unsupported. From a source checkout, build it with ` +
      `\`cargo build -p twill-engine --release\` or set TWILLDB_ENGINE_PATH to a built ` +
      `libengine.${suffix}. Original error: ${(e as Error).message}`,
  );
}

export const lib = opened.symbols;
export const libPath = LIB_PATH;

/** The ABI version the wrapper was written against. */
export const EXPECTED_ABI_VERSION = 3;

// Verify the loaded library matches the wrapper's expected ABI, failing fast
// rather than calling a stale symbol (undefined behaviour).
{
  const got = lib.engine_abi_version();
  if (got !== EXPECTED_ABI_VERSION) {
    throw new Error(
      `@twilldb/bun: engine ABI v${got}, wrapper expects v${EXPECTED_ABI_VERSION}. ` +
        `Upgrade @twilldb/bun or the engine binary.`,
    );
  }
}

/** JS string -> NUL-terminated UTF-8 buffer for a const char* argument. */
export function cstr(s: string): Buffer {
  return Buffer.from(s + "\0", "utf8");
}

/** Read a borrowed const char*; returns null for a NULL pointer (SQL NULL). */
export function readCString(p: Pointer | null): string | null {
  if (p === null || (p as unknown as number) === 0) return null;
  return new CString(p).toString();
}

export { ptr };
export type { Pointer };

// EngineStatus codes (mirror engine.h).
export const STATUS = {
  OK: 0,
  ERR_SQL: 1,
  ERR_CONSTRAINT: 2,
  ERR_CONFLICT: 3,
  ERR_STORAGE: 4,
  ERR_TXN: 5,
  ERR_MISUSE: 6,
  ERR_INTERNAL: 7,
} as const;
