#!/usr/bin/env bun
// Stamp @twilldb/bun's package.json to the Cargo workspace version: its own
// `version` plus every `@twilldb/engine-*` entry in optionalDependencies. The
// wrapper and its per-platform binary packages are an ABI pair, so they must
// always publish at the exact same version. Run by CI before `npm publish`, and
// usable locally / by the /release skill after bumping the workspace version.

import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { workspaceVersion } from "./build-npm-packages.ts";

const version = workspaceVersion();
const pkgPath = join(import.meta.dir, "..", "package.json");
const pkg = JSON.parse(readFileSync(pkgPath, "utf8"));

let changed = pkg.version !== version;
pkg.version = version;
for (const dep of Object.keys(pkg.optionalDependencies ?? {})) {
  if (dep.startsWith("@twilldb/engine-") && pkg.optionalDependencies[dep] !== version) {
    pkg.optionalDependencies[dep] = version;
    changed = true;
  }
}

writeFileSync(pkgPath, JSON.stringify(pkg, null, 2) + "\n");
console.log(`@twilldb/bun pinned to ${version}${changed ? "" : " (already in sync)"}`);
