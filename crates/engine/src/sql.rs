//! A focused SQL frontend: a recursive-descent parser (over the token stream
//! from [`crate::lex`]) producing the engine's internal statement AST (spec 02 —
//! parser stage). The supported subset is deliberately small for Phase 1 (DDL +
//! DML + queries + transaction control); anything outside it is rejected with
//! `ENGINE_ERR_SQL` rather than silently mis-parsed.

use crate::error::{EngineError, Result};
use crate::lex::{lex, Tok};
use crate::value::{ColumnType, Value};
use crate::vector::{IndexParams, Metric};

/// The session GUC that overrides HNSW `ef_search` per connection (VH-3).
pub const VECTOR_EF_SEARCH_GUC: &str = "twill.vector_ef_search";

// ---- AST ------------------------------------------------------------------

#[derive(Debug)]
pub enum Stmt {
    CreateTable {
        name: String,
        columns: Vec<ColumnSpec>,
        foreign_keys: Vec<ForeignKeySpec>,
        /// Table-level / composite `PRIMARY KEY (cols)` columns (stage 6D).
        primary_key: Vec<String>,
        /// Table-level / composite `UNIQUE (cols)` constraints (stage 6D).
        uniques: Vec<Vec<String>>,
        /// `CHECK (expr)` predicate texts (column- and table-level, stage 6D).
        checks: Vec<String>,
        if_not_exists: bool,
    },
    /// `ALTER TABLE … <action>` (stage 6D).
    AlterTable {
        table: String,
        action: AlterAction,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex {
        name: String,
        table: String,
        column: String,
        params: IndexParams,
        if_not_exists: bool,
    },
    DropIndex {
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        source: InsertSource,
        /// Conflict action for a clashing key (`ON CONFLICT …` / `OR …`).
        on_conflict: OnConflict,
        /// `RETURNING …` projection over the inserted rows, if present.
        returning: Option<Vec<SelItem>>,
    },
    Select(Box<SelectStmt>),
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        /// `UPDATE … FROM <sources>` — extra row sources joined to the target via
        /// the `WHERE` clause (Postgres). `None` for a plain single-table update.
        from: Option<FromClause>,
        filter: Option<Expr>,
        returning: Option<Vec<SelItem>>,
    },
    Delete {
        table: String,
        /// `DELETE … USING <sources>` — extra row sources joined to the target via
        /// the `WHERE` clause (Postgres). `None` for a plain single-table delete.
        using: Option<FromClause>,
        filter: Option<Expr>,
        returning: Option<Vec<SelItem>>,
    },
    Begin,
    Commit,
    Rollback,
    /// `SAVEPOINT name` (stage 6D).
    Savepoint(String),
    /// `RELEASE [SAVEPOINT] name` (stage 6D).
    ReleaseSavepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] name` (stage 6D).
    RollbackTo(String),
    /// An accepted-and-ignored session statement — `SET`/`PRAGMA`/`VACUUM`/
    /// `ANALYZE`/`RESET`/`DISCARD` (stage 6E dialect shim).
    Noop,
    /// `SET twill.vector_ef_search = N` / `RESET twill.vector_ef_search` (VH-3):
    /// the per-session HNSW recall knob. `Some(n)` sets the search width for
    /// subsequent KNN queries on this connection; `None` resets it to each index's
    /// configured default.
    SetVectorEf(Option<usize>),
    /// `SHOW name` — returns a one-row result for the setting (stage 6E).
    Show(String),
    /// `EXPLAIN [ANALYZE] <statement>` — returns a one-line plan (stage 6E).
    Explain(Box<Stmt>),
    /// `CREATE [OR REPLACE] VIEW name [(cols)] AS <select>` (deferred 6B item).
    /// `sql` is the full statement text, stored verbatim so the view can be
    /// re-parsed on WAL replay (a new additive durable catalog fact). DDL stays
    /// autocommit-only.
    CreateView {
        name: String,
        query: Box<SelectStmt>,
        sql: String,
        or_replace: bool,
        if_not_exists: bool,
    },
    /// `DROP VIEW [IF EXISTS] name`.
    DropView {
        name: String,
        if_exists: bool,
    },
}

/// Where an `INSERT` gets its rows: literal `VALUES` tuples or the result of a
/// query (`INSERT … SELECT`). The query is evaluated to rows that feed the same
/// staging/validation loop the `VALUES` form uses.
#[derive(Debug)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<SelectStmt>),
}

/// The action taken when an `INSERT` row clashes with an existing primary key.
/// Covers Postgres `ON CONFLICT …` and SQLite `INSERT OR …` (mapped on parse).
#[derive(Debug)]
pub enum OnConflict {
    /// Default: a clash is a `UNIQUE` constraint violation.
    Error,
    /// `ON CONFLICT … DO NOTHING` / `INSERT OR IGNORE`: skip the clashing row.
    Nothing,
    /// `ON CONFLICT … DO UPDATE SET …`: update the existing row. `excluded.<col>`
    /// in an assignment refers to the row proposed for insertion.
    Update {
        sets: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    /// `INSERT OR REPLACE`: replace the existing row wholesale with the new one.
    Replace,
}

#[derive(Debug)]
pub struct ColumnSpec {
    pub name: String,
    pub ty: ColumnType,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
    pub autoincrement: bool,
    /// `DEFAULT <expr>` text, captured for re-parsing on insert (stage 6D).
    pub default_sql: Option<String>,
    /// `CHECK (<expr>)` text declared inline on the column (stage 6D).
    pub check_sql: Option<String>,
    /// An inline `REFERENCES <table>[(<col>)]` constraint: the referenced table
    /// and, optionally, the referenced column (defaulting to its primary key).
    pub references: Option<(String, Option<String>)>,
}

/// One `ALTER TABLE` action (stage 6D). DDL stays autocommit-only.
#[derive(Debug)]
pub enum AlterAction {
    AddColumn(ColumnSpec),
    DropColumn { name: String, if_exists: bool },
    RenameColumn { from: String, to: String },
    RenameTable { to: String },
}

/// A foreign key as parsed from `CREATE TABLE` (inline or table-level). The
/// referenced columns may be empty, meaning "the referenced table's primary
/// key", resolved against the catalog when the table is created.
#[derive(Debug)]
pub struct ForeignKeySpec {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub foreign_table: String,
    pub foreign_columns: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SelectStmt {
    /// Non-recursive common table expressions (`WITH a AS (…), …`). Empty unless
    /// the query opened with `WITH`. Carried only on the outermost select.
    pub with: Vec<CteDef>,
    pub distinct: bool,
    pub items: Vec<SelItem>,
    pub from: Option<FromClause>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    /// Trailing set-operation chain (`UNION`/`INTERSECT`/`EXCEPT [ALL]`); the
    /// `order_by`/`limit`/`offset` below apply to the combined result.
    pub set_ops: Vec<SetOpPart>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

/// A `WITH` binding: a name and the query it stands for (materialized once).
#[derive(Debug, Clone)]
pub struct CteDef {
    pub name: String,
    pub query: Box<SelectStmt>,
}

/// One arm of a set-operation chain after the leading select.
#[derive(Debug, Clone)]
pub struct SetOpPart {
    pub op: SetOp,
    pub all: bool,
    pub query: Box<SelectStmt>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

/// The `FROM` clause: a single source, a join tree, or a derived table.
#[derive(Debug, Clone)]
pub enum FromClause {
    /// A base table or CTE reference, with an optional alias.
    Table { name: String, alias: Option<String> },
    /// A parenthesized subquery `(SELECT …) AS alias`.
    Derived {
        query: Box<SelectStmt>,
        alias: String,
    },
    /// A join of two sources. `on`/`using` are empty for `CROSS JOIN`.
    Join {
        left: Box<FromClause>,
        right: Box<FromClause>,
        kind: JoinKind,
        on: Option<Box<Expr>>,
        using: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// One `ORDER BY` key: the sort expression, direction, and NULL placement.
/// `nulls_first` defaults to the engine's historical rule (NULLs first on `ASC`,
/// last on `DESC`) unless an explicit `NULLS FIRST`/`NULLS LAST` overrides it.
#[derive(Debug, Clone)]
pub struct OrderKey {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: bool,
}

#[derive(Debug, Clone)]
pub enum SelItem {
    /// `*` (all columns) or `alias.*` (all columns of one source).
    Star {
        qualifier: Option<String>,
    },
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
    /// `json_agg(expr)` — aggregate the group's values into a JSON array. The
    /// data-path shape PostgREST wraps result sets in.
    JsonAgg,
    /// `group_concat`/`string_agg` — join the group's values with a separator.
    GroupConcat,
}

#[derive(Debug, Clone)]
pub enum AggArg {
    Star,
    Expr(Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Null,
    Int(i64),
    Real(f64),
    Str(String),
    Vector(Vec<f32>),
    /// A pre-computed value — produced when the relational planner folds a
    /// non-correlated subquery down to a constant.
    Lit(Value),
    Param(usize), // 1-based
    Column(String),
    Binary {
        op: BinOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Unary {
        op: UnOp,
        e: Box<Expr>,
    },
    IsNull {
        e: Box<Expr>,
        negated: bool,
    },
    Like {
        e: Box<Expr>,
        pattern: Box<Expr>,
        /// Optional `ESCAPE c` — the character that escapes a literal `%`/`_`.
        escape: Option<Box<Expr>>,
        negated: bool,
        /// `true` for case-insensitive `ILIKE` (Postgres); `false` for `LIKE`.
        /// (The engine's historical `LIKE` is case-insensitive; the split lands
        /// in stage 6E. Until then both are case-insensitive — see the executor.)
        insensitive: bool,
    },
    /// `expr [NOT] IN (a, b, c)` — membership against a literal value list.
    InList {
        e: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `expr [NOT] BETWEEN lo AND hi` — desugared to `>= lo AND <= hi` at eval.
    Between {
        e: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        negated: bool,
    },
    /// `CASE [operand] WHEN cond THEN result … [ELSE result] END`. A searched
    /// `CASE` has `operand == None`; a simple `CASE` compares `operand` to each
    /// `when` value for equality.
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        els: Option<Box<Expr>>,
    },
    /// `excluded.<col>` inside an `ON CONFLICT DO UPDATE` — the proposed-insert
    /// value for that column (resolved from the upsert's excluded row).
    Excluded(String),
    /// The `DEFAULT` keyword used as an `INSERT … VALUES` cell — substituted with
    /// the column's default (or autoincrement) during insert staging (stage 6D).
    Default,
    /// Postgres `expr::type` cast — coerces the inner value to `target`.
    Cast {
        e: Box<Expr>,
        target: CastTarget,
    },
    /// A scalar function call, e.g. `coalesce(a, 0)`, `lower(name)`. Aggregate
    /// functions parse to [`Expr::Aggregate`] instead.
    Func {
        name: String,
        args: Vec<Expr>,
    },
    /// An aggregate over the current group (or the whole table when there is no
    /// `GROUP BY`). Parses anywhere an expression may appear, so it nests inside
    /// scalars — e.g. `coalesce(json_agg(x), '[]')`.
    Aggregate {
        func: AggFunc,
        arg: AggArg,
        /// `DISTINCT` argument (e.g. `COUNT(DISTINCT x)`).
        distinct: bool,
        /// Separator for `group_concat`/`string_agg` (default `,`).
        sep: Option<Box<Expr>>,
    },
    /// A qualified column reference `table.col` / `alias.col`. Resolved against
    /// the active `FROM` namespace by the relational executor; the single-table
    /// path ignores the qualifier.
    Qualified(String, String),
    /// A scalar subquery `(SELECT …)` — must yield at most one row/column.
    /// Non-correlated only (evaluated once before the main scan).
    ScalarSubquery(Box<SelectStmt>),
    /// `[NOT] EXISTS (SELECT …)` (non-correlated).
    Exists {
        query: Box<SelectStmt>,
        negated: bool,
    },
    /// `expr [NOT] IN (SELECT …)` (non-correlated).
    InSubquery {
        e: Box<Expr>,
        query: Box<SelectStmt>,
        negated: bool,
    },
}

/// The storage class a `::type` cast coerces to. Types we don't specifically
/// model (uuid, json, timestamp, regclass, …) map to [`CastTarget::Passthrough`]
/// and leave the value unchanged — lenient by design, for wire compatibility.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CastTarget {
    Int,
    Real,
    Text,
    Bool,
    Passthrough,
}

impl CastTarget {
    /// Map a SQL type's leading word to a cast target.
    fn from_word(w: &str) -> CastTarget {
        match w.to_ascii_lowercase().as_str() {
            "int" | "integer" | "int2" | "int4" | "int8" | "smallint" | "bigint" | "oid"
            | "serial" | "bigserial" => CastTarget::Int,
            "real" | "float" | "float4" | "float8" | "double" | "numeric" | "decimal" => {
                CastTarget::Real
            }
            "text" | "varchar" | "character" | "char" | "name" | "json" | "jsonb" | "uuid"
            | "citext" | "bytea" | "bpchar" => CastTarget::Text,
            "bool" | "boolean" => CastTarget::Bool,
            _ => CastTarget::Passthrough,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// String concatenation `||` (NULL-propagating, operands rendered to text).
    Concat,
    /// Vector distance operators (spec 12): `<->` L2, `<=>` cosine, `<#>` inner
    /// product. Each evaluates to a REAL distance and selects the matching
    /// HNSW metric when pushed into an index scan.
    VecL2,
    VecCosine,
    VecIp,
}

impl BinOp {
    /// The HNSW metric a distance operator queries under, if it is one.
    pub fn vec_metric(self) -> Option<Metric> {
        match self {
            BinOp::VecL2 => Some(Metric::L2),
            BinOp::VecCosine => Some(Metric::Cosine),
            BinOp::VecIp => Some(Metric::InnerProduct),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum UnOp {
    Not,
    Neg,
}

// ---- parser ---------------------------------------------------------------

/// Parse a single SQL statement, returning it plus the parameter count (the
/// highest placeholder index seen across `?` / `$n` / `:name` forms).
pub fn parse(sql: &str) -> Result<(Stmt, usize)> {
    let toks = lex(sql)?;
    let mut p = Parser::new(toks);
    p.src = sql.to_string();
    let stmt = p.statement()?;
    p.skip(Tok::Semi);
    if p.peek() != &Tok::Eof {
        return Err(EngineError::sql("trailing tokens after statement"));
    }
    Ok((stmt, p.max_param))
}

/// Parse a standalone expression (used to re-parse stored `DEFAULT`/`CHECK`
/// texts at insert/update time, stage 6D). Parameters are not expected here.
pub fn parse_expr(sql: &str) -> Result<Expr> {
    let toks = lex(sql)?;
    let mut p = Parser::new(toks);
    let e = p.expr()?;
    if p.peek() != &Tok::Eof {
        return Err(EngineError::sql("trailing tokens after expression"));
    }
    Ok(e)
}

/// Collect the base-relation names a query references in any `FROM` position
/// (recursively through joins, derived tables, set operations, CTE bodies, and
/// expression subqueries), excluding the names bound by this query's own `WITH`.
/// Used to reject view-definition cycles at `CREATE VIEW` time.
pub fn referenced_relations(sel: &SelectStmt) -> Vec<String> {
    let mut out = Vec::new();
    collect_select_refs(sel, &mut out);
    let local: Vec<String> = sel
        .with
        .iter()
        .map(|c| c.name.to_ascii_lowercase())
        .collect();
    out.retain(|n| !local.contains(&n.to_ascii_lowercase()));
    out
}

fn collect_select_refs(sel: &SelectStmt, out: &mut Vec<String>) {
    for cte in &sel.with {
        collect_select_refs(&cte.query, out);
    }
    if let Some(f) = &sel.from {
        collect_from_refs(f, out);
    }
    for item in &sel.items {
        if let SelItem::Expr { expr, .. } = item {
            collect_expr_refs(expr, out);
        }
    }
    if let Some(e) = &sel.filter {
        collect_expr_refs(e, out);
    }
    for e in &sel.group_by {
        collect_expr_refs(e, out);
    }
    if let Some(e) = &sel.having {
        collect_expr_refs(e, out);
    }
    for k in &sel.order_by {
        collect_expr_refs(&k.expr, out);
    }
    for part in &sel.set_ops {
        collect_select_refs(&part.query, out);
    }
}

fn collect_from_refs(f: &FromClause, out: &mut Vec<String>) {
    match f {
        FromClause::Table { name, .. } => out.push(name.clone()),
        FromClause::Derived { query, .. } => collect_select_refs(query, out),
        FromClause::Join {
            left, right, on, ..
        } => {
            collect_from_refs(left, out);
            collect_from_refs(right, out);
            if let Some(e) = on {
                collect_expr_refs(e, out);
            }
        }
    }
}

fn collect_expr_refs(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::ScalarSubquery(q) => collect_select_refs(q, out),
        Expr::Exists { query, .. } => collect_select_refs(query, out),
        Expr::InSubquery { e, query, .. } => {
            collect_expr_refs(e, out);
            collect_select_refs(query, out);
        }
        Expr::Binary { l, r, .. } => {
            collect_expr_refs(l, out);
            collect_expr_refs(r, out);
        }
        Expr::Unary { e, .. } | Expr::IsNull { e, .. } | Expr::Cast { e, .. } => {
            collect_expr_refs(e, out)
        }
        Expr::Like {
            e, pattern, escape, ..
        } => {
            collect_expr_refs(e, out);
            collect_expr_refs(pattern, out);
            if let Some(x) = escape {
                collect_expr_refs(x, out);
            }
        }
        Expr::InList { e, list, .. } => {
            collect_expr_refs(e, out);
            for x in list {
                collect_expr_refs(x, out);
            }
        }
        Expr::Between { e, lo, hi, .. } => {
            collect_expr_refs(e, out);
            collect_expr_refs(lo, out);
            collect_expr_refs(hi, out);
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            if let Some(o) = operand {
                collect_expr_refs(o, out);
            }
            for (c, r) in whens {
                collect_expr_refs(c, out);
                collect_expr_refs(r, out);
            }
            if let Some(x) = els {
                collect_expr_refs(x, out);
            }
        }
        Expr::Func { args, .. } => {
            for a in args {
                collect_expr_refs(a, out);
            }
        }
        Expr::Aggregate { arg, sep, .. } => {
            if let AggArg::Expr(x) = arg {
                collect_expr_refs(x, out);
            }
            if let Some(s) = sep {
                collect_expr_refs(s, out);
            }
        }
        _ => {}
    }
}

struct Parser {
    toks: Vec<Tok>,
    /// The original statement text, kept so `CREATE VIEW` can store its body
    /// verbatim (re-parsed on replay). Empty for the standalone-expression path.
    src: String,
    pos: usize,
    /// Next sequential index assigned to a `?` / new `:name` placeholder.
    next_param: usize,
    /// Highest placeholder index seen (drives the reported parameter count).
    max_param: usize,
    /// Index assigned to each `:name`, so a repeated name reuses its slot.
    named_params: std::collections::HashMap<String, usize>,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Parser {
        Parser {
            toks,
            src: String::new(),
            pos: 0,
            next_param: 1,
            max_param: 0,
            named_params: std::collections::HashMap::new(),
        }
    }

    /// Resolve a placeholder token to its 1-based index, allocating a sequential
    /// slot for `?` and `:name` and honouring the explicit number of `$n`.
    fn param_index(&mut self, tok: &Tok) -> usize {
        let idx = match tok {
            Tok::NumParam(n) => *n,
            Tok::NamedParam(name) => {
                if let Some(&i) = self.named_params.get(name) {
                    i
                } else {
                    let i = self.next_param;
                    self.next_param += 1;
                    self.named_params.insert(name.clone(), i);
                    i
                }
            }
            _ => {
                let i = self.next_param;
                self.next_param += 1;
                i
            }
        };
        self.max_param = self.max_param.max(idx);
        idx
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn skip(&mut self, t: Tok) -> bool {
        if self.peek() == &t {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Tok) -> Result<()> {
        if self.peek() == &t {
            self.bump();
            Ok(())
        } else {
            Err(EngineError::sql(format!(
                "expected {:?}, found {:?}",
                t,
                self.peek()
            )))
        }
    }

    fn peek_word(&self) -> Option<&str> {
        match self.peek() {
            Tok::Word(w) => Some(w.as_str()),
            _ => None,
        }
    }
    /// Match a keyword case-insensitively (consumes on match).
    fn eat_kw(&mut self, kw: &str) -> bool {
        if let Tok::Word(w) = self.peek() {
            if w.eq_ignore_ascii_case(kw) {
                self.bump();
                return true;
            }
        }
        false
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }
    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(EngineError::sql(format!(
                "expected keyword {kw}, found {:?}",
                self.peek()
            )))
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.bump() {
            Tok::Word(w) => Ok(w),
            other => Err(EngineError::sql(format!(
                "expected identifier, found {other:?}"
            ))),
        }
    }

    fn next_is_kw(&self, kw: &str) -> bool {
        matches!(self.toks.get(self.pos + 1), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    /// Consume tokens up to (not including) a statement terminator — for the
    /// accepted-and-ignored session statements (stage 6E).
    fn skip_to_end(&mut self) {
        while !matches!(self.peek(), Tok::Semi | Tok::Eof) {
            self.bump();
        }
    }

    /// Consume a balanced `( … )` group (e.g. an `EXPLAIN (options)` list).
    fn skip_parens(&mut self) {
        if self.peek() != &Tok::LParen {
            return;
        }
        let mut depth = 0;
        loop {
            match self.peek() {
                Tok::LParen => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                    continue;
                }
                Tok::Eof => return,
                _ => {}
            }
            self.bump();
        }
    }

    fn statement(&mut self) -> Result<Stmt> {
        if self.is_kw("create") {
            if self.next_is_kw("index") {
                self.create_index()
            } else if self.create_is_view() {
                self.create_view()
            } else {
                self.create_table()
            }
        } else if self.is_kw("drop") {
            if self.next_is_kw("index") {
                self.drop_index()
            } else if self.next_is_kw("view") {
                self.drop_view()
            } else {
                self.drop_table()
            }
        } else if self.is_kw("alter") {
            self.alter_table()
        } else if self.is_kw("insert") {
            self.insert()
        } else if self.is_kw("select") || self.is_kw("with") {
            Ok(Stmt::Select(Box::new(self.select()?)))
        } else if self.is_kw("update") {
            self.update()
        } else if self.is_kw("delete") {
            self.delete()
        } else if let Some(stmt) = self.utility_stmt()? {
            stmt
        } else if let Some(stmt) = self.transaction_stmt()? {
            stmt
        } else {
            Err(EngineError::sql(format!(
                "unsupported statement starting at {:?}",
                self.peek()
            )))
        }
    }

    /// `EXPLAIN` / `SHOW` plus the accepted-and-ignored session statements
    /// (`SET`/`PRAGMA`/`VACUUM`/`ANALYZE`/`RESET`/`DISCARD`); `None` if the next
    /// token starts none of them. (Stage 6E — these only peek, never consume on a
    /// miss, so the caller can fall through to the next group.)
    fn utility_stmt(&mut self) -> Result<Option<Result<Stmt>>> {
        if self.is_kw("explain") {
            self.bump();
            let _ = self.eat_kw("analyze");
            let _ = self.eat_kw("verbose");
            if self.peek() == &Tok::LParen {
                self.skip_parens(); // EXPLAIN (options) — accepted, ignored
            }
            Ok(Some(self.statement().map(|s| Stmt::Explain(Box::new(s)))))
        } else if self.is_kw("show") {
            self.bump();
            // `SHOW name` / `SHOW TRANSACTION ISOLATION LEVEL` / `SHOW ALL`. A
            // dotted GUC name (`twill.vector_ef_search`) reads as one segment.
            let mut parts = Vec::new();
            while self.peek_word().is_some() {
                parts.push(self.read_dotted_name());
            }
            Ok(Some(Ok(Stmt::Show(parts.join(" ")))))
        } else if self.is_kw("set") {
            self.bump();
            // `SET [SESSION|LOCAL] name [=|TO] value`. Recognize the vector recall
            // knob (VH-3); every other GUC stays accepted-and-ignored.
            let _ = self.eat_kw("session") || self.eat_kw("local");
            let name = self.read_dotted_name();
            if name.eq_ignore_ascii_case(VECTOR_EF_SEARCH_GUC) {
                let _ = self.skip(Tok::Eq) || self.eat_kw("to");
                let value = if self.eat_kw("default") {
                    None
                } else {
                    Some(self.int_value()?.max(1) as usize)
                };
                self.skip_to_end();
                return Ok(Some(Ok(Stmt::SetVectorEf(value))));
            }
            self.skip_to_end();
            Ok(Some(Ok(Stmt::Noop)))
        } else if self.is_kw("reset") {
            self.bump();
            let name = self.read_dotted_name();
            self.skip_to_end();
            if name.eq_ignore_ascii_case(VECTOR_EF_SEARCH_GUC) {
                Ok(Some(Ok(Stmt::SetVectorEf(None))))
            } else {
                Ok(Some(Ok(Stmt::Noop)))
            }
        } else if self.is_kw("pragma")
            || self.is_kw("vacuum")
            || self.is_kw("analyze")
            || self.is_kw("discard")
        {
            self.skip_to_end();
            Ok(Some(Ok(Stmt::Noop)))
        } else {
            Ok(None)
        }
    }

    /// Read a (possibly schema-qualified) GUC / setting name: `word(.word)*`,
    /// joined with `.` — e.g. `twill.vector_ef_search`. Returns an empty string
    /// when the next token is not a word.
    fn read_dotted_name(&mut self) -> String {
        let mut s = String::new();
        match self.peek_word() {
            Some(w) => {
                s.push_str(w);
                self.bump();
            }
            None => return s,
        }
        while self.peek() == &Tok::Dot {
            self.bump();
            if let Some(w) = self.peek_word() {
                s.push('.');
                s.push_str(w);
                self.bump();
            } else {
                break;
            }
        }
        s
    }

    /// Transaction-control statements (`BEGIN`/`START`, `COMMIT`/`END`,
    /// `SAVEPOINT`/`RELEASE`, `ROLLBACK`/`ABORT [TO]`); `None` if the next token
    /// starts none of them. `eat_kw` only consumes on a match, so a miss leaves
    /// the cursor untouched.
    fn transaction_stmt(&mut self) -> Result<Option<Result<Stmt>>> {
        let stmt = if self.eat_kw("begin") {
            let _ = self.eat_kw("transaction") || self.eat_kw("work");
            self.eat_transaction_modes();
            Ok(Stmt::Begin)
        } else if self.eat_kw("start") {
            let _ = self.eat_kw("transaction");
            self.eat_transaction_modes();
            Ok(Stmt::Begin)
        } else if self.eat_kw("commit") || self.eat_kw("end") {
            // END [WORK|TRANSACTION] is a COMMIT synonym.
            let _ = self.eat_kw("transaction") || self.eat_kw("work");
            Ok(Stmt::Commit)
        } else if self.eat_kw("savepoint") {
            self.ident().map(Stmt::Savepoint)
        } else if self.eat_kw("release") {
            let _ = self.eat_kw("savepoint");
            self.ident().map(Stmt::ReleaseSavepoint)
        } else if self.eat_kw("rollback") || self.eat_kw("abort") {
            // ABORT [WORK|TRANSACTION] is a ROLLBACK synonym (PostgREST uses it).
            let _ = self.eat_kw("transaction") || self.eat_kw("work");
            // `ROLLBACK TO [SAVEPOINT] name` rolls back to a savepoint.
            if self.eat_kw("to") {
                let _ = self.eat_kw("savepoint");
                self.ident().map(Stmt::RollbackTo)
            } else {
                Ok(Stmt::Rollback)
            }
        } else {
            return Ok(None);
        };
        Ok(Some(stmt))
    }

    /// Consume the optional transaction-mode list after BEGIN / START
    /// TRANSACTION (`ISOLATION LEVEL …`, `READ ONLY` / `READ WRITE`,
    /// `[NOT] DEFERRABLE`), comma- or space-separated. The engine runs one
    /// snapshot-isolation mode, so these are accepted and ignored — needed for
    /// clients (PostgREST) that open `BEGIN ISOLATION LEVEL READ COMMITTED READ
    /// ONLY`.
    fn eat_transaction_modes(&mut self) {
        loop {
            let _ = self.skip(Tok::Comma);
            if self.eat_kw("isolation") {
                let _ = self.eat_kw("level");
                // SERIALIZABLE | REPEATABLE READ | READ COMMITTED | READ UNCOMMITTED
                if self.eat_kw("serializable") {
                } else if self.eat_kw("repeatable") {
                    let _ = self.eat_kw("read");
                } else if self.eat_kw("read") {
                    let _ = self.eat_kw("committed") || self.eat_kw("uncommitted");
                }
            } else if self.eat_kw("read") {
                let _ = self.eat_kw("only") || self.eat_kw("write");
            } else if self.eat_kw("not") {
                let _ = self.eat_kw("deferrable");
            } else if self.eat_kw("deferrable") {
                // accepted
            } else {
                break;
            }
        }
    }

    fn create_table(&mut self) -> Result<Stmt> {
        self.expect_kw("create")?;
        self.expect_kw("table")?;
        let if_not_exists = if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        self.expect(Tok::LParen)?;
        let mut columns = Vec::new();
        let mut foreign_keys = Vec::new();
        let mut primary_key: Vec<String> = Vec::new();
        let mut uniques: Vec<Vec<String>> = Vec::new();
        let mut checks: Vec<String> = Vec::new();
        loop {
            // A table-level constraint (`[CONSTRAINT n] PRIMARY KEY/UNIQUE/FOREIGN
            // KEY/CHECK …`) rather than a column definition.
            if self.peek_table_constraint() {
                self.table_constraint(
                    &mut foreign_keys,
                    &mut primary_key,
                    &mut uniques,
                    &mut checks,
                )?;
            } else {
                let col = self.column_spec()?;
                if let Some((ft, fc)) = col.references.clone() {
                    foreign_keys.push(ForeignKeySpec {
                        name: None,
                        columns: vec![col.name.clone()],
                        foreign_table: ft,
                        foreign_columns: fc.into_iter().collect(),
                    });
                }
                if let Some(c) = &col.check_sql {
                    checks.push(c.clone());
                }
                columns.push(col);
            }
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(Tok::RParen)?;
        if columns.is_empty() {
            return Err(EngineError::sql("table must have at least one column"));
        }
        Ok(Stmt::CreateTable {
            name,
            columns,
            foreign_keys,
            primary_key,
            uniques,
            checks,
            if_not_exists,
        })
    }

    /// `ALTER TABLE name <action>`: ADD/DROP/RENAME COLUMN or RENAME TO.
    fn alter_table(&mut self) -> Result<Stmt> {
        self.expect_kw("alter")?;
        self.expect_kw("table")?;
        let _ = self.eat_kw("if") && self.eat_kw("exists");
        let table = self.ident()?;
        let action = if self.eat_kw("add") {
            let _ = self.eat_kw("column");
            AlterAction::AddColumn(self.column_spec()?)
        } else if self.eat_kw("drop") {
            let _ = self.eat_kw("column");
            let if_exists = self.eat_kw("if") && self.eat_kw("exists");
            AlterAction::DropColumn {
                name: self.ident()?,
                if_exists,
            }
        } else if self.eat_kw("rename") {
            if self.eat_kw("to") {
                AlterAction::RenameTable { to: self.ident()? }
            } else {
                let _ = self.eat_kw("column");
                let from = self.ident()?;
                self.expect_kw("to")?;
                AlterAction::RenameColumn {
                    from,
                    to: self.ident()?,
                }
            }
        } else {
            return Err(EngineError::sql(
                "unsupported ALTER TABLE action (expected ADD/DROP/RENAME)",
            ));
        };
        Ok(Stmt::AlterTable { table, action })
    }

    fn column_spec(&mut self) -> Result<ColumnSpec> {
        let name = self.ident()?;
        // Optional type name (one or more words, optional (n) / (n,m)). SERIAL
        // family declares an integer autoincrement column.
        let mut ty = ColumnType::Text;
        let mut autoincrement = false;
        if let Some(w) = self.peek_word() {
            if !is_constraint_kw(w) {
                let tyname = self.ident()?;
                if is_serial_type(&tyname) {
                    ty = ColumnType::Integer;
                    autoincrement = true;
                } else {
                    ty = ColumnType::from_sql(&tyname);
                }
                ty = self.parse_type_size(ty)?;
            }
        }
        let mut primary_key = false;
        let mut not_null = false;
        let mut unique = false;
        let mut references = None;
        let mut default_sql = None;
        let mut check_sql = None;
        loop {
            if self.eat_kw("primary") {
                self.expect_kw("key")?;
                primary_key = true;
                not_null = true;
                // SQLite spells `INTEGER PRIMARY KEY AUTOINCREMENT`.
                if self.eat_kw("autoincrement") {
                    autoincrement = true;
                }
            } else if self.eat_kw("not") {
                self.expect_kw("null")?;
                not_null = true;
            } else if self.eat_kw("null") {
                // explicit nullable
            } else if self.eat_kw("unique") {
                unique = true;
            } else if self.eat_kw("autoincrement") {
                autoincrement = true;
            } else if self.eat_kw("default") {
                default_sql = Some(self.capture_expr_sql()?);
            } else if self.eat_kw("check") {
                check_sql = Some(self.capture_paren_expr_sql()?);
            } else if self.eat_kw("references") {
                references = Some(self.references_target()?);
            } else if self.eat_kw("collate") {
                let _ = self.ident()?; // collation name accepted and ignored
            } else if self.eat_kw("generated") {
                return Err(EngineError::sql("GENERATED columns are not supported"));
            } else {
                break;
            }
        }
        Ok(ColumnSpec {
            name,
            ty,
            primary_key,
            not_null,
            unique,
            autoincrement,
            default_sql,
            check_sql,
            references,
        })
    }

    /// Capture a `DEFAULT` expression's source by parsing it and re-stringifying
    /// the tokens it consumed (re-parseable on replay).
    fn capture_expr_sql(&mut self) -> Result<String> {
        let start = self.pos;
        let _ = self.expr()?;
        Ok(render_tokens(&self.toks[start..self.pos]))
    }

    /// Capture a parenthesized `CHECK (expr)` body's source.
    fn capture_paren_expr_sql(&mut self) -> Result<String> {
        self.expect(Tok::LParen)?;
        let start = self.pos;
        let _ = self.expr()?;
        let text = render_tokens(&self.toks[start..self.pos]);
        self.expect(Tok::RParen)?;
        Ok(text)
    }

    /// The `<table>[(<col>)]` after a `REFERENCES` keyword.
    fn references_target(&mut self) -> Result<(String, Option<String>)> {
        let table = self.ident()?;
        let column = if self.skip(Tok::LParen) {
            let c = self.ident()?;
            self.expect(Tok::RParen)?;
            Some(c)
        } else {
            None
        };
        Ok((table, column))
    }

    /// Does the next token begin a table-level constraint clause?
    fn peek_table_constraint(&self) -> bool {
        self.peek_word().is_some_and(|w| {
            ["constraint", "primary", "unique", "foreign", "check"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
        })
    }

    /// Parse a table-level constraint into the right collection: `FOREIGN KEY`,
    /// composite `PRIMARY KEY (cols)`, `UNIQUE (cols)`, or `CHECK (expr)`.
    fn table_constraint(
        &mut self,
        fks: &mut Vec<ForeignKeySpec>,
        pks: &mut Vec<String>,
        uniques: &mut Vec<Vec<String>>,
        checks: &mut Vec<String>,
    ) -> Result<()> {
        let mut name = None;
        if self.eat_kw("constraint") {
            name = Some(self.ident()?);
        }
        if self.eat_kw("foreign") {
            self.expect_kw("key")?;
            let columns = self.paren_ident_list()?;
            self.expect_kw("references")?;
            let foreign_table = self.ident()?;
            let foreign_columns = if self.peek() == &Tok::LParen {
                self.paren_ident_list()?
            } else {
                Vec::new()
            };
            fks.push(ForeignKeySpec {
                name,
                columns,
                foreign_table,
                foreign_columns,
            });
            return Ok(());
        }
        if self.eat_kw("primary") {
            self.expect_kw("key")?;
            pks.extend(self.paren_ident_list()?);
        } else if self.eat_kw("unique") {
            uniques.push(self.paren_ident_list()?);
        } else if self.eat_kw("check") {
            checks.push(self.capture_paren_expr_sql()?);
        }
        Ok(())
    }

    /// A parenthesized, comma-separated identifier list: `(a, b, c)`.
    fn paren_ident_list(&mut self) -> Result<Vec<String>> {
        self.expect(Tok::LParen)?;
        let mut cols = Vec::new();
        loop {
            cols.push(self.ident()?);
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(Tok::RParen)?;
        Ok(cols)
    }

    /// Parse an optional `(n)` / `(p,s)` type suffix. For a vector type the first
    /// integer is the declared dimension; for any other type the suffix is parsed
    /// and ignored (affinity is name-based).
    fn parse_type_size(&mut self, ty: ColumnType) -> Result<ColumnType> {
        if !self.skip(Tok::LParen) {
            return Ok(ty);
        }
        let mut first_int: Option<i64> = None;
        while self.peek() != &Tok::RParen && self.peek() != &Tok::Eof {
            if first_int.is_none() {
                if let Tok::Int(n) = self.peek() {
                    first_int = Some(*n);
                }
            }
            self.bump();
        }
        self.expect(Tok::RParen)?;
        if matches!(ty, ColumnType::Vector(_)) {
            let d = first_int.unwrap_or(0);
            if d <= 0 {
                return Err(EngineError::sql("vector(N) requires a positive dimension"));
            }
            return Ok(ColumnType::Vector(d as u32));
        }
        Ok(ty)
    }

    /// `CREATE INDEX [IF NOT EXISTS] name ON table USING hnsw (col [opclass])
    /// [WITH (m=.., ef_construction=.., ef_search=.., metric='cosine')]`.
    fn create_index(&mut self) -> Result<Stmt> {
        self.expect_kw("create")?;
        self.expect_kw("index")?;
        let if_not_exists = if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        self.expect_kw("on")?;
        let table = self.ident()?;
        self.expect_kw("using")?;
        let method = self.ident()?;
        if !method.eq_ignore_ascii_case("hnsw") {
            return Err(EngineError::sql(format!(
                "unsupported index method '{method}'; only HNSW is supported"
            )));
        }
        self.expect(Tok::LParen)?;
        let column = self.ident()?;
        let mut params = IndexParams::default();
        // Optional pgvector-style opclass (e.g. vector_cosine_ops).
        if let Some(w) = self.peek_word() {
            if let Some(m) = opclass_metric(w) {
                params.metric = m;
                self.bump();
            }
        }
        self.expect(Tok::RParen)?;
        if self.eat_kw("with") {
            self.parse_index_options(&mut params)?;
        }
        Ok(Stmt::CreateIndex {
            name,
            table,
            column,
            params,
            if_not_exists,
        })
    }

    fn parse_index_options(&mut self, params: &mut IndexParams) -> Result<()> {
        self.expect(Tok::LParen)?;
        loop {
            let key = self.ident()?;
            self.expect(Tok::Eq)?;
            self.apply_index_option(&key, params)?;
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(Tok::RParen)
    }

    fn apply_index_option(&mut self, key: &str, params: &mut IndexParams) -> Result<()> {
        if key.eq_ignore_ascii_case("metric") {
            let name = self.string_or_ident()?;
            params.metric = Metric::from_name(&name)
                .ok_or_else(|| EngineError::sql(format!("unknown vector metric '{name}'")))?;
            return Ok(());
        }
        let n = self.int_value()?;
        match key.to_ascii_lowercase().as_str() {
            "m" => params.m = (n.max(2)) as usize,
            "ef_construction" => params.ef_construction = (n.max(1)) as usize,
            "ef_search" => params.ef_search = (n.max(1)) as usize,
            other => return Err(EngineError::sql(format!("unknown index option '{other}'"))),
        }
        Ok(())
    }

    fn string_or_ident(&mut self) -> Result<String> {
        match self.bump() {
            Tok::Str(s) => Ok(s),
            Tok::Word(w) => Ok(w),
            other => Err(EngineError::sql(format!(
                "expected a name, found {other:?}"
            ))),
        }
    }

    fn int_value(&mut self) -> Result<i64> {
        match self.bump() {
            Tok::Int(n) => Ok(n),
            other => Err(EngineError::sql(format!(
                "expected an integer, found {other:?}"
            ))),
        }
    }

    fn drop_index(&mut self) -> Result<Stmt> {
        self.expect_kw("drop")?;
        self.expect_kw("index")?;
        let if_exists = if self.eat_kw("if") {
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        Ok(Stmt::DropIndex { name, if_exists })
    }

    fn drop_table(&mut self) -> Result<Stmt> {
        self.expect_kw("drop")?;
        self.expect_kw("table")?;
        let if_exists = if self.eat_kw("if") {
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        Ok(Stmt::DropTable { name, if_exists })
    }

    /// Whether the `CREATE` at the cursor introduces a view: `CREATE [OR REPLACE]
    /// [TEMP|TEMPORARY] VIEW …`. Peeks only; does not consume.
    fn create_is_view(&self) -> bool {
        let kw = |i: usize, w: &str| matches!(self.toks.get(i), Some(Tok::Word(s)) if s.eq_ignore_ascii_case(w));
        let mut i = self.pos + 1; // past CREATE
        if kw(i, "or") && kw(i + 1, "replace") {
            i += 2;
        }
        if kw(i, "temp") || kw(i, "temporary") {
            i += 1;
        }
        kw(i, "view")
    }

    fn create_view(&mut self) -> Result<Stmt> {
        self.expect_kw("create")?;
        let or_replace = if self.eat_kw("or") {
            self.expect_kw("replace")?;
            true
        } else {
            false
        };
        let _ = self.eat_kw("temp") || self.eat_kw("temporary");
        self.expect_kw("view")?;
        let if_not_exists = if self.eat_kw("if") {
            self.expect_kw("not")?;
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        // An optional column-alias list `(a, b)` is accepted and ignored — the
        // view's columns are the select's output names.
        if self.peek() == &Tok::LParen {
            let _ = self.paren_ident_list()?;
        }
        self.expect_kw("as")?;
        let query = self.select()?;
        let sql = self.src.trim().trim_end_matches(';').trim().to_string();
        Ok(Stmt::CreateView {
            name,
            query: Box::new(query),
            sql,
            or_replace,
            if_not_exists,
        })
    }

    fn drop_view(&mut self) -> Result<Stmt> {
        self.expect_kw("drop")?;
        self.expect_kw("view")?;
        let if_exists = if self.eat_kw("if") {
            self.expect_kw("exists")?;
            true
        } else {
            false
        };
        let name = self.ident()?;
        Ok(Stmt::DropView { name, if_exists })
    }

    fn insert(&mut self) -> Result<Stmt> {
        self.expect_kw("insert")?;
        // SQLite `INSERT OR REPLACE|IGNORE` maps to a conflict action; the other
        // SQLite resolutions (ABORT/FAIL/ROLLBACK) behave like the default error.
        let mut on_conflict = OnConflict::Error;
        if self.eat_kw("or") {
            if self.eat_kw("replace") {
                on_conflict = OnConflict::Replace;
            } else if self.eat_kw("ignore") {
                on_conflict = OnConflict::Nothing;
            } else {
                let _ = self.eat_kw("abort") || self.eat_kw("fail") || self.eat_kw("rollback");
            }
        }
        self.expect_kw("into")?;
        let table = self.ident()?;
        let columns = if self.peek() == &Tok::LParen && !self.next_is_kw("select") {
            Some(self.paren_ident_list()?)
        } else {
            None
        };
        let source = self.insert_source()?;
        // An explicit `ON CONFLICT …` overrides any `OR …` resolution.
        if self.eat_kw("on") {
            self.expect_kw("conflict")?;
            on_conflict = self.on_conflict_action()?;
        }
        let returning = self.opt_returning()?;
        Ok(Stmt::Insert {
            table,
            columns,
            source,
            on_conflict,
            returning,
        })
    }

    /// The row source after `INSERT … [(cols)]`: `VALUES (…)[, …]`, a `SELECT`,
    /// or `DEFAULT VALUES` (one all-defaults row, encoded as an empty value row).
    fn insert_source(&mut self) -> Result<InsertSource> {
        if self.is_kw("select") || self.is_kw("with") {
            return Ok(InsertSource::Select(Box::new(self.select()?)));
        }
        if self.eat_kw("default") {
            self.expect_kw("values")?;
            return Ok(InsertSource::Values(vec![Vec::new()]));
        }
        self.expect_kw("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(Tok::LParen)?;
            let mut row = Vec::new();
            if self.peek() != &Tok::RParen {
                loop {
                    row.push(self.expr()?);
                    if self.skip(Tok::Comma) {
                        continue;
                    }
                    break;
                }
            }
            self.expect(Tok::RParen)?;
            rows.push(row);
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        Ok(InsertSource::Values(rows))
    }

    /// Parse the action after `ON CONFLICT`: an optional conflict target
    /// `(col, …)` (accepted, not used — the engine keys on the primary key),
    /// then `DO NOTHING` or `DO UPDATE SET … [WHERE …]`.
    fn on_conflict_action(&mut self) -> Result<OnConflict> {
        if self.peek() == &Tok::LParen {
            let _ = self.paren_ident_list()?; // conflict target columns
        } else if self.eat_kw("on") {
            // `ON CONSTRAINT <name>` target form.
            self.expect_kw("constraint")?;
            let _ = self.ident()?;
        }
        self.expect_kw("do")?;
        if self.eat_kw("nothing") {
            return Ok(OnConflict::Nothing);
        }
        self.expect_kw("update")?;
        self.expect_kw("set")?;
        let sets = self.assignment_list()?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(OnConflict::Update { sets, filter })
    }

    /// A comma-separated `col = expr` assignment list (shared by `UPDATE SET`
    /// and `ON CONFLICT DO UPDATE SET`).
    fn assignment_list(&mut self) -> Result<Vec<(String, Expr)>> {
        let mut sets = Vec::new();
        loop {
            let col = self.ident()?;
            self.expect(Tok::Eq)?;
            let val = self.expr()?;
            sets.push((col, val));
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        Ok(sets)
    }

    /// `RETURNING <select-item-list>` after a DML statement, if present.
    fn opt_returning(&mut self) -> Result<Option<Vec<SelItem>>> {
        if !self.eat_kw("returning") {
            return Ok(None);
        }
        let mut items = Vec::new();
        loop {
            items.push(self.select_item()?);
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        Ok(Some(items))
    }

    /// A full query: optional `WITH`, a select core, a set-operation chain, and
    /// the trailing `ORDER BY`/`LIMIT`/`OFFSET` that apply to the combined result.
    fn select(&mut self) -> Result<SelectStmt> {
        let with = self.parse_with()?;
        let mut q = self.select_core()?;
        q.with = with;
        loop {
            let op = if self.eat_kw("union") {
                SetOp::Union
            } else if self.eat_kw("intersect") {
                SetOp::Intersect
            } else if self.eat_kw("except") {
                SetOp::Except
            } else {
                break;
            };
            let all = self.eat_kw("all");
            let _ = self.eat_kw("distinct"); // the default; accepted explicitly
            let part = if self.peek() == &Tok::LParen {
                self.expect(Tok::LParen)?;
                let s = self.select()?;
                self.expect(Tok::RParen)?;
                s
            } else {
                self.select_core()?
            };
            q.set_ops.push(SetOpPart {
                op,
                all,
                query: Box::new(part),
            });
        }
        q.order_by = self.parse_order_by()?;
        let (limit, offset) = self.parse_limit_offset()?;
        q.limit = limit;
        q.offset = offset;
        Ok(q)
    }

    /// `WITH [RECURSIVE] name [(cols)] AS (query), …` — the leading CTE list.
    /// `RECURSIVE` is accepted but self-referential bodies are unsupported (they
    /// fail at execution); the column-alias list is accepted and ignored.
    fn parse_with(&mut self) -> Result<Vec<CteDef>> {
        if !self.eat_kw("with") {
            return Ok(Vec::new());
        }
        let _ = self.eat_kw("recursive");
        let mut ctes = Vec::new();
        loop {
            let name = self.ident()?;
            if self.peek() == &Tok::LParen {
                let _ = self.paren_ident_list()?;
            }
            self.expect_kw("as")?;
            self.expect(Tok::LParen)?;
            let query = self.select()?;
            self.expect(Tok::RParen)?;
            ctes.push(CteDef {
                name,
                query: Box::new(query),
            });
            if !self.skip(Tok::Comma) {
                break;
            }
        }
        Ok(ctes)
    }

    /// A single select core (no `WITH`, set-op chain, or trailing ORDER/LIMIT).
    fn select_core(&mut self) -> Result<SelectStmt> {
        self.expect_kw("select")?;
        let _ = self.eat_kw("all");
        let distinct = self.eat_kw("distinct");
        let mut items = Vec::new();
        loop {
            items.push(self.select_item()?);
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        let from = if self.eat_kw("from") {
            Some(self.parse_from()?)
        } else {
            None
        };
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let group_by = self.parse_group_by()?;
        let having = if self.eat_kw("having") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(SelectStmt {
            with: Vec::new(),
            distinct,
            items,
            from,
            filter,
            group_by,
            having,
            set_ops: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })
    }

    /// Parse the `FROM` clause: a source, optionally chained with joins (or
    /// comma = `CROSS JOIN`).
    fn parse_from(&mut self) -> Result<FromClause> {
        let mut left = self.parse_from_item()?;
        loop {
            if let Some(kind) = self.parse_join_kind()? {
                let right = self.parse_from_item()?;
                let (on, using) = if kind == JoinKind::Cross {
                    (None, Vec::new())
                } else {
                    self.parse_join_condition()?
                };
                left = FromClause::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind,
                    on,
                    using,
                };
            } else if self.skip(Tok::Comma) {
                let right = self.parse_from_item()?;
                left = FromClause::Join {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind: JoinKind::Cross,
                    on: None,
                    using: Vec::new(),
                };
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// A single `FROM` source: a (possibly aliased) table, a derived table, or a
    /// parenthesized join.
    fn parse_from_item(&mut self) -> Result<FromClause> {
        if self.peek() == &Tok::LParen {
            self.bump();
            if self.is_kw("select") || self.is_kw("with") {
                let query = self.select()?;
                self.expect(Tok::RParen)?;
                let alias = self.parse_from_alias(true)?.ok_or_else(|| {
                    EngineError::sql("a derived table (subquery) in FROM requires an alias")
                })?;
                return Ok(FromClause::Derived {
                    query: Box::new(query),
                    alias,
                });
            }
            let inner = self.parse_from()?;
            self.expect(Tok::RParen)?;
            return Ok(inner);
        }
        let name = self.ident()?;
        let alias = self.parse_from_alias(false)?;
        Ok(FromClause::Table { name, alias })
    }

    /// `[INNER | LEFT|RIGHT|FULL [OUTER] | CROSS] JOIN`, or `None` if the next
    /// token does not begin a join.
    fn parse_join_kind(&mut self) -> Result<Option<JoinKind>> {
        if self.eat_kw("join") || self.is_kw("inner") && self.next_is_kw("join") {
            let _ = self.eat_kw("inner") && self.eat_kw("join");
            return Ok(Some(JoinKind::Inner));
        }
        if self.eat_kw("cross") {
            self.expect_kw("join")?;
            return Ok(Some(JoinKind::Cross));
        }
        for (kw, kind) in [
            ("left", JoinKind::Left),
            ("right", JoinKind::Right),
            ("full", JoinKind::Full),
        ] {
            if self.is_kw(kw) {
                self.bump();
                let _ = self.eat_kw("outer");
                self.expect_kw("join")?;
                return Ok(Some(kind));
            }
        }
        Ok(None)
    }

    /// The `ON predicate` or `USING (cols)` after a join.
    fn parse_join_condition(&mut self) -> Result<(Option<Box<Expr>>, Vec<String>)> {
        if self.eat_kw("on") {
            Ok((Some(Box::new(self.expr()?)), Vec::new()))
        } else if self.eat_kw("using") {
            Ok((None, self.paren_ident_list()?))
        } else {
            Err(EngineError::sql("JOIN requires an ON or USING clause"))
        }
    }

    /// An optional table alias: `[AS] ident`, stopping at a clause/join keyword.
    /// `require` is unused here but documents the derived-table call site.
    fn parse_from_alias(&mut self, _require: bool) -> Result<Option<String>> {
        if self.eat_kw("as") {
            return Ok(Some(self.ident()?));
        }
        if let Some(w) = self.peek_word() {
            if !is_from_boundary_kw(w) {
                return Ok(Some(self.ident()?));
            }
        }
        Ok(None)
    }

    /// `GROUP BY expr, ...` (empty when absent).
    fn parse_group_by(&mut self) -> Result<Vec<Expr>> {
        let mut group_by = Vec::new();
        if self.eat_kw("group") {
            self.expect_kw("by")?;
            loop {
                group_by.push(self.expr()?);
                if !self.skip(Tok::Comma) {
                    break;
                }
            }
        }
        Ok(group_by)
    }

    /// `ORDER BY expr [ASC|DESC] [NULLS FIRST|LAST], ...` (empty when absent).
    fn parse_order_by(&mut self) -> Result<Vec<OrderKey>> {
        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            loop {
                let expr = self.expr()?;
                let asc = if self.eat_kw("desc") {
                    false
                } else {
                    let _ = self.eat_kw("asc");
                    true
                };
                // Default placement preserves the engine's historical behaviour
                // (NULLs first on ASC, last on DESC); `NULLS FIRST/LAST` overrides.
                let mut nulls_first = asc;
                if self.eat_kw("nulls") {
                    if self.eat_kw("first") {
                        nulls_first = true;
                    } else {
                        self.expect_kw("last")?;
                        nulls_first = false;
                    }
                }
                order_by.push(OrderKey {
                    expr,
                    asc,
                    nulls_first,
                });
                if !self.skip(Tok::Comma) {
                    break;
                }
            }
        }
        Ok(order_by)
    }

    /// `LIMIT n` / `OFFSET m` in either order (`LIMIT ALL` means no limit).
    fn parse_limit_offset(&mut self) -> Result<(Option<Expr>, Option<Expr>)> {
        let mut limit = None;
        let mut offset = None;
        loop {
            if limit.is_none() && self.eat_kw("limit") {
                if self.eat_kw("all") {
                    continue;
                }
                limit = Some(self.limit_value("LIMIT")?);
            } else if offset.is_none() && self.eat_kw("offset") {
                offset = Some(self.limit_value("OFFSET")?);
                let _ = self.eat_kw("row") || self.eat_kw("rows");
            } else {
                break;
            }
        }
        Ok((limit, offset))
    }

    /// A LIMIT / OFFSET count: an integer literal (optionally negated) or a
    /// placeholder (PostgREST parameterizes pagination), resolved at execution.
    fn limit_value(&mut self, what: &str) -> Result<Expr> {
        if matches!(
            self.peek(),
            Tok::Param | Tok::NumParam(_) | Tok::NamedParam(_)
        ) {
            let tok = self.bump();
            let idx = self.param_index(&tok);
            return Ok(Expr::Param(idx));
        }
        let neg = self.skip(Tok::Minus);
        match self.bump() {
            Tok::Int(n) => Ok(Expr::Int(if neg { -n } else { n })),
            other => Err(EngineError::sql(format!(
                "{what} expects an integer or parameter, found {other:?}"
            ))),
        }
    }

    fn select_item(&mut self) -> Result<SelItem> {
        if self.peek() == &Tok::Star {
            self.bump();
            return Ok(SelItem::Star { qualifier: None });
        }
        // `alias.*` — all columns of one FROM source.
        if let Tok::Word(w) = self.peek().clone() {
            if matches!(self.toks.get(self.pos + 1), Some(Tok::Dot))
                && matches!(self.toks.get(self.pos + 2), Some(Tok::Star))
            {
                self.bump();
                self.bump();
                self.bump();
                return Ok(SelItem::Star { qualifier: Some(w) });
            }
        }
        // Aggregates (and trailing `::type` casts on them, e.g. `count(*)::int`)
        // parse through the ordinary expression grammar now, so this is uniform.
        let expr = self.expr()?;
        let alias = self.opt_alias()?;
        Ok(SelItem::Expr { expr, alias })
    }

    fn opt_alias(&mut self) -> Result<Option<String>> {
        if self.eat_kw("as") {
            Ok(Some(self.ident()?))
        } else if let Some(w) = self.peek_word() {
            if !is_clause_kw(w) {
                Ok(Some(self.ident()?))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn update(&mut self) -> Result<Stmt> {
        self.expect_kw("update")?;
        let table = self.ident()?;
        self.expect_kw("set")?;
        let sets = self.assignment_list()?;
        // `UPDATE … FROM <sources>` (Postgres) — extra row sources correlated to
        // the target through the WHERE clause.
        let from = if self.eat_kw("from") {
            Some(self.parse_from()?)
        } else {
            None
        };
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.opt_returning()?;
        Ok(Stmt::Update {
            table,
            sets,
            from,
            filter,
            returning,
        })
    }

    fn delete(&mut self) -> Result<Stmt> {
        self.expect_kw("delete")?;
        self.expect_kw("from")?;
        let table = self.ident()?;
        // `DELETE … USING <sources>` (Postgres) — extra row sources correlated to
        // the target through the WHERE clause.
        let using = if self.eat_kw("using") {
            Some(self.parse_from()?)
        } else {
            None
        };
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let returning = self.opt_returning()?;
        Ok(Stmt::Delete {
            table,
            using,
            filter,
            returning,
        })
    }

    // ---- expression parsing (precedence climbing) --------------------------

    fn expr(&mut self) -> Result<Expr> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr> {
        let mut left = self.and_expr()?;
        while self.eat_kw("or") {
            let right = self.and_expr()?;
            left = Expr::Binary {
                op: BinOp::Or,
                l: Box::new(left),
                r: Box::new(right),
            };
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr> {
        let mut left = self.not_expr()?;
        while self.eat_kw("and") {
            let right = self.not_expr()?;
            left = Expr::Binary {
                op: BinOp::And,
                l: Box::new(left),
                r: Box::new(right),
            };
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            let e = self.not_expr()?;
            Ok(Expr::Unary {
                op: UnOp::Not,
                e: Box::new(e),
            })
        } else {
            self.cmp_expr()
        }
    }

    fn cmp_expr(&mut self) -> Result<Expr> {
        let left = self.vecdist_expr()?;
        // IS [NOT] NULL
        if self.eat_kw("is") {
            let negated = self.eat_kw("not");
            self.expect_kw("null")?;
            return Ok(Expr::IsNull {
                e: Box::new(left),
                negated,
            });
        }
        // A `NOT` here belongs to the infix predicate that follows it
        // (`NOT LIKE`/`NOT ILIKE`/`NOT IN`/`NOT BETWEEN`), not a logical NOT.
        let negated = if self.is_kw("not") && self.next_starts_neg_predicate() {
            self.bump();
            true
        } else {
            false
        };
        if self.is_kw("like") || self.is_kw("ilike") {
            return self.like_tail(left, negated);
        }
        if self.eat_kw("in") {
            return self.in_tail(left, negated);
        }
        if self.eat_kw("between") {
            return self.between_tail(left, negated);
        }
        let op = match self.peek() {
            Tok::Eq => Some(BinOp::Eq),
            Tok::Ne => Some(BinOp::Ne),
            Tok::Lt => Some(BinOp::Lt),
            Tok::Le => Some(BinOp::Le),
            Tok::Gt => Some(BinOp::Gt),
            Tok::Ge => Some(BinOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let right = self.vecdist_expr()?;
            Ok(Expr::Binary {
                op,
                l: Box::new(left),
                r: Box::new(right),
            })
        } else {
            Ok(left)
        }
    }

    /// Whether the token after a `NOT` begins an infix predicate that takes the
    /// `NOT` (so `a NOT LIKE b` is one node, not `NOT (a LIKE b)`).
    fn next_starts_neg_predicate(&self) -> bool {
        matches!(self.toks.get(self.pos + 1),
            Some(Tok::Word(w)) if ["like", "ilike", "in", "between"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k)))
    }

    /// `[NOT] (LIKE|ILIKE) pattern [ESCAPE c]` — `left` already parsed.
    fn like_tail(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        let insensitive = self.is_kw("ilike");
        self.bump(); // LIKE | ILIKE
        let pattern = self.vecdist_expr()?;
        let escape = if self.eat_kw("escape") {
            Some(Box::new(self.vecdist_expr()?))
        } else {
            None
        };
        Ok(Expr::Like {
            e: Box::new(left),
            pattern: Box::new(pattern),
            escape,
            negated,
            insensitive,
        })
    }

    /// `[NOT] IN (expr, …)` or `[NOT] IN (SELECT …)` — `left` and `IN` consumed.
    fn in_tail(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        self.expect(Tok::LParen)?;
        if self.is_kw("select") || self.is_kw("with") {
            let query = self.select()?;
            self.expect(Tok::RParen)?;
            return Ok(Expr::InSubquery {
                e: Box::new(left),
                query: Box::new(query),
                negated,
            });
        }
        let mut list = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                list.push(self.expr()?);
                if self.skip(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(Expr::InList {
            e: Box::new(left),
            list,
            negated,
        })
    }

    /// `[NOT] BETWEEN lo AND hi` — `left` and `BETWEEN` already consumed. The
    /// bounds parse at the additive level so the trailing `AND` is the BETWEEN
    /// separator, not a logical conjunction.
    fn between_tail(&mut self, left: Expr, negated: bool) -> Result<Expr> {
        let lo = self.vecdist_expr()?;
        self.expect_kw("and")?;
        let hi = self.vecdist_expr()?;
        Ok(Expr::Between {
            e: Box::new(left),
            lo: Box::new(lo),
            hi: Box::new(hi),
            negated,
        })
    }

    /// Vector distance operators bind tighter than comparison but looser than
    /// arithmetic, so `embedding <-> ? < 0.5` parses as `(embedding <-> ?) < 0.5`
    /// and `a + 1 <-> b` as `(a + 1) <-> b`.
    fn vecdist_expr(&mut self) -> Result<Expr> {
        let mut left = self.concat_expr()?;
        loop {
            let op = match self.peek() {
                Tok::VecL2 => BinOp::VecL2,
                Tok::VecCosine => BinOp::VecCosine,
                Tok::VecIp => BinOp::VecIp,
                _ => break,
            };
            self.bump();
            let right = self.concat_expr()?;
            left = Expr::Binary {
                op,
                l: Box::new(left),
                r: Box::new(right),
            };
        }
        Ok(left)
    }

    /// String concatenation `||` and the JSON arrow accessors `->` / `->>`,
    /// between the distance and additive levels (Postgres puts these "other
    /// operators" looser than `+`/`-`, tighter than comparison). The arrows
    /// desugar to the `json_get` / `json_get_text` scalar functions.
    fn concat_expr(&mut self) -> Result<Expr> {
        let mut left = self.add_expr()?;
        loop {
            let func = match self.peek() {
                Tok::Concat => {
                    self.bump();
                    let right = self.add_expr()?;
                    left = Expr::Binary {
                        op: BinOp::Concat,
                        l: Box::new(left),
                        r: Box::new(right),
                    };
                    continue;
                }
                Tok::Arrow => "json_get",
                Tok::ArrowText => "json_get_text",
                _ => break,
            };
            self.bump();
            let right = self.add_expr()?;
            left = Expr::Func {
                name: func.to_string(),
                args: vec![left, right],
            };
        }
        Ok(left)
    }

    fn add_expr(&mut self) -> Result<Expr> {
        let mut left = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let right = self.mul_expr()?;
            left = Expr::Binary {
                op,
                l: Box::new(left),
                r: Box::new(right),
            };
        }
        Ok(left)
    }

    fn mul_expr(&mut self) -> Result<Expr> {
        let mut left = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.bump();
            let right = self.unary_expr()?;
            left = Expr::Binary {
                op,
                l: Box::new(left),
                r: Box::new(right),
            };
        }
        Ok(left)
    }

    fn unary_expr(&mut self) -> Result<Expr> {
        if self.skip(Tok::Minus) {
            let e = self.unary_expr()?;
            Ok(Expr::Unary {
                op: UnOp::Neg,
                e: Box::new(e),
            })
        } else if self.skip(Tok::Plus) {
            self.unary_expr()
        } else {
            self.cast_expr()
        }
    }

    /// Parse a primary then any trailing `::type` casts (left-associative; binds
    /// tighter than unary minus, matching Postgres so `-1::int` == `-(1::int)`).
    fn cast_expr(&mut self) -> Result<Expr> {
        let mut e = self.primary()?;
        while self.skip(Tok::DColon) {
            let target = self.parse_cast_type()?;
            e = Expr::Cast {
                e: Box::new(e),
                target,
            };
        }
        Ok(e)
    }

    /// Consume a (possibly schema-qualified, multi-word, parameterized, array)
    /// type name after `::`. Only the leading word picks the target; the rest is
    /// consumed and ignored so any Postgres type spelling parses.
    fn parse_cast_type(&mut self) -> Result<CastTarget> {
        let mut word = self.ident()?;
        if self.skip(Tok::Dot) {
            word = self.ident()?; // schema-qualified: `pg_catalog.text`
        }
        let target = CastTarget::from_word(&word);
        self.eat_type_modifiers(&word);
        if self.skip(Tok::LParen) {
            // precision/scale, e.g. `(255)` or `(10, 2)` — consume balanced
            while self.peek() != &Tok::RParen && self.peek() != &Tok::Eof {
                self.bump();
            }
            self.expect(Tok::RParen)?;
        }
        while self.skip(Tok::LBracket) {
            self.expect(Tok::RBracket)?; // array marker `[]`
        }
        Ok(target)
    }

    /// Consume the trailing words of a multi-word type name (e.g. the
    /// `precision` of `double precision`, the `with time zone` of `timestamp`).
    fn eat_type_modifiers(&mut self, word: &str) {
        match word.to_ascii_lowercase().as_str() {
            "double" => {
                self.eat_kw("precision");
            }
            "character" | "bit" => {
                self.eat_kw("varying");
            }
            "timestamp" | "time" if self.eat_kw("with") || self.eat_kw("without") => {
                self.eat_kw("time");
                self.eat_kw("zone");
            }
            _ => {}
        }
    }

    /// Parse the argument list of a call; the function name `name` has been
    /// consumed and the cursor sits on `(`. Aggregate names produce
    /// [`Expr::Aggregate`]; everything else a scalar [`Expr::Func`].
    fn func_call(&mut self, name: String) -> Result<Expr> {
        if let Some(func) = agg_func(&name) {
            return self.aggregate_call(func);
        }
        self.expect(Tok::LParen)?;
        let mut args = Vec::new();
        if self.peek() != &Tok::RParen {
            // A bare `*` argument is only meaningful to `count(*)` (an aggregate,
            // handled above); tolerate it here so a stray one fails at eval.
            if self.peek() == &Tok::Star {
                self.bump();
            } else {
                loop {
                    args.push(self.expr()?);
                    if self.skip(Tok::Comma) {
                        continue;
                    }
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        Ok(Expr::Func { name, args })
    }

    /// Parse `(* | [DISTINCT] expr [, separator])` for an aggregate; cursor sits
    /// on `(`. The trailing separator applies to `string_agg`/`group_concat`.
    fn aggregate_call(&mut self, func: AggFunc) -> Result<Expr> {
        self.expect(Tok::LParen)?;
        let distinct = self.eat_kw("distinct");
        let _ = self.eat_kw("all");
        let arg = if self.peek() == &Tok::Star {
            self.bump();
            AggArg::Star
        } else {
            AggArg::Expr(Box::new(self.expr()?))
        };
        let sep = if self.skip(Tok::Comma) {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect(Tok::RParen)?;
        Ok(Expr::Aggregate {
            func,
            arg,
            distinct,
            sep,
        })
    }

    fn next_is_lparen(&self) -> bool {
        matches!(self.toks.get(self.pos + 1), Some(Tok::LParen))
    }

    /// `CAST ( expr AS type )` — the functional cast form. Routes through the
    /// same [`parse_cast_type`] machinery the `::` operator uses.
    fn cast_call(&mut self) -> Result<Expr> {
        self.bump(); // CAST
        self.expect(Tok::LParen)?;
        let e = self.expr()?;
        self.expect_kw("as")?;
        let target = self.parse_cast_type()?;
        self.expect(Tok::RParen)?;
        Ok(Expr::Cast {
            e: Box::new(e),
            target,
        })
    }

    /// `[NOT] EXISTS ( SELECT … )`. A leading `NOT` is handled by `not_expr`.
    fn exists_expr(&mut self) -> Result<Expr> {
        self.bump(); // EXISTS
        self.expect(Tok::LParen)?;
        let query = self.select()?;
        self.expect(Tok::RParen)?;
        Ok(Expr::Exists {
            query: Box::new(query),
            negated: false,
        })
    }

    /// `EXTRACT ( field FROM expr )` — desugars to `extract('<field>', expr)` so
    /// it dispatches through the ordinary scalar-function table.
    fn extract_call(&mut self) -> Result<Expr> {
        self.bump(); // EXTRACT
        self.expect(Tok::LParen)?;
        let field = self.ident()?;
        self.expect_kw("from")?;
        let source = self.expr()?;
        self.expect(Tok::RParen)?;
        Ok(Expr::Func {
            name: "extract".to_string(),
            args: vec![Expr::Str(field.to_ascii_lowercase()), source],
        })
    }

    /// `CASE [operand] WHEN cond THEN result … [ELSE result] END`.
    fn case_expr(&mut self) -> Result<Expr> {
        self.expect_kw("case")?;
        // A simple CASE has an operand before the first WHEN.
        let operand = if self.is_kw("when") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw("when") {
            let cond = self.expr()?;
            self.expect_kw("then")?;
            let result = self.expr()?;
            whens.push((cond, result));
        }
        if whens.is_empty() {
            return Err(EngineError::sql("CASE requires at least one WHEN"));
        }
        let els = if self.eat_kw("else") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("end")?;
        Ok(Expr::Case {
            operand,
            whens,
            els,
        })
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(Expr::Int(n))
            }
            Tok::Real(r) => {
                self.bump();
                Ok(Expr::Real(r))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Str(s))
            }
            Tok::Param | Tok::NumParam(_) | Tok::NamedParam(_) => {
                let tok = self.bump();
                let idx = self.param_index(&tok);
                Ok(Expr::Param(idx))
            }
            Tok::LParen => {
                self.bump();
                // A parenthesized subquery is a scalar subquery; otherwise it is
                // a grouped expression.
                if self.is_kw("select") || self.is_kw("with") {
                    let q = self.select()?;
                    self.expect(Tok::RParen)?;
                    return Ok(Expr::ScalarSubquery(Box::new(q)));
                }
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::LBracket => self.vector_literal(),
            Tok::Word(w) => {
                if w.eq_ignore_ascii_case("null") {
                    self.bump();
                    Ok(Expr::Null)
                } else if w.eq_ignore_ascii_case("true") {
                    self.bump();
                    Ok(Expr::Int(1))
                } else if w.eq_ignore_ascii_case("false") {
                    self.bump();
                    Ok(Expr::Int(0))
                } else if w.eq_ignore_ascii_case("default") {
                    self.bump();
                    Ok(Expr::Default)
                } else if w.eq_ignore_ascii_case("case") {
                    self.case_expr()
                } else if w.eq_ignore_ascii_case("cast") && self.next_is_lparen() {
                    self.cast_call()
                } else if w.eq_ignore_ascii_case("extract") && self.next_is_lparen() {
                    self.extract_call()
                } else if w.eq_ignore_ascii_case("exists") && self.next_is_lparen() {
                    self.exists_expr()
                } else {
                    self.bump();
                    if self.peek() == &Tok::LParen {
                        self.func_call(w)
                    } else if self.skip(Tok::Dot) {
                        // qualified `name.column`. `excluded.col` (the upsert
                        // pseudo-table) is its own node; an ordinary `table.col`
                        // becomes a qualified reference the relational binder
                        // resolves (the single-table path ignores the qualifier).
                        let col = self.ident()?;
                        if w.eq_ignore_ascii_case("excluded") {
                            Ok(Expr::Excluded(col))
                        } else {
                            Ok(Expr::Qualified(w, col))
                        }
                    } else {
                        Ok(Expr::Column(w))
                    }
                }
            }
            other => Err(EngineError::sql(format!(
                "unexpected token in expression: {other:?}"
            ))),
        }
    }

    /// A `[a, b, c]` vector literal of numeric (optionally signed) components.
    fn vector_literal(&mut self) -> Result<Expr> {
        self.expect(Tok::LBracket)?;
        let mut comps = Vec::new();
        if self.peek() != &Tok::RBracket {
            loop {
                comps.push(self.number_component()?);
                if self.skip(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(Tok::RBracket)?;
        Ok(Expr::Vector(comps))
    }

    fn number_component(&mut self) -> Result<f32> {
        let neg = self.skip(Tok::Minus);
        let v = match self.bump() {
            Tok::Int(n) => n as f32,
            Tok::Real(r) => r as f32,
            other => {
                return Err(EngineError::sql(format!(
                    "vector literal expects numbers, found {other:?}"
                )))
            }
        };
        Ok(if neg { -v } else { v })
    }
}

/// Map a pgvector-style opclass keyword to its metric (e.g. `vector_cosine_ops`).
fn opclass_metric(w: &str) -> Option<Metric> {
    let w = w.to_ascii_lowercase();
    if !w.starts_with("vector_") || !w.ends_with("_ops") {
        return None;
    }
    match w.trim_start_matches("vector_").trim_end_matches("_ops") {
        "cosine" => Some(Metric::Cosine),
        "l2" => Some(Metric::L2),
        "ip" => Some(Metric::InnerProduct),
        _ => None,
    }
}

fn agg_func(w: &str) -> Option<AggFunc> {
    match () {
        _ if w.eq_ignore_ascii_case("count") => Some(AggFunc::Count),
        _ if w.eq_ignore_ascii_case("sum") => Some(AggFunc::Sum),
        _ if w.eq_ignore_ascii_case("min") => Some(AggFunc::Min),
        _ if w.eq_ignore_ascii_case("max") => Some(AggFunc::Max),
        _ if w.eq_ignore_ascii_case("avg") => Some(AggFunc::Avg),
        _ if w.eq_ignore_ascii_case("json_agg") => Some(AggFunc::JsonAgg),
        _ if w.eq_ignore_ascii_case("jsonb_agg") => Some(AggFunc::JsonAgg),
        _ if w.eq_ignore_ascii_case("group_concat") => Some(AggFunc::GroupConcat),
        _ if w.eq_ignore_ascii_case("string_agg") => Some(AggFunc::GroupConcat),
        _ => None,
    }
}

fn is_constraint_kw(w: &str) -> bool {
    [
        "primary",
        "not",
        "null",
        "unique",
        "references",
        "default",
        "check",
        "autoincrement",
        "collate",
        "generated",
    ]
    .iter()
    .any(|k| w.eq_ignore_ascii_case(k))
}

/// Whether a type name is a SERIAL-family auto-increment integer alias.
fn is_serial_type(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "serial" | "bigserial" | "smallserial" | "serial4" | "serial8" | "serial2"
    )
}

/// Re-stringify a token slice into a re-parseable SQL fragment (whitespace
/// normalized). Used to capture `DEFAULT`/`CHECK` expression source.
fn render_tokens(toks: &[Tok]) -> String {
    let mut out = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i > 0
            && !matches!(t, Tok::Comma | Tok::RParen | Tok::DColon)
            && !matches!(toks[i - 1], Tok::LParen | Tok::DColon)
        {
            out.push(' ');
        }
        out.push_str(&render_token(t));
    }
    out
}

fn render_token(t: &Tok) -> String {
    match t {
        Tok::Word(w) => w.clone(),
        Tok::Int(n) => n.to_string(),
        Tok::Real(r) => format!("{r}"),
        Tok::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Tok::Param => "?".to_string(),
        Tok::NumParam(n) => format!("${n}"),
        Tok::NamedParam(name) => format!(":{name}"),
        Tok::LParen => "(".to_string(),
        Tok::RParen => ")".to_string(),
        Tok::LBracket => "[".to_string(),
        Tok::RBracket => "]".to_string(),
        Tok::Comma => ",".to_string(),
        Tok::Semi => ";".to_string(),
        Tok::DColon => "::".to_string(),
        Tok::Dot => ".".to_string(),
        Tok::Star => "*".to_string(),
        Tok::Eq => "=".to_string(),
        Tok::Ne => "<>".to_string(),
        Tok::Lt => "<".to_string(),
        Tok::Le => "<=".to_string(),
        Tok::Gt => ">".to_string(),
        Tok::Ge => ">=".to_string(),
        Tok::Plus => "+".to_string(),
        Tok::Minus => "-".to_string(),
        Tok::Slash => "/".to_string(),
        Tok::Percent => "%".to_string(),
        Tok::Concat => "||".to_string(),
        Tok::Arrow => "->".to_string(),
        Tok::ArrowText => "->>".to_string(),
        Tok::VecL2 => "<->".to_string(),
        Tok::VecCosine => "<=>".to_string(),
        Tok::VecIp => "<#>".to_string(),
        Tok::Eof => String::new(),
    }
}

fn is_clause_kw(w: &str) -> bool {
    [
        "from",
        "where",
        "order",
        "limit",
        "offset",
        "group",
        "having",
        "as",
        "and",
        "or",
        "is",
        "like",
        "ilike",
        "asc",
        "desc",
        "nulls",
        "returning",
        "on",
        "between",
        "union",
        "intersect",
        "except",
    ]
    .iter()
    .any(|k| w.eq_ignore_ascii_case(k))
}

/// Keywords that terminate an optional `FROM`-source alias (so we don't read a
/// join/clause keyword as the alias name).
fn is_from_boundary_kw(w: &str) -> bool {
    [
        "join",
        "inner",
        "left",
        "right",
        "full",
        "cross",
        "on",
        "using",
        "where",
        "group",
        "order",
        "limit",
        "offset",
        "having",
        "union",
        "intersect",
        "except",
        "as",
    ]
    .iter()
    .any(|k| w.eq_ignore_ascii_case(k))
}
