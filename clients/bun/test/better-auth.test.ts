// Composition test (issue #26): better-auth running in-process against the
// embedded engine via the Twill adapter. Exercises the real library — sign-up,
// sign-in, session lookup — through better-auth's own server API (no HTTP), and
// proves the two acceptance properties that make this composition and not a
// bolt-on: auth state is ordinary engine rows, and it branches with the database.
//
// Run: cargo build -p twill-engine --release && (cd clients/bun && bun test better-auth)

import { test, expect, beforeEach, afterEach } from "bun:test";
import { betterAuth } from "better-auth";
import { open, type Database } from "../src/index";
import { twillAdapter } from "../src/better-auth";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { rmSync } from "node:fs";

let dbFile: string;
let url: string;

function makeAuth(db: Database) {
  return betterAuth({
    database: twillAdapter(db),
    emailAndPassword: { enabled: true },
    secret: "test-secret-not-for-production-only-tests",
    baseURL: "http://localhost",
  });
}

beforeEach(() => {
  dbFile = join(tmpdir(), `twilldb-auth-${process.pid}-${Math.random().toString(36).slice(2)}.db`);
  url = `file://${dbFile}`;
});

afterEach(() => {
  try {
    rmSync(dbFile, { force: true });
  } catch {}
});

test("sign-up persists a user and session as engine rows", async () => {
  using db = open(url);
  const auth = makeAuth(db);

  const res = await auth.api.signUpEmail({
    body: { email: "ada@example.com", password: "correct-horse-battery", name: "Ada" },
  });
  expect(res.user.email).toBe("ada@example.com");

  // The user is a plain row in the engine — readable with ordinary SQL.
  const users = db.query<{ email: string; name: string }>("SELECT email, name FROM user");
  expect(users.length).toBe(1);
  expect(users[0].email).toBe("ada@example.com");
  expect(users[0].name).toBe("Ada");

  // And a session row exists, tied to that user.
  const sessions = db.query("SELECT id, userId FROM session");
  expect(sessions.length).toBe(1);
});

test("sign-in succeeds for a registered user and rejects a bad password", async () => {
  using db = open(url);
  const auth = makeAuth(db);

  await auth.api.signUpEmail({
    body: { email: "grace@example.com", password: "hopper-1906", name: "Grace" },
  });

  const ok = await auth.api.signInEmail({
    body: { email: "grace@example.com", password: "hopper-1906" },
  });
  expect(ok.user.email).toBe("grace@example.com");
  expect(ok.token).toBeTruthy();

  await expect(
    auth.api.signInEmail({ body: { email: "grace@example.com", password: "wrong" } }),
  ).rejects.toBeDefined();
});

test("a session token resolves back to its user through engine rows", async () => {
  using db = open(url);
  const auth = makeAuth(db);

  await auth.api.signUpEmail({
    body: { email: "linus@example.com", password: "torvalds-1991", name: "Linus" },
  });
  const signIn = await auth.api.signInEmail({
    body: { email: "linus@example.com", password: "torvalds-1991" },
  });

  // The bearer token better-auth hands back IS the stored session token, so a
  // session lookup is two ordinary engine queries — exactly what an in-process
  // auth check costs here: a local function call, no network, no service.
  const sess = db.query<{ userId: string }>("SELECT userId FROM session WHERE token = ?", [
    signIn.token,
  ]);
  expect(sess.length).toBe(1);
  const user = db.query<{ email: string }>("SELECT email FROM user WHERE id = ?", [sess[0].userId]);
  expect(user[0]?.email).toBe("linus@example.com");
});

test("auth state branches with the database", async () => {
  using db = open(url);
  const auth = makeAuth(db);
  await auth.api.signUpEmail({
    body: { email: "base@example.com", password: "base-password-1", name: "Base" },
  });

  // Branch the database; the branch inherits the base user but writes in isolation.
  using branch = db.branch("staging");
  const branchAuth = makeAuth(branch);

  // The base user is visible on the branch (read-through below the fork LSN).
  const inherited = branch.query<{ email: string }>("SELECT email FROM user");
  expect(inherited.map((u) => u.email)).toContain("base@example.com");

  // A sign-up on the branch must not leak back to the base.
  await branchAuth.api.signUpEmail({
    body: { email: "branch-only@example.com", password: "branch-password-1", name: "Branch" },
  });

  const baseEmails = db.query<{ email: string }>("SELECT email FROM user").map((u) => u.email);
  expect(baseEmails).toContain("base@example.com");
  expect(baseEmails).not.toContain("branch-only@example.com");

  const branchEmails = branch.query<{ email: string }>("SELECT email FROM user").map((u) => u.email);
  expect(branchEmails).toContain("branch-only@example.com");
});
