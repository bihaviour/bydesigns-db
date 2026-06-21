// Phase 5 example (issue #26) — better-auth composed in-process over the engine.
//
// Run: cargo build -p twill-engine --release
//      cd clients/bun && bun run examples/auth-app.ts
//
// better-auth is an ordinary library here, not a service. We hand it a Twill
// adapter (`../src/better-auth`) so every user/session/account it writes is a
// plain row in the embedded engine. There is no auth server to run, deploy, or
// scale: a session check is a local function call. And because auth state is
// just rows, it inherits what rows already get — it syncs to object storage,
// it branches copy-on-write, and it re-warms after scale-to-zero.

import { betterAuth } from "better-auth";
import { open } from "../src/index";
import { twillAdapter } from "../src/better-auth";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rmSync } from "node:fs";

const dbFile = join(tmpdir(), `auth-app-${process.pid}.db`);
const url = `file://${dbFile}`;

async function main(): Promise<void> {
  using db = open(url);

  const auth = betterAuth({
    database: twillAdapter(db),
    emailAndPassword: { enabled: true },
    secret: "example-secret-please-use-a-real-one-in-production",
    baseURL: "http://localhost",
  });

  // Register a user — better-auth hashes the password and writes the user,
  // account, and session rows through the adapter into the embedded engine.
  await auth.api.signUpEmail({
    body: { email: "ada@example.com", password: "correct-horse-battery", name: "Ada" },
  });
  console.log("signed up: ada@example.com");

  // Sign in — verifies the hash and mints a session token.
  const signIn = await auth.api.signInEmail({
    body: { email: "ada@example.com", password: "correct-horse-battery" },
  });
  console.log("signed in, session token:", signIn.token);

  // The auth state is just rows — readable with ordinary SQL, no auth API needed.
  const users = db.query<{ email: string; name: string }>("SELECT email, name FROM user");
  console.log("users in the engine:", users);

  // Resolve the token the way an in-process check would: two local queries.
  const sess = db.query<{ userId: string }>("SELECT userId FROM session WHERE token = ?", [
    signIn.token,
  ]);
  const who = db.query<{ name: string }>("SELECT name FROM user WHERE id = ?", [sess[0].userId]);
  console.log("token resolves to:", who[0]?.name);

  // Auth state branches with the database: a staging branch gets its own users,
  // and a sign-up there never leaks back to the base.
  using staging = db.branch("staging");
  const stagingAuth = betterAuth({
    database: twillAdapter(staging),
    emailAndPassword: { enabled: true },
    secret: "example-secret-please-use-a-real-one-in-production",
    baseURL: "http://localhost",
  });
  await stagingAuth.api.signUpEmail({
    body: { email: "bel@example.com", password: "staging-only-secret", name: "Bel" },
  });
  const base = db.query<{ email: string }>("SELECT email FROM user").map((u) => u.email);
  const branch = staging.query<{ email: string }>("SELECT email FROM user").map((u) => u.email);
  console.log("base branch users:   ", base);
  console.log("staging branch users:", branch);
  console.log("base never saw the staging user:", !base.includes("bel@example.com"));
}

try {
  await main();
} finally {
  rmSync(dbFile, { force: true });
}
