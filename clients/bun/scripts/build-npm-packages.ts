#!/usr/bin/env bun
// Assemble the per-platform binary packages that back @twilldb/bun.
//
// Each platform gets its own tiny npm package (e.g. @twilldb/engine-linux-x64)
// carrying just the prebuilt libengine for that os/cpu plus engine.h. They are
// the optionalDependencies of @twilldb/bun, so npm/bun installs only the one
// matching the host — no postinstall, no network at install time, offline-safe.
// (Same shape esbuild/swc use.) `ffi.ts::resolveLibraryPath` resolves the
// matching package at load time.
//
// Usage:
//   bun run scripts/build-npm-packages.ts <artifacts-dir> [--out dist/npm]
//
// <artifacts-dir> holds the release binaries named as the matrix uploads them:
//   libengine-linux-x64.so      libengine-linux-arm64.so
//   libengine-darwin-x64.dylib  libengine-darwin-arm64.dylib
//   engine-win32-x64.dll        engine.h
// Missing binaries are skipped (so a single-platform local run still works).

import { existsSync, mkdirSync, copyFileSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const REPO = "https://github.com/bihaviour/twill-db";

/** Single source of truth for the version: the Cargo workspace package version. */
export function workspaceVersion(): string {
  const cargo = readFileSync(resolve(import.meta.dir, "..", "..", "..", "Cargo.toml"), "utf8");
  const m = cargo.match(/\[workspace\.package\][\s\S]*?\bversion\s*=\s*"([^"]+)"/);
  if (!m) throw new Error("could not parse [workspace.package] version from Cargo.toml");
  return m[1]!;
}

const VERSION = workspaceVersion();

interface Target {
  pkg: string; // npm package name suffix → @twilldb/engine-<pkg>
  os: string; // npm "os" field (process.platform value)
  cpu: string; // npm "cpu" field (process.arch value)
  suffix: string; // installed binary extension (matches bun:ffi `suffix`)
  artifact: string; // filename in <artifacts-dir>
}

const TARGETS: Target[] = [
  { pkg: "linux-x64", os: "linux", cpu: "x64", suffix: "so", artifact: "libengine-linux-x64.so" },
  { pkg: "linux-arm64", os: "linux", cpu: "arm64", suffix: "so", artifact: "libengine-linux-arm64.so" },
  { pkg: "darwin-x64", os: "darwin", cpu: "x64", suffix: "dylib", artifact: "libengine-darwin-x64.dylib" },
  { pkg: "darwin-arm64", os: "darwin", cpu: "arm64", suffix: "dylib", artifact: "libengine-darwin-arm64.dylib" },
  { pkg: "win32-x64", os: "win32", cpu: "x64", suffix: "dll", artifact: "engine-win32-x64.dll" },
];

function main() {
  const args = process.argv.slice(2);
  const artifactsDir = resolve(args.find((a) => !a.startsWith("--")) ?? "");
  if (!artifactsDir || !existsSync(artifactsDir)) {
    console.error("usage: bun run scripts/build-npm-packages.ts <artifacts-dir> [--out dist/npm]");
    process.exit(1);
  }
  const outIdx = args.indexOf("--out");
  const outDir = resolve(outIdx >= 0 ? args[outIdx + 1]! : "dist/npm");

  const header = existsSync(join(artifactsDir, "engine.h")) ? join(artifactsDir, "engine.h") : undefined;

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  let built = 0;
  for (const t of TARGETS) {
  const src = join(artifactsDir, t.artifact);
  if (!existsSync(src)) {
    console.warn(`skip @twilldb/engine-${t.pkg} — missing ${t.artifact}`);
    continue;
  }
  const dir = join(outDir, t.pkg);
  mkdirSync(dir, { recursive: true });

  // The wrapper always loads `libengine.<suffix>`, regardless of artifact name.
  copyFileSync(src, join(dir, `libengine.${t.suffix}`));
  const files = [`libengine.${t.suffix}`, "README.md"];
  if (header) {
    copyFileSync(header, join(dir, "engine.h"));
    files.push("engine.h");
  }

  const pkgJson = {
    name: `@twilldb/engine-${t.pkg}`,
    version: VERSION,
    description: `Prebuilt twill-db engine native library for ${t.os}/${t.cpu}.`,
    license: "BUSL-1.1",
    repository: { type: "git", url: REPO },
    os: [t.os],
    cpu: [t.cpu],
    files,
  };
  writeFileSync(join(dir, "package.json"), JSON.stringify(pkgJson, null, 2) + "\n");
  writeFileSync(
    join(dir, "README.md"),
    `# @twilldb/engine-${t.pkg}\n\n` +
      `Prebuilt \`libengine\` native binary for **${t.os}/${t.cpu}**, consumed by ` +
      `[\`@twilldb/bun\`](${REPO}). You don't install this directly — it ships as an ` +
      `optional dependency and is selected automatically for your platform.\n`,
  );
  console.log(`built @twilldb/engine-${t.pkg} (${VERSION})`);
  built++;
}

  if (built === 0) {
    console.error(`no binaries found in ${artifactsDir} — nothing to build`);
    process.exit(1);
  }
  console.log(`\n${built} package(s) → ${outDir}`);
}

if (import.meta.main) main();
