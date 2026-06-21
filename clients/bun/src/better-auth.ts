// @twilldb/bun — a better-auth database adapter backed by the embedded engine.
//
// This is composition, not bundling (spec 12): better-auth is an ordinary
// in-process library; we only hand it a `Storage`-backed adapter so its users,
// sessions, accounts, and verifications are stored as plain rows in the engine.
// Because they are plain rows they inherit everything the engine already gives
// rows for free — they sync to object storage, they branch copy-on-write, and
// they re-warm after scale-to-zero. No external auth service, no engine change.
//
//   import { betterAuth } from "better-auth";
//   import { open } from "@twilldb/bun";
//   import { twillAdapter } from "@twilldb/bun/better-auth";
//
//   using db = open("file://./app.db");
//   const auth = betterAuth({
//     database: twillAdapter(db),
//     emailAndPassword: { enabled: true },
//     secret: process.env.BETTER_AUTH_SECRET!,
//   });
//
// The engine's SQL surface is a focused subset (no JOIN / GROUP BY / subqueries,
// no native IN / OFFSET). The adapter maps better-auth's CRUD contract onto that
// subset: `IN` becomes an OR-chain, `OFFSET` is applied client-side, and LIMIT is
// emitted as the integer literal the parser requires. better-auth handles the
// rest — id generation, password hashing, field/date/boolean transforms — so the
// adapter only has to translate queries.

import { createAdapterFactory } from "better-auth/adapters";
import type { CreateCustomAdapter } from "better-auth/adapters";
import type { Database, Param } from "./index";

// better-auth's field descriptor (the parts this adapter reads). The full type
// lives in @better-auth/core; we keep a structural subset to avoid a type-only
// dependency on its internals.
// better-auth field types are a literal-string union (or an array of literals for
// enums); we only branch on the scalar cases, so anything non-scalar is TEXT.
type FieldAttribute = {
  type: string | string[];
  fieldName?: string;
  references?: unknown;
};
type ModelSchema = { modelName: string; fields: Record<string, FieldAttribute> };
type AuthSchema = Record<string, ModelSchema>;

type WhereOperator =
  | "eq"
  | "ne"
  | "lt"
  | "lte"
  | "gt"
  | "gte"
  | "in"
  | "not_in"
  | "contains"
  | "starts_with"
  | "ends_with";
type WhereClause = {
  field: string;
  value: unknown;
  operator?: WhereOperator;
  connector?: "AND" | "OR";
};

// Map a better-auth field type onto the engine's storage-class affinity. Dates
// and JSON/arrays are serialized to strings by better-auth (we disable native
// support below), so they land in TEXT; booleans and numbers map to INTEGER.
function sqlType(type: string | string[]): string {
  switch (type) {
    case "number":
    case "boolean":
      return "INTEGER";
    default:
      return "TEXT"; // string, date, json, enum (string[]), number[]
  }
}

// A value safe to bind through the C ABI: Dates become ISO strings (better-auth
// usually pre-serializes them, but a stray Date must not reach the binder).
function bindable(value: unknown): Param {
  if (value instanceof Date) return value.toISOString();
  return value as Param;
}

// Translate one better-auth WHERE clause (its field is already the physical
// column name) into an engine SQL fragment plus the params it binds. The engine
// has no native IN/NOT IN, so set-membership expands to an OR/AND chain; the
// substring operators ride LIKE with a %-wrapped pattern.
function clauseFragment(w: WhereClause): { frag: string; params: Param[] } {
  const f = w.field;
  const op = w.operator ?? "eq";
  const cmp: Partial<Record<WhereOperator, string>> = {
    lt: "<",
    lte: "<=",
    gt: ">",
    gte: ">=",
  };
  if (op === "eq" || op === "ne") {
    if (w.value === null) return { frag: `${f} IS ${op === "ne" ? "NOT " : ""}NULL`, params: [] };
    return { frag: `${f} ${op === "ne" ? "!=" : "="} ?`, params: [bindable(w.value)] };
  }
  if (cmp[op]) return { frag: `${f} ${cmp[op]} ?`, params: [bindable(w.value)] };
  if (op === "in" || op === "not_in") {
    const arr = (w.value as unknown[]) ?? [];
    if (arr.length === 0) return { frag: op === "in" ? "1 = 0" : "1 = 1", params: [] };
    const join = op === "in" ? " OR " : " AND ";
    const eq = op === "in" ? "=" : "!=";
    return { frag: `(${arr.map(() => `${f} ${eq} ?`).join(join)})`, params: arr.map(bindable) };
  }
  const like: Partial<Record<WhereOperator, (v: unknown) => string>> = {
    contains: (v) => `%${v}%`,
    starts_with: (v) => `${v}%`,
    ends_with: (v) => `%${v}`,
  };
  if (like[op]) return { frag: `${f} LIKE ?`, params: [like[op]!(w.value)] };
  return { frag: `${f} = ?`, params: [bindable(w.value)] };
}

/**
 * Build a better-auth database adapter that stores its tables in the given
 * embedded {@link Database}. Pass the result straight to `betterAuth({ database })`.
 *
 * Schema is created on first use (one `CREATE TABLE` per better-auth model);
 * because DDL runs in autocommit, this happens at adapter initialization, before
 * any auth request.
 */
export function twillAdapter(db: Database) {
  return createAdapterFactory({
    config: {
      adapterId: "twill",
      adapterName: "Twill DB",
      usePlural: false,
      // The engine stores six storage classes; let better-auth serialize the
      // rich types down to them and parse them back on the way out.
      supportsJSON: false,
      supportsDates: false,
      supportsBooleans: false,
      supportsArrays: false,
      supportsNumericIds: false,
    },
    // The factory hands the creator a rich helper object; we only read `schema`.
    // better-auth's CustomAdapter methods are generic in the row type; our impls
    // are concrete, so the creator is cast through the library's creator type.
    adapter: ((helpers: { schema: AuthSchema }) => {
      const schema = helpers.schema;
      // Physical column type, keyed by table then column — used to coerce the
      // engine's all-string row values back to the shapes better-auth expects.
      const colTypes: Record<string, Record<string, string | string[]>> = {};
      // Default field name -> physical column, per table, for ORDER BY (sortBy
      // arrives untransformed, unlike WHERE which the factory pre-transforms).
      const physical: Record<string, Record<string, string>> = {};

      for (const modelKey of Object.keys(schema)) {
        const { modelName, fields } = schema[modelKey];
        const types: Record<string, string | string[]> = { id: "string" };
        const phys: Record<string, string> = { id: "id" };
        const cols = ["id TEXT PRIMARY KEY"];
        for (const fieldKey of Object.keys(fields)) {
          const f = fields[fieldKey];
          const name = f.fieldName ?? fieldKey;
          cols.push(`${name} ${sqlType(f.type)}`);
          types[name] = f.type;
          phys[fieldKey] = name;
        }
        colTypes[modelName] = types;
        physical[modelName] = phys;
        // Idempotent: a second open over the same storage already has the table.
        try {
          db.exec(`CREATE TABLE ${modelName} (${cols.join(", ")})`);
        } catch {
          // table already exists — leave the existing definition in place.
        }
      }

      // Coerce one engine row (all columns are strings or null) back toward the
      // types better-auth's output transform expects: real booleans, numbers,
      // and Dates. Date strings are handed back as-is — better-auth parses them.
      const coerce = (table: string, row: Record<string, string | null>) => {
        const types = colTypes[table] ?? {};
        const out: Record<string, unknown> = {};
        for (const [k, v] of Object.entries(row)) {
          if (v === null) {
            out[k] = null;
            continue;
          }
          switch (types[k]) {
            case "boolean":
              out[k] = v === "1" || v === "true";
              break;
            case "number":
              out[k] = Number(v);
              break;
            default:
              out[k] = v;
          }
        }
        return out;
      };

      // Translate a better-auth WHERE (already field-name-transformed by the
      // factory) into an engine WHERE clause. Each clause carries its own
      // connector to the previous one (better-auth defaults to AND).
      const buildWhere = (where?: WhereClause[]): { sql: string; params: Param[] } => {
        if (!where || where.length === 0) return { sql: "", params: [] };
        const parts: string[] = [];
        const params: Param[] = [];
        where.forEach((w, i) => {
          const conn = i === 0 ? "" : w.connector === "OR" ? " OR " : " AND ";
          const { frag, params: p } = clauseFragment(w);
          parts.push(conn + frag);
          params.push(...p);
        });
        return { sql: ` WHERE ${parts.join("")}`, params };
      };

      const selectRows = (
        model: string,
        where?: WhereClause[],
        opts?: { sortBy?: { field: string; direction: "asc" | "desc" }; limit?: number; offset?: number },
      ) => {
        const { sql, params } = buildWhere(where);
        let q = `SELECT * FROM ${model}${sql}`;
        if (opts?.sortBy) {
          const col = physical[model]?.[opts.sortBy.field] ?? opts.sortBy.field;
          q += ` ORDER BY ${col} ${opts.sortBy.direction === "desc" ? "DESC" : "ASC"}`;
        }
        // The parser requires an integer literal for LIMIT, and has no OFFSET —
        // over-fetch by the offset, then drop it client-side.
        const offset = opts?.offset ?? 0;
        if (opts?.limit !== undefined) {
          q += ` LIMIT ${Math.max(0, Math.floor(opts.limit) + offset)}`;
        }
        const rows = db.query<Record<string, string | null>>(q, params).map((r) => coerce(model, r));
        return offset > 0 ? rows.slice(offset) : rows;
      };

      return {
        create: async ({ model, data }: { model: string; data: Record<string, unknown> }) => {
          const cols = Object.keys(data);
          const placeholders = cols.map(() => "?").join(", ");
          db.query(
            `INSERT INTO ${model} (${cols.join(", ")}) VALUES (${placeholders})`,
            cols.map((c) => bindable(data[c])),
          );
          return data;
        },

        findOne: async ({ model, where }: { model: string; where: WhereClause[] }) => {
          return selectRows(model, where, { limit: 1 })[0] ?? null;
        },

        findMany: async ({
          model,
          where,
          limit,
          sortBy,
          offset,
        }: {
          model: string;
          where?: WhereClause[];
          limit: number;
          sortBy?: { field: string; direction: "asc" | "desc" };
          offset?: number;
        }) => {
          return selectRows(model, where, { sortBy, limit, offset });
        },

        count: async ({ model, where }: { model: string; where?: WhereClause[] }) => {
          const { sql, params } = buildWhere(where);
          const rows = db.query<{ c: string }>(`SELECT COUNT(*) AS c FROM ${model}${sql}`, params);
          return Number(rows[0]?.c ?? 0);
        },

        update: async ({
          model,
          where,
          update,
        }: {
          model: string;
          where: WhereClause[];
          update: Record<string, unknown>;
        }) => {
          const cols = Object.keys(update);
          if (cols.length === 0) return selectRows(model, where, { limit: 1 })[0] ?? null;
          const { sql, params } = buildWhere(where);
          const set = cols.map((c) => `${c} = ?`).join(", ");
          db.query(`UPDATE ${model} SET ${set}${sql}`, [
            ...cols.map((c) => bindable(update[c])),
            ...params,
          ]);
          return selectRows(model, where, { limit: 1 })[0] ?? null;
        },

        updateMany: async ({
          model,
          where,
          update,
        }: {
          model: string;
          where: WhereClause[];
          update: Record<string, unknown>;
        }) => {
          const cols = Object.keys(update);
          if (cols.length === 0) return 0;
          const { sql, params } = buildWhere(where);
          const set = cols.map((c) => `${c} = ?`).join(", ");
          using stmt = db.prepare(`UPDATE ${model} SET ${set}${sql}`);
          return stmt.run(...cols.map((c) => bindable(update[c])), ...params);
        },

        delete: async ({ model, where }: { model: string; where: WhereClause[] }) => {
          const { sql, params } = buildWhere(where);
          db.query(`DELETE FROM ${model}${sql}`, params);
        },

        deleteMany: async ({ model, where }: { model: string; where: WhereClause[] }) => {
          const { sql, params } = buildWhere(where);
          using stmt = db.prepare(`DELETE FROM ${model}${sql}`);
          return stmt.run(...params);
        },
      };
    }) as unknown as CreateCustomAdapter,
  });
}
