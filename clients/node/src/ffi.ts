// Raw koffi bindings over the engine's C ABI (engine.h). The mirror of
// `@twilldb/bun`'s ffi.ts, but for Node: where Bun has `bun:ffi`, Node binds the
// same `libengine` through koffi — a prebuilt, dependency-light FFI library that
// works on Node and every Node-based framework runtime (Next.js, Astro, Nuxt,
// Remix, SvelteKit, …). Nothing here branches on the storage backend; that is
// chosen entirely by the URL scheme passed to engine_open.

import { createRequire } from "node:module";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";

// koffi is CommonJS; load it through createRequire so this file stays ESM.
const require = createRequire(import.meta.url);
// eslint-disable-next-line @typescript-eslint/no-var-requires
const koffi = require("koffi");

/** Opaque pointer type returned to JS as an external handle. */
export type Pointer = unknown;

/** The platform's dynamic-library extension for libengine. */
function libSuffix(): string {
  switch (process.platform) {
    case "darwin":
      return "dylib";
    case "win32":
      return "dll";
    default:
      return "so";
  }
}

/**
 * The published per-platform binary package for the current host, e.g.
 * `@twilldb/engine-linux-x64`. These are the optionalDependencies of
 * `@twilldb/node` (the *same* binaries `@twilldb/bun` uses); npm/pnpm/yarn
 * installs only the one matching `os`+`cpu`, so an end user gets a working
 * engine with no `cargo build` and no postinstall.
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

  const file = `libengine.${libSuffix()}`;

  // 1) Installed from npm: resolve the matching per-platform binary package.
  const pkg = platformPackage();
  if (pkg) {
    try {
      const binary = join(dirname(require.resolve(`${pkg}/package.json`)), file);
      if (existsSync(binary)) return binary;
    } catch {
      // package not installed (e.g. running from a source checkout) — fall through.
    }
  }

  // 2) Source checkout: pick up a locally built libengine from the cargo target.
  const here = dirname(new URL(import.meta.url).pathname); // clients/node/src
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

let nativeLib: ReturnType<typeof koffi.load>;
try {
  nativeLib = koffi.load(LIB_PATH);
} catch (e) {
  throw new Error(
    `@twilldb/node: failed to load the engine library at "${LIB_PATH}". ` +
      `If you installed from npm, the per-platform binary package (${
        platformPackage() ?? "unsupported platform"
      }) is missing or your platform is unsupported. From a source checkout, build it ` +
      `with \`cargo build -p twill-engine --release\` or set TWILLDB_ENGINE_PATH to a ` +
      `built libengine.${libSuffix()}. Original error: ${(e as Error).message}`,
  );
}

// Declare every exported symbol with a C prototype string. `_Out_` marks the
// pointer-to-pointer / pointer-to-int out-parameters koffi must write back.
export const lib = {
  // lifecycle
  engine_open: nativeLib.func("void *engine_open(const char *url)"),
  engine_close: nativeLib.func("void engine_close(void *h)"),
  // one-shot execution
  engine_exec: nativeLib.func("int engine_exec(void *h, const char *sql)"),
  engine_query: nativeLib.func("int engine_query(void *h, const char *sql, _Out_ void **out)"),
  // prepared statements
  engine_prepare: nativeLib.func("int engine_prepare(void *h, const char *sql, _Out_ void **out)"),
  engine_bind: nativeLib.func("int engine_bind(void *s, int idx, const char *value)"),
  engine_step: nativeLib.func("int engine_step(void *s, _Out_ int *done)"),
  engine_finalize: nativeLib.func("int engine_finalize(void *s)"),
  engine_reset: nativeLib.func("int engine_reset(void *s)"),
  // transactions
  engine_begin: nativeLib.func("int engine_begin(void *h)"),
  engine_commit: nativeLib.func("int engine_commit(void *h)"),
  engine_rollback: nativeLib.func("int engine_rollback(void *h)"),
  // branching (Phase 4)
  engine_branch: nativeLib.func("void *engine_branch(void *h, const char *name)"),
  // result / row access (const char* returns decode to JS strings; NULL -> null)
  engine_result_rows: nativeLib.func("int engine_result_rows(void *r)"),
  engine_result_cols: nativeLib.func("int engine_result_cols(void *r)"),
  engine_result_colname: nativeLib.func("const char *engine_result_colname(void *r, int col)"),
  engine_result_value: nativeLib.func("const char *engine_result_value(void *r, int row, int col)"),
  // statement cursor
  engine_column_count: nativeLib.func("int engine_column_count(void *s)"),
  engine_column_name: nativeLib.func("const char *engine_column_name(void *s, int col)"),
  engine_column_value: nativeLib.func("const char *engine_column_value(void *s, int col)"),
  // errors / metadata
  engine_last_error: nativeLib.func("const char *engine_last_error(void *h)"),
  engine_changes: nativeLib.func("int64_t engine_changes(void *h)"),
  engine_last_lsn: nativeLib.func("int64_t engine_last_lsn(void *h)"),
  engine_abi_version: nativeLib.func("int engine_abi_version()"),
  // freeing
  engine_result_free: nativeLib.func("void engine_result_free(void *r)"),
};

export const libPath = LIB_PATH;

/** The ABI version the wrapper was written against (mirrors engine.h). */
export const EXPECTED_ABI_VERSION = 3;

// Verify the loaded library matches the wrapper's expected ABI, failing fast
// rather than calling a stale symbol (undefined behaviour).
{
  const got = lib.engine_abi_version();
  if (got !== EXPECTED_ABI_VERSION) {
    throw new Error(
      `@twilldb/node: engine ABI v${got}, wrapper expects v${EXPECTED_ABI_VERSION}. ` +
        `Upgrade @twilldb/node or the engine binary.`,
    );
  }
}

/** True for a NULL / absent external pointer returned by koffi. */
export function isNull(p: Pointer | null | undefined): boolean {
  return p === null || p === undefined;
}

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
