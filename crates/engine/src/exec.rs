//! Executor: evaluates expressions and runs statements against the MVCC store.
//!
//! Reads acquire an MVCC snapshot and filter row versions by visibility; writes
//! validate fully *before* mutating so each statement is atomic (a mid-statement
//! constraint failure leaves the store untouched). Mutations stamp new versions
//! `PENDING` and emit [`WalOp`]s; durability and publish-at-commit-LSN are the
//! transaction manager's job (see [`crate::conn`]).

use crate::catalog::TableSchema;
use crate::datetime;
use crate::error::{EngineError, Result};
use crate::sql::{
    AggArg, AggFunc, BinOp, CastTarget, Expr, FromClause, InsertSource, JoinKind, OnConflict,
    OrderKey, SelItem, SelectStmt, SetOp, UnOp,
};
use crate::store::{RowVersion, Store, PENDING};
use crate::value::{parse_vector, ColumnType, Value};
use crate::vector::{distance, Metric};
use crate::wal::WalOp;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// How many candidates the HNSW scan over-fetches per requested result, to
/// absorb hits that are MVCC-invisible or filtered out by a `WHERE` clause.
const KNN_OVERFETCH: usize = 4;

/// A buffered query result. Cells render to the string-only C ABI on demand.
#[derive(Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<String>,
    /// Best-effort declared type per column (from the catalog for column
    /// references, inferred for literals/aggregates). Lets the pgwire server
    /// report accurate type OIDs even for an empty result; the embedded C ABI
    /// ignores it (it renders every cell to text). Parallel to `columns`.
    pub types: Vec<ColumnType>,
    pub rows: Vec<Vec<Value>>,
}

struct EvalCtx<'a> {
    row: Option<&'a [Value]>,
    schema: Option<&'a TableSchema>,
    params: &'a [Value],
    /// The rows of the current group, present only while projecting an
    /// aggregated query. When set, [`Expr::Aggregate`] folds over these rows;
    /// when `None`, an aggregate expression is an error.
    group: Option<&'a [&'a [Value]]>,
    /// The proposed-insert row backing `excluded.<col>` inside an
    /// `ON CONFLICT DO UPDATE`; `None` everywhere else.
    excluded: Option<&'a [Value]>,
    /// Multi-source column namespace (joins / derived tables). When set,
    /// `Column`/`Qualified` resolve against it instead of a single `schema`.
    cols: Option<&'a [RelCol]>,
}

impl<'a> EvalCtx<'a> {
    /// A context with no row/schema/group — for constant expressions and params.
    fn root(params: &'a [Value]) -> Self {
        EvalCtx {
            row: None,
            schema: None,
            params,
            group: None,
            excluded: None,
            cols: None,
        }
    }

    /// A per-row context (no group); used by scans, writes, and ORDER BY.
    fn row(row: &'a [Value], schema: &'a TableSchema, params: &'a [Value]) -> Self {
        EvalCtx {
            row: Some(row),
            schema: Some(schema),
            params,
            group: None,
            excluded: None,
            cols: None,
        }
    }

    /// A per-row context over a multi-source column namespace (relational path).
    fn rel(row: &'a [Value], cols: &'a [RelCol], params: &'a [Value]) -> Self {
        EvalCtx {
            row: Some(row),
            schema: None,
            params,
            group: None,
            excluded: None,
            cols: Some(cols),
        }
    }
}

/// A column in a relational (multi-source) namespace: its originating source
/// (table name or alias), column name, and declared type.
#[derive(Clone)]
pub(crate) struct RelCol {
    table: Option<String>,
    name: String,
    ty: ColumnType,
}

/// Resolve a (optionally qualified) column reference against a namespace.
fn resolve_col(cols: &[RelCol], table: Option<&str>, name: &str) -> Result<usize> {
    let mut found = None;
    for (i, c) in cols.iter().enumerate() {
        let name_ok = c.name.eq_ignore_ascii_case(name);
        let table_ok = match table {
            None => true,
            Some(t) => c
                .table
                .as_deref()
                .is_some_and(|ct| ct.eq_ignore_ascii_case(t)),
        };
        if name_ok && table_ok {
            if found.is_some() {
                let q = table.map(|t| format!("{t}.")).unwrap_or_default();
                return Err(EngineError::sql(format!(
                    "ambiguous column reference: {q}{name}"
                )));
            }
            found = Some(i);
        }
    }
    found.ok_or_else(|| {
        let q = table.map(|t| format!("{t}.")).unwrap_or_default();
        EngineError::sql(format!("no such column: {q}{name}"))
    })
}

// ---- expression evaluation ------------------------------------------------

fn eval(e: &Expr, ctx: &EvalCtx) -> Result<Value> {
    match e {
        Expr::Null => Ok(Value::Null),
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Real(r) => Ok(Value::Real(*r)),
        Expr::Str(s) => Ok(Value::Text(s.clone())),
        Expr::Vector(v) => Ok(Value::Vector(v.clone())),
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Param(idx) => ctx
            .params
            .get(idx - 1)
            .cloned()
            .ok_or_else(|| EngineError::misuse(format!("missing bound parameter ?{idx}"))),
        Expr::Column(name) => resolve_value(ctx, None, name),
        // A qualified `table.col`. In the relational path the qualifier selects
        // the source; the single-table path ignores it (resolves by name).
        Expr::Qualified(table, name) => resolve_value(ctx, Some(table), name),
        Expr::Unary { op, e } => eval_unary(op, e, ctx),
        Expr::IsNull { e, negated } => {
            let v = eval(e, ctx)?;
            let is_null = v.is_null();
            Ok(Value::Int((is_null ^ negated) as i64))
        }
        Expr::Like {
            e,
            pattern,
            escape,
            negated,
            insensitive,
        } => eval_like(e, pattern, escape, *negated, *insensitive, ctx),
        Expr::InList { e, list, negated } => eval_in_list(e, list, *negated, ctx),
        Expr::Between { e, lo, hi, negated } => eval_between(e, lo, hi, *negated, ctx),
        Expr::Case {
            operand,
            whens,
            els,
        } => eval_case(operand, whens, els, ctx),
        Expr::Default => Err(EngineError::sql("DEFAULT is only valid as an INSERT value")),
        Expr::Excluded(name) => {
            let row = ctx.excluded.ok_or_else(|| {
                EngineError::sql("excluded.* is only valid in ON CONFLICT DO UPDATE")
            })?;
            let schema = ctx
                .schema
                .ok_or_else(|| EngineError::sql("excluded.* referenced with no table"))?;
            let idx = schema
                .column_index(name)
                .ok_or_else(|| EngineError::sql(format!("no such column: excluded.{name}")))?;
            Ok(row[idx].clone())
        }
        Expr::Binary { op, l, r } => {
            let lv = eval(l, ctx)?;
            let rv = eval(r, ctx)?;
            eval_binary(*op, lv, rv)
        }
        Expr::Cast { e, target } => {
            let v = eval(e, ctx)?;
            cast_value(v, *target)
        }
        Expr::Func { name, args } => {
            let vals: Vec<Value> = args.iter().map(|a| eval(a, ctx)).collect::<Result<_>>()?;
            call_func(name, vals)
        }
        Expr::Aggregate {
            func,
            arg,
            distinct,
            sep,
        } => {
            let group = ctx.group.ok_or_else(|| {
                EngineError::sql("aggregate function not allowed in this context")
            })?;
            let sep = match sep {
                Some(e) => eval(e, ctx)?.render(),
                None => None,
            };
            // Fold each group row in the same scope (schema or namespace) as ctx.
            let eval_row = |e: &Expr, r: &[Value]| {
                let rc = EvalCtx {
                    row: Some(r),
                    schema: ctx.schema,
                    params: ctx.params,
                    group: None,
                    excluded: None,
                    cols: ctx.cols,
                };
                eval(e, &rc)
            };
            compute_aggregate(*func, arg, *distinct, sep.as_deref(), group, &eval_row)
        }
        // Subqueries are pre-resolved (replaced with literals) before evaluation
        // in the relational path; reaching here means a subquery in a context
        // that does not support it (e.g. a DML predicate).
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => Err(
            EngineError::sql("subqueries are only supported in SELECT statements"),
        ),
    }
}

/// Evaluate a unary operator (`NOT`, unary minus) with NULL propagation.
fn eval_unary(op: &UnOp, e: &Expr, ctx: &EvalCtx) -> Result<Value> {
    let v = eval(e, ctx)?;
    match op {
        UnOp::Not => Ok(match v.as_bool() {
            None => Value::Null,
            Some(b) => Value::Int(!b as i64),
        }),
        UnOp::Neg => match v {
            Value::Null => Ok(Value::Null),
            Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
            Value::Real(r) => Ok(Value::Real(-r)),
            other => Err(EngineError::sql(format!(
                "cannot negate {}",
                other.type_name()
            ))),
        },
    }
}

/// Evaluate `[NOT] LIKE` / `ILIKE` with an optional single-char `ESCAPE`; any
/// NULL operand (value, pattern, or escape) yields NULL.
fn eval_like(
    e: &Expr,
    pattern: &Expr,
    escape: &Option<Box<Expr>>,
    negated: bool,
    insensitive: bool,
    ctx: &EvalCtx,
) -> Result<Value> {
    let v = eval(e, ctx)?;
    let p = eval(pattern, ctx)?;
    let esc = match escape {
        Some(x) => match eval(x, ctx)? {
            Value::Null => return Ok(Value::Null),
            other => other.render().and_then(|s| s.chars().next()),
        },
        None => None,
    };
    match (v, p) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(s), Value::Text(pat)) => {
            let m = like_match(&s, &pat, esc, insensitive);
            Ok(Value::Int((m ^ negated) as i64))
        }
        _ => Ok(Value::Null),
    }
}

/// Resolve a (optionally qualified) column reference to its value, in either the
/// relational namespace (`cols`) or the single-table (`schema`) scope.
fn resolve_value(ctx: &EvalCtx, table: Option<&str>, name: &str) -> Result<Value> {
    let row = ctx
        .row
        .ok_or_else(|| EngineError::sql(format!("column {name} referenced with no row")))?;
    if let Some(cols) = ctx.cols {
        let idx = resolve_col(cols, table, name)?;
        return Ok(row[idx].clone());
    }
    let schema = ctx
        .schema
        .ok_or_else(|| EngineError::sql(format!("column {name} referenced with no table")))?;
    let idx = schema
        .column_index(name)
        .ok_or_else(|| EngineError::sql(format!("no such column: {name}")))?;
    Ok(row[idx].clone())
}

/// Coerce a value to a `::type` cast target. NULL casts to NULL; unmodelled
/// targets pass the value through unchanged. Bad numeric text errors, matching
/// Postgres `invalid input syntax`.
fn cast_value(v: Value, target: CastTarget) -> Result<Value> {
    if v.is_null() || target == CastTarget::Passthrough {
        return Ok(v);
    }
    match target {
        CastTarget::Int => match v {
            Value::Int(i) => Ok(Value::Int(i)),
            Value::Real(r) => Ok(Value::Int(r.round() as i64)),
            Value::Text(s) => parse_int_text(&s),
            other => Err(cast_err(&other, "integer")),
        },
        CastTarget::Real => match v {
            Value::Int(i) => Ok(Value::Real(i as f64)),
            Value::Real(r) => Ok(Value::Real(r)),
            Value::Text(s) => s.trim().parse::<f64>().map(Value::Real).map_err(|_| {
                EngineError::sql(format!("invalid input syntax for type real: {s:?}"))
            }),
            other => Err(cast_err(&other, "real")),
        },
        CastTarget::Text => Ok(match v.render() {
            Some(s) => Value::Text(s),
            None => Value::Null,
        }),
        CastTarget::Bool => match v.as_bool() {
            Some(b) => Ok(Value::Int(b as i64)),
            None => Ok(Value::Null),
        },
        CastTarget::Passthrough => Ok(v),
    }
}

/// Parse integer text the way Postgres `::int` does: accept an integer literal,
/// or a real literal that it rounds.
fn parse_int_text(s: &str) -> Result<Value> {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::Int(i));
    }
    if let Ok(r) = t.parse::<f64>() {
        return Ok(Value::Int(r.round() as i64));
    }
    Err(EngineError::sql(format!(
        "invalid input syntax for type integer: {s:?}"
    )))
}

fn cast_err(v: &Value, ty: &str) -> EngineError {
    EngineError::sql(format!("cannot cast {} to {ty}", v.type_name()))
}

fn eval_binary(op: BinOp, l: Value, r: Value) -> Result<Value> {
    use BinOp::*;
    match op {
        Eq | Ne | Lt | Le | Gt | Ge => {
            let cmp = l.sql_cmp(&r);
            let res = match op {
                Eq => l.sql_eq(&r),
                Ne => l.sql_eq(&r).map(|b| !b),
                Lt => cmp.map(|o| o == Ordering::Less),
                Le => cmp.map(|o| o != Ordering::Greater),
                Gt => cmp.map(|o| o == Ordering::Greater),
                Ge => cmp.map(|o| o != Ordering::Less),
                _ => unreachable!(),
            };
            Ok(match res {
                None => Value::Null,
                Some(b) => Value::Int(b as i64),
            })
        }
        And => Ok(three_valued_and(l.as_bool(), r.as_bool())),
        Or => Ok(three_valued_or(l.as_bool(), r.as_bool())),
        Add | Sub | Mul | Div | Mod => arith(op, l, r),
        Concat => Ok(concat_values(l, r)),
        VecL2 | VecCosine | VecIp => vec_distance(op, &l, &r),
    }
}

/// `||` string concatenation: NULL-propagating, operands rendered to text.
fn concat_values(l: Value, r: Value) -> Value {
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    Value::Text(format!(
        "{}{}",
        l.render().unwrap_or_default(),
        r.render().unwrap_or_default()
    ))
}

/// `expr [NOT] IN (list)` with SQL three-valued logic: NULL if `expr` is NULL or
/// no element matched but some element compared NULL; otherwise the membership.
fn eval_in_list(e: &Expr, list: &[Expr], negated: bool, ctx: &EvalCtx) -> Result<Value> {
    let v = eval(e, ctx)?;
    if v.is_null() {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for item in list {
        match v.sql_eq(&eval(item, ctx)?) {
            Some(true) => return Ok(Value::Int(!negated as i64)),
            Some(false) => {}
            None => saw_null = true,
        }
    }
    Ok(if saw_null {
        Value::Null
    } else {
        Value::Int(negated as i64)
    })
}

/// `expr [NOT] BETWEEN lo AND hi`, i.e. `expr >= lo AND expr <= hi` (negated by
/// De Morgan), preserving three-valued logic via [`eval_binary`].
fn eval_between(e: &Expr, lo: &Expr, hi: &Expr, negated: bool, ctx: &EvalCtx) -> Result<Value> {
    let v = eval(e, ctx)?;
    let lo = eval(lo, ctx)?;
    let hi = eval(hi, ctx)?;
    let ge = eval_binary(BinOp::Ge, v.clone(), lo)?;
    let le = eval_binary(BinOp::Le, v, hi)?;
    let within = three_valued_and(ge.as_bool(), le.as_bool());
    if !negated {
        return Ok(within);
    }
    Ok(match within.as_bool() {
        Some(b) => Value::Int(!b as i64),
        None => Value::Null,
    })
}

/// Evaluate a `CASE` expression, short-circuiting at the first matching branch.
/// A simple `CASE operand WHEN v …` compares `operand` to each `v` for equality;
/// a searched `CASE WHEN cond …` tests each `cond` for truth.
fn eval_case(
    operand: &Option<Box<Expr>>,
    whens: &[(Expr, Expr)],
    els: &Option<Box<Expr>>,
    ctx: &EvalCtx,
) -> Result<Value> {
    let operand = match operand {
        Some(e) => Some(eval(e, ctx)?),
        None => None,
    };
    for (cond, result) in whens {
        let matched = match &operand {
            // Simple form: operand = when-value (NULL never matches).
            Some(op) => eval(cond, ctx)?.sql_eq(op) == Some(true),
            // Searched form: when-condition is truthy.
            None => eval(cond, ctx)?.as_bool().unwrap_or(false),
        };
        if matched {
            return eval(result, ctx);
        }
    }
    match els {
        Some(e) => eval(e, ctx),
        None => Ok(Value::Null),
    }
}

/// Evaluate a vector distance operator to a REAL distance. NULL operands yield
/// NULL; a `'[1,2,3]'` text operand is accepted (parsed) for ergonomics, so the
/// same query works whether the query vector arrives as a literal, a `?`
/// parameter, or a string.
fn vec_distance(op: BinOp, l: &Value, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let metric = op.vec_metric().expect("vector distance op");
    let (Some(a), Some(b)) = (to_vec_operand(l), to_vec_operand(r)) else {
        return Err(EngineError::sql(
            "vector distance operators require vector operands",
        ));
    };
    if a.len() != b.len() {
        return Err(EngineError::sql(format!(
            "vector dimension mismatch: {} vs {}",
            a.len(),
            b.len()
        )));
    }
    Ok(Value::Real(distance(metric, &a, &b) as f64))
}

/// Coerce a value usable as a vector operand: a real vector, or a `'[...]'`
/// text literal that parses as one.
fn to_vec_operand(v: &Value) -> Option<Vec<f32>> {
    match v {
        Value::Vector(x) => Some(x.clone()),
        Value::Text(s) => parse_vector(s),
        _ => None,
    }
}

fn three_valued_and(a: Option<bool>, b: Option<bool>) -> Value {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Value::Int(0),
        (Some(true), Some(true)) => Value::Int(1),
        _ => Value::Null,
    }
}

fn three_valued_or(a: Option<bool>, b: Option<bool>) -> Value {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Value::Int(1),
        (Some(false), Some(false)) => Value::Int(0),
        _ => Value::Null,
    }
}

fn arith(op: BinOp, l: Value, r: Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let both_int = matches!(l, Value::Int(_)) && matches!(r, Value::Int(_));
    let ln = num(&l)?;
    let rn = num(&r)?;
    Ok(match op {
        BinOp::Add if both_int => int_or_real(li(&l).checked_add(li(&r)), ln + rn),
        BinOp::Sub if both_int => int_or_real(li(&l).checked_sub(li(&r)), ln - rn),
        BinOp::Mul if both_int => int_or_real(li(&l).checked_mul(li(&r)), ln * rn),
        BinOp::Add => Value::Real(ln + rn),
        BinOp::Sub => Value::Real(ln - rn),
        BinOp::Mul => Value::Real(ln * rn),
        BinOp::Div => {
            if both_int {
                if li(&r) == 0 {
                    Value::Null
                } else {
                    Value::Int(li(&l) / li(&r))
                }
            } else if rn == 0.0 {
                Value::Null
            } else {
                Value::Real(ln / rn)
            }
        }
        BinOp::Mod => {
            if both_int {
                if li(&r) == 0 {
                    Value::Null
                } else {
                    Value::Int(li(&l) % li(&r))
                }
            } else if rn == 0.0 {
                Value::Null
            } else {
                Value::Real(ln % rn)
            }
        }
        _ => unreachable!(),
    })
}

fn int_or_real(checked: Option<i64>, real: f64) -> Value {
    match checked {
        Some(i) => Value::Int(i),
        None => Value::Real(real),
    }
}

fn li(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => 0,
    }
}

fn num(v: &Value) -> Result<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Real(r) => Ok(*r),
        Value::Text(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| EngineError::sql(format!("cannot use text '{s}' in arithmetic"))),
        other => Err(EngineError::sql(format!(
            "cannot use {} in arithmetic",
            other.type_name()
        ))),
    }
}

/// One token of a compiled `LIKE` pattern: a wildcard run, a single-char
/// wildcard, or a literal character.
enum LikeTok {
    Any,
    One,
    Lit(char),
}

/// SQL `LIKE`/`ILIKE` with `%` (any run) and `_` (single char). `escape`, when
/// set, makes the following pattern char a literal (so `\%` matches a real `%`).
/// `insensitive` (or the engine's default `LIKE`) folds ASCII case before
/// matching. The pattern is pre-tokenized into "literal char" vs "wildcard".
fn like_match(s: &str, pattern: &str, escape: Option<char>, insensitive: bool) -> bool {
    // `LIKE` is case-sensitive; `ILIKE` folds ASCII case (stage 6E dialect split).
    let fold = |c: char| {
        if insensitive {
            c.to_ascii_lowercase()
        } else {
            c
        }
    };
    let s: Vec<char> = s.chars().map(fold).collect();

    let mut p: Vec<LikeTok> = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            let lit = chars.next().unwrap_or(c);
            p.push(LikeTok::Lit(fold(lit)));
        } else if c == '%' {
            p.push(LikeTok::Any);
        } else if c == '_' {
            p.push(LikeTok::One);
        } else {
            p.push(LikeTok::Lit(fold(c)));
        }
    }
    like_go(&s, &p)
}

/// Recursive `LIKE` matcher over a compiled pattern (module-level so it nests
/// cleanly; `%` collapses consecutive runs then tries every split).
fn like_go(s: &[char], p: &[LikeTok]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match &p[0] {
        LikeTok::Any => {
            let mut k = 0;
            while k < p.len() && matches!(p[k], LikeTok::Any) {
                k += 1;
            }
            let rest = &p[k..];
            rest.is_empty() || (0..=s.len()).any(|i| like_go(&s[i..], rest))
        }
        LikeTok::One => !s.is_empty() && like_go(&s[1..], &p[1..]),
        LikeTok::Lit(c) => !s.is_empty() && s[0] == *c && like_go(&s[1..], &p[1..]),
    }
}

fn predicate(filter: &Option<Expr>, ctx: &EvalCtx) -> Result<bool> {
    match filter {
        None => Ok(true),
        Some(e) => Ok(eval(e, ctx)?.as_bool().unwrap_or(false)),
    }
}

// ---- SELECT ---------------------------------------------------------------

/// MVCC visibility for a row version given the caller's role: a writer
/// (`Some(owner)`) sees committed rows plus its own pending changes; a plain
/// reader (`None`) sees only committed rows at-or-before its snapshot.
fn row_visible(v: &RowVersion, snapshot: u64, writer: Option<u64>) -> bool {
    match writer {
        Some(me) => v.visible_to_writer(snapshot, me),
        None => v.visible_to_reader(snapshot),
    }
}

pub fn run_select(
    store: &Store,
    sel: &SelectStmt,
    snapshot: u64,
    writer: Option<u64>,
    params: &[Value],
) -> Result<ResultSet> {
    // The single-source fast path (and its KNN optimization) handles the common
    // shape directly; multi-source / set-op / DISTINCT / CTE / subquery queries
    // route to the relational executor.
    match &sel.from {
        Some(FromClause::Table { name, .. }) if !needs_relational(sel) => {
            single_table_select(store, sel, name, snapshot, writer, params)
        }
        None if !needs_relational(sel) => constant_select(sel, params),
        _ => relational::run_query(store, sel, snapshot, writer, params),
    }
}

/// Whether a query needs the relational (multi-source) executor rather than the
/// single-table fast path: a `WITH`/`DISTINCT`/set-op, a join or derived table,
/// or any subquery expression (which must be pre-resolved).
fn needs_relational(sel: &SelectStmt) -> bool {
    !sel.with.is_empty()
        || sel.distinct
        || !sel.set_ops.is_empty()
        || matches!(
            sel.from,
            Some(FromClause::Join { .. }) | Some(FromClause::Derived { .. })
        )
        || select_has_subquery(sel)
}

/// Whether any clause of `sel` contains a subquery expression.
fn select_has_subquery(sel: &SelectStmt) -> bool {
    let item_has = sel.items.iter().any(|i| match i {
        SelItem::Expr { expr, .. } => expr_has_subquery(expr),
        SelItem::Star { .. } => false,
    });
    item_has
        || sel.filter.as_ref().is_some_and(expr_has_subquery)
        || sel.group_by.iter().any(expr_has_subquery)
        || sel.having.as_ref().is_some_and(expr_has_subquery)
        || sel.order_by.iter().any(|k| expr_has_subquery(&k.expr))
}

/// The single-table fast path (preserves the original scan / KNN / group logic).
fn single_table_select(
    store: &Store,
    sel: &SelectStmt,
    table_name: &str,
    snapshot: u64,
    writer: Option<u64>,
    params: &[Value],
) -> Result<ResultSet> {
    let table = store
        .table(table_name)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table_name}")))?;
    let schema = &table.schema;

    // Index-accelerated top-k nearest-neighbour, when the query shape allows it
    // (an HNSW index exists for the ordered-by distance) — answered by the access
    // method, not a full scan plus sort (spec 12).
    if let Some(rs) = knn_select(store, sel, table_name, snapshot, writer, params)? {
        return Ok(rs);
    }

    // Visible rows passing WHERE.
    let mut matched: Vec<&Vec<Value>> = Vec::new();
    for v in &table.rows {
        if !row_visible(v, snapshot, writer) {
            continue;
        }
        let ctx = EvalCtx::row(&v.values, schema, params);
        if predicate(&sel.filter, &ctx)? {
            matched.push(&v.values);
        }
    }

    if is_aggregated(sel) {
        return grouped_select(sel, schema, &matched, params);
    }

    order_matched(sel, schema, &mut matched, params)?;
    let limit = eval_count(&sel.limit, params)?;
    let offset = eval_count(&sel.offset, params)?;
    apply_offset_limit(&mut matched, limit, offset);
    project(sel, schema, &matched, params)
}

/// Whether an expression tree contains a subquery node anywhere.
fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => true,
        Expr::Cast { e, .. } | Expr::Unary { e, .. } | Expr::IsNull { e, .. } => {
            expr_has_subquery(e)
        }
        Expr::Binary { l, r, .. } => expr_has_subquery(l) || expr_has_subquery(r),
        Expr::Like {
            e, pattern, escape, ..
        } => {
            expr_has_subquery(e)
                || expr_has_subquery(pattern)
                || escape.as_deref().is_some_and(expr_has_subquery)
        }
        Expr::InList { e, list, .. } => expr_has_subquery(e) || list.iter().any(expr_has_subquery),
        Expr::Between { e, lo, hi, .. } => {
            expr_has_subquery(e) || expr_has_subquery(lo) || expr_has_subquery(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(expr_has_subquery)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_subquery(c) || expr_has_subquery(r))
                || els.as_deref().is_some_and(expr_has_subquery)
        }
        Expr::Func { args, .. } => args.iter().any(expr_has_subquery),
        Expr::Aggregate { arg, sep, .. } => {
            matches!(arg, AggArg::Expr(e) if expr_has_subquery(e))
                || sep.as_deref().is_some_and(expr_has_subquery)
        }
        _ => false,
    }
}

/// Best-effort column type of a pre-computed literal value.
fn lit_type(v: &Value) -> ColumnType {
    match v {
        Value::Int(_) => ColumnType::Integer,
        Value::Real(_) => ColumnType::Real,
        Value::Blob(_) => ColumnType::Blob,
        Value::Vector(x) => ColumnType::Vector(x.len() as u32),
        _ => ColumnType::Text,
    }
}

/// A query is aggregated if it has a `GROUP BY` / `HAVING`, or any projected
/// expression folds an aggregate.
fn is_aggregated(sel: &SelectStmt) -> bool {
    if !sel.group_by.is_empty() || sel.having.is_some() {
        return true;
    }
    sel.items.iter().any(|i| match i {
        SelItem::Expr { expr, .. } => expr_has_aggregate(expr),
        SelItem::Star { .. } => false,
    })
}

/// Whether an expression tree contains an aggregate call anywhere.
fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Aggregate { .. } => true,
        Expr::Cast { e, .. } | Expr::Unary { e, .. } | Expr::IsNull { e, .. } => {
            expr_has_aggregate(e)
        }
        Expr::Binary { l, r, .. } => expr_has_aggregate(l) || expr_has_aggregate(r),
        Expr::Like { e, pattern, .. } => expr_has_aggregate(e) || expr_has_aggregate(pattern),
        Expr::Func { args, .. } => args.iter().any(expr_has_aggregate),
        Expr::InList { e, list, .. } => {
            expr_has_aggregate(e) || list.iter().any(expr_has_aggregate)
        }
        Expr::Between { e, lo, hi, .. } => {
            expr_has_aggregate(e) || expr_has_aggregate(lo) || expr_has_aggregate(hi)
        }
        Expr::Case {
            operand,
            whens,
            els,
        } => {
            operand.as_deref().is_some_and(expr_has_aggregate)
                || whens
                    .iter()
                    .any(|(c, r)| expr_has_aggregate(c) || expr_has_aggregate(r))
                || els.as_deref().is_some_and(expr_has_aggregate)
        }
        _ => false,
    }
}

/// Evaluate a LIMIT / OFFSET count expression (an integer literal or a `?`
/// parameter) to a number; `None` means the clause was absent or NULL.
fn eval_count(opt: &Option<Expr>, params: &[Value]) -> Result<Option<i64>> {
    let Some(e) = opt else { return Ok(None) };
    let ctx = EvalCtx::root(params);
    match eval(e, &ctx)? {
        Value::Null => Ok(None),
        Value::Int(n) => Ok(Some(n)),
        Value::Real(r) => Ok(Some(r as i64)),
        Value::Text(s) => s
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| EngineError::sql("LIMIT/OFFSET must be an integer")),
        other => Err(EngineError::sql(format!(
            "LIMIT/OFFSET must be an integer, found {}",
            other.type_name()
        ))),
    }
}

/// Drop the first `offset` rows, then keep at most `limit` (both clamped at 0).
fn apply_offset_limit(matched: &mut Vec<&Vec<Value>>, limit: Option<i64>, offset: Option<i64>) {
    if let Some(off) = offset {
        let off = (off.max(0) as usize).min(matched.len());
        matched.drain(..off);
    }
    if let Some(limit) = limit {
        matched.truncate(limit.max(0) as usize);
    }
}

enum ProjItem {
    Column(usize),
    Expr(Expr),
}

/// Sort `matched` in place by the `ORDER BY` keys (brute-force, evaluated per
/// row). The first evaluation error aborts the sort and is returned.
fn order_matched(
    sel: &SelectStmt,
    schema: &TableSchema,
    matched: &mut Vec<&Vec<Value>>,
    params: &[Value],
) -> Result<()> {
    if sel.order_by.is_empty() {
        return Ok(());
    }
    let keys = resolved_order_keys(sel);
    let keys = &keys;
    let mut indexed: Vec<usize> = (0..matched.len()).collect();
    let mut err: Option<EngineError> = None;
    indexed.sort_by(|&a, &b| {
        if err.is_some() {
            return Ordering::Equal;
        }
        order_key_cmp(keys, schema, matched[a], matched[b], params, &mut err)
    });
    if let Some(e) = err {
        return Err(e);
    }
    *matched = indexed.into_iter().map(|i| matched[i]).collect();
    Ok(())
}

/// Resolve each `ORDER BY` key against the select list: a bare column that
/// matches an output alias is replaced by that item's expression (Postgres
/// resolves `ORDER BY <alias>`). Direction / NULL placement pass through.
fn resolved_order_keys(sel: &SelectStmt) -> Vec<OrderKey> {
    sel.order_by
        .iter()
        .map(|k| OrderKey {
            expr: resolve_alias(sel, &k.expr),
            asc: k.asc,
            nulls_first: k.nulls_first,
        })
        .collect()
}

fn resolve_alias(sel: &SelectStmt, expr: &Expr) -> Expr {
    if let Expr::Column(name) = expr {
        for item in &sel.items {
            if let SelItem::Expr {
                expr: e,
                alias: Some(a),
            } = item
            {
                if a.eq_ignore_ascii_case(name) {
                    return e.clone();
                }
            }
        }
    }
    expr.clone()
}

/// Compare two rows by the ordered key list, recording the first eval error.
fn order_key_cmp(
    keys: &[OrderKey],
    schema: &TableSchema,
    ra: &[Value],
    rb: &[Value],
    params: &[Value],
    err: &mut Option<EngineError>,
) -> Ordering {
    for key in keys {
        let ca = EvalCtx::row(ra, schema, params);
        let cb = EvalCtx::row(rb, schema, params);
        let (va, vb) = match (eval(&key.expr, &ca), eval(&key.expr, &cb)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => {
                *err = Some(e);
                return Ordering::Equal;
            }
        };
        let ord = order_cmp(&va, &vb, key.asc, key.nulls_first);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Compare two values for `ORDER BY`: direction applies to non-NULL values;
/// NULL placement is independent (`nulls_first`), per the SQL standard.
fn order_cmp(a: &Value, b: &Value, asc: bool, nulls_first: bool) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let ord = a.sql_cmp(b).unwrap_or(Ordering::Equal);
            if asc {
                ord
            } else {
                ord.reverse()
            }
        }
    }
}

/// Project the `matched` rows through the SELECT list, producing column names,
/// best-effort types, and rendered rows. Shared by the scan and KNN paths.
fn project(
    sel: &SelectStmt,
    schema: &TableSchema,
    matched: &[&Vec<Value>],
    params: &[Value],
) -> Result<ResultSet> {
    let mut columns = Vec::new();
    let mut types: Vec<ColumnType> = Vec::new();
    let mut plan: Vec<ProjItem> = Vec::new();
    for item in &sel.items {
        match item {
            SelItem::Star { .. } => {
                for c in &schema.columns {
                    columns.push(c.name.clone());
                    types.push(c.ty);
                    plan.push(ProjItem::Column(schema.column_index(&c.name).unwrap()));
                }
            }
            SelItem::Expr { expr, alias } => {
                columns.push(column_name(expr, alias, columns.len()));
                types.push(expr_type(expr, Some(schema)));
                plan.push(ProjItem::Expr(expr.clone()));
            }
        }
    }

    let mut rows = Vec::with_capacity(matched.len());
    for vals in matched {
        let mut out = Vec::with_capacity(plan.len());
        for p in &plan {
            match p {
                ProjItem::Column(i) => out.push(vals[*i].clone()),
                ProjItem::Expr(e) => {
                    let ctx = EvalCtx::row(vals, schema, params);
                    out.push(eval(e, &ctx)?);
                }
            }
        }
        rows.push(out);
    }
    Ok(ResultSet {
        columns,
        types,
        rows,
    })
}

/// The recognized shape of an index-answerable nearest-neighbour query:
/// `ORDER BY <col> <dist-op> <query> ASC LIMIT k`, no aggregates.
struct KnnPlan<'a> {
    column: String,
    metric: Metric,
    query: &'a Expr,
    limit: usize,
}

/// Recognize a top-k nearest-neighbour query, independent of whether an index
/// exists (the executor checks that next). `limit` is the already-evaluated
/// LIMIT count (KNN requires a non-negative LIMIT).
fn knn_plan(sel: &SelectStmt, limit: Option<i64>) -> Option<KnnPlan<'_>> {
    let limit = limit?;
    if limit < 0 || sel.order_by.len() != 1 {
        return None;
    }
    let key = &sel.order_by[0];
    if !key.asc || sel.offset.is_some() || is_aggregated(sel) {
        return None;
    }
    let Expr::Binary { op, l, r } = &key.expr else {
        return None;
    };
    let metric = op.vec_metric()?;
    let Expr::Column(col) = l.as_ref() else {
        return None;
    };
    Some(KnnPlan {
        column: col.clone(),
        metric,
        query: r.as_ref(),
        limit: limit as usize,
    })
}

/// Answer a top-k nearest-neighbour `SELECT` via the HNSW index, or `None` if the
/// query is not of that shape or no matching index exists (caller falls back to a
/// brute-force scan + sort, which the distance operator already supports).
fn knn_select(
    store: &Store,
    sel: &SelectStmt,
    table_name: &str,
    snapshot: u64,
    writer: Option<u64>,
    params: &[Value],
) -> Result<Option<ResultSet>> {
    let limit = eval_count(&sel.limit, params)?;
    let Some(plan) = knn_plan(sel, limit) else {
        return Ok(None);
    };
    if plan.limit == 0 {
        return Ok(None);
    }
    let Some(index) = store.index_for(table_name, &plan.column) else {
        return Ok(None);
    };
    if index.metric() != plan.metric || index.is_empty() {
        return Ok(None);
    }
    // Evaluate the (constant) query vector; bail to the scan path if it is not one.
    let cctx = EvalCtx::root(params);
    let Some(query) = to_vec_operand(&eval(plan.query, &cctx)?) else {
        return Ok(None);
    };

    let Some(table) = store.table(table_name) else {
        return Ok(None);
    };
    let schema = &table.schema;
    // Over-fetch to absorb MVCC-invisible / filtered hits, but never beyond the
    // number of live vectors in the index.
    let fetch = plan
        .limit
        .saturating_mul(KNN_OVERFETCH)
        .max(index.def.params.ef_search)
        .min(index.len());

    let mut matched: Vec<&Vec<Value>> = Vec::new();
    for (vid, _dist) in index.search(&query, fetch) {
        let Some(v) = table.version(vid) else {
            continue;
        };
        if !row_visible(v, snapshot, writer) {
            continue;
        }
        let ctx = EvalCtx::row(&v.values, schema, params);
        if predicate(&sel.filter, &ctx)? {
            matched.push(&v.values);
            if matched.len() >= plan.limit {
                break;
            }
        }
    }
    Ok(Some(project(sel, schema, &matched, params)?))
}

/// Best-effort static type of a projected expression. A bare column resolves to
/// its catalog type; literals resolve to their storage class; anything else
/// defaults to `Text` (the pgwire server reports this as the column OID).
fn expr_type(expr: &Expr, schema: Option<&TableSchema>) -> ColumnType {
    match expr {
        Expr::Int(_) => ColumnType::Integer,
        Expr::Real(_) => ColumnType::Real,
        Expr::Str(_) => ColumnType::Text,
        Expr::Vector(v) => ColumnType::Vector(v.len() as u32),
        Expr::Lit(v) => lit_type(v),
        // A distance operator yields a REAL distance.
        Expr::Binary { op, .. } if op.vec_metric().is_some() => ColumnType::Real,
        Expr::Column(name) => schema
            .and_then(|s| s.column_index(name).map(|i| s.columns[i].ty))
            .unwrap_or(ColumnType::Text),
        // A cast reports its target type (a passthrough keeps the inner type).
        Expr::Cast { e, target } => match target {
            CastTarget::Passthrough => expr_type(e, schema),
            other => cast_column_type(*other),
        },
        Expr::Aggregate { func, arg, .. } => match schema {
            Some(s) => agg_type(*func, arg, s),
            None => ColumnType::Text,
        },
        Expr::Func { name, .. } => func_type(name),
        Expr::Qualified(_, name) => schema
            .and_then(|s| s.column_index(name).map(|i| s.columns[i].ty))
            .unwrap_or(ColumnType::Text),
        _ => ColumnType::Text,
    }
}

/// Best-effort result type of a scalar function (for column-OID reporting).
fn func_type(name: &str) -> ColumnType {
    match name.to_ascii_lowercase().as_str() {
        "length" | "char_length" | "character_length" | "abs" | "instr" | "strpos" | "sign"
        | "extract" | "unixepoch" => ColumnType::Integer,
        "sqrt" | "exp" | "ln" | "log" | "log10" | "power" | "pow" | "pi" | "random" | "ceil"
        | "ceiling" | "floor" | "trunc" | "round" => ColumnType::Real,
        _ => ColumnType::Text,
    }
}

/// The result-set column type a non-passthrough cast target reports.
fn cast_column_type(t: CastTarget) -> ColumnType {
    match t {
        CastTarget::Int | CastTarget::Bool => ColumnType::Integer,
        CastTarget::Real => ColumnType::Real,
        CastTarget::Text | CastTarget::Passthrough => ColumnType::Text,
    }
}

/// Best-effort static type of an aggregate result.
fn agg_type(func: AggFunc, arg: &AggArg, schema: &TableSchema) -> ColumnType {
    match func {
        AggFunc::Count => ColumnType::Integer,
        AggFunc::Avg => ColumnType::Real,
        AggFunc::JsonAgg | AggFunc::GroupConcat => ColumnType::Text,
        AggFunc::Sum => match arg {
            AggArg::Expr(e) if matches!(expr_type(e, Some(schema)), ColumnType::Real) => {
                ColumnType::Real
            }
            _ => ColumnType::Integer,
        },
        AggFunc::Min | AggFunc::Max => match arg {
            AggArg::Expr(e) => expr_type(e, Some(schema)),
            AggArg::Star => ColumnType::Text,
        },
    }
}

fn constant_select(sel: &SelectStmt, params: &[Value]) -> Result<ResultSet> {
    for item in &sel.items {
        match item {
            SelItem::Star { .. } => {
                return Err(EngineError::sql("SELECT * requires a FROM clause"))
            }
            SelItem::Expr { expr, .. } if expr_has_aggregate(expr) => {
                return Err(EngineError::sql("aggregate requires a FROM clause"))
            }
            SelItem::Expr { .. } => {}
        }
    }
    let ctx = EvalCtx::root(params);
    let mut columns = Vec::new();
    let mut types = Vec::new();
    let mut row = Vec::new();
    for item in &sel.items {
        if let SelItem::Expr { expr, alias } = item {
            columns.push(column_name(expr, alias, columns.len()));
            types.push(expr_type(expr, None));
            row.push(eval(expr, &ctx)?);
        }
    }
    Ok(ResultSet {
        columns,
        types,
        rows: vec![row],
    })
}

/// Execute an aggregated `SELECT`: partition `matched` into groups by the
/// `GROUP BY` keys (one group of everything when there is none), evaluate each
/// projected expression — which may fold aggregates or reference group-key
/// columns — once per group, filter by `HAVING`, then order / offset / limit.
fn grouped_select(
    sel: &SelectStmt,
    schema: &TableSchema,
    matched: &[&Vec<Value>],
    params: &[Value],
) -> Result<ResultSet> {
    // Partition into groups, preserving first-seen order for determinism.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&[Value]>> =
        std::collections::HashMap::new();
    for r in matched {
        let row: &[Value] = r;
        let key = group_key(&sel.group_by, schema, row, params)?;
        match groups.get_mut(&key) {
            Some(v) => v.push(row),
            None => {
                order.push(key.clone());
                groups.insert(key, vec![row]);
            }
        }
    }
    // With no GROUP BY, an empty input still yields one (all-rows) group so a
    // bare aggregate like `count(*)` returns a single 0 row.
    if sel.group_by.is_empty() && order.is_empty() {
        order.push(String::new());
        groups.insert(String::new(), Vec::new());
    }

    // Column names + types from the projection (aggregate-aware).
    let mut columns = Vec::new();
    let mut types = Vec::new();
    for item in &sel.items {
        match item {
            SelItem::Star { .. } => {
                for c in &schema.columns {
                    columns.push(c.name.clone());
                    types.push(c.ty);
                }
            }
            SelItem::Expr { expr, alias } => {
                columns.push(column_name(expr, alias, columns.len()));
                types.push(expr_type(expr, Some(schema)));
            }
        }
    }

    // Build (representative-row, group-rows) and compute each output row.
    let mut out_rows: Vec<(Vec<Value>, Vec<&[Value]>)> = Vec::new();
    for key in &order {
        let rows = groups.remove(key).unwrap();
        let repr = rows.first().copied();
        let ctx = EvalCtx {
            row: repr,
            schema: Some(schema),
            params,
            group: Some(&rows),
            excluded: None,
            cols: None,
        };
        // HAVING filters whole groups.
        if let Some(h) = &sel.having {
            if !eval(h, &ctx)?.as_bool().unwrap_or(false) {
                continue;
            }
        }
        let mut out = Vec::new();
        for item in &sel.items {
            match item {
                SelItem::Star { .. } => {
                    let r = repr.ok_or_else(|| EngineError::sql("SELECT * over an empty group"))?;
                    out.extend(r.iter().cloned());
                }
                SelItem::Expr { expr, .. } => out.push(eval(expr, &ctx)?),
            }
        }
        out_rows.push((out, rows));
    }

    // ORDER BY (evaluated in each group's aggregate-aware context).
    order_groups(sel, schema, &mut out_rows, params)?;

    let mut rows: Vec<Vec<Value>> = out_rows.into_iter().map(|(o, _)| o).collect();
    // OFFSET / LIMIT over the produced group rows.
    if let Some(off) = eval_count(&sel.offset, params)? {
        let off = (off.max(0) as usize).min(rows.len());
        rows.drain(..off);
    }
    if let Some(limit) = eval_count(&sel.limit, params)? {
        rows.truncate(limit.max(0) as usize);
    }
    Ok(ResultSet {
        columns,
        types,
        rows,
    })
}

/// A stable key for the `GROUP BY` tuple of one row.
fn group_key(
    group_by: &[Expr],
    schema: &TableSchema,
    row: &[Value],
    params: &[Value],
) -> Result<String> {
    if group_by.is_empty() {
        return Ok(String::new());
    }
    let ctx = EvalCtx::row(row, schema, params);
    let mut key = String::new();
    for e in group_by {
        key.push_str(&value_key(&eval(e, &ctx)?));
        key.push('\u{1}');
    }
    Ok(key)
}

/// Sort the produced group rows by `ORDER BY`, evaluating each key in the
/// group's aggregate-aware context (so `ORDER BY count(*) DESC` works).
fn order_groups(
    sel: &SelectStmt,
    schema: &TableSchema,
    out_rows: &mut [(Vec<Value>, Vec<&[Value]>)],
    params: &[Value],
) -> Result<()> {
    if sel.order_by.is_empty() {
        return Ok(());
    }
    let keys = resolved_order_keys(sel);
    let mut err: Option<EngineError> = None;
    out_rows.sort_by(|a, b| {
        if err.is_some() {
            return Ordering::Equal;
        }
        for key in &keys {
            let ca = EvalCtx {
                row: a.1.first().copied(),
                schema: Some(schema),
                params,
                group: Some(&a.1),
                excluded: None,
                cols: None,
            };
            let cb = EvalCtx {
                row: b.1.first().copied(),
                schema: Some(schema),
                params,
                group: Some(&b.1),
                excluded: None,
                cols: None,
            };
            let (va, vb) = match (eval(&key.expr, &ca), eval(&key.expr, &cb)) {
                (Ok(x), Ok(y)) => (x, y),
                (Err(e), _) | (_, Err(e)) => {
                    err = Some(e);
                    return Ordering::Equal;
                }
            };
            let ord = order_cmp(&va, &vb, key.asc, key.nulls_first);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Fold an aggregate over the rows of a group (the group is empty only for a
/// bare aggregate over an empty input, which yields the SQL identity element).
/// `eval_row` evaluates the argument against one group row in the caller's scope
/// (single-table `schema` or relational namespace), so this is scope-agnostic.
fn compute_aggregate(
    func: AggFunc,
    arg: &AggArg,
    distinct: bool,
    sep: Option<&str>,
    rows: &[&[Value]],
    eval_row: &dyn Fn(&Expr, &[Value]) -> Result<Value>,
) -> Result<Value> {
    if let (AggFunc::Count, AggArg::Star) = (func, arg) {
        return Ok(Value::Int(rows.len() as i64));
    }
    let AggArg::Expr(expr) = arg else {
        return Err(EngineError::sql("only COUNT(*) takes a star argument"));
    };

    // json_agg keeps every element (including NULLs and group order); the
    // others fold over non-NULL values only.
    if matches!(func, AggFunc::JsonAgg) {
        if rows.is_empty() {
            return Ok(Value::Null); // PostgREST wraps this in coalesce(…, '[]')
        }
        let mut parts = Vec::with_capacity(rows.len());
        for r in rows {
            parts.push(value_to_json(&eval_row(expr, r)?));
        }
        return Ok(Value::Text(format!("[{}]", parts.join(","))));
    }

    let mut vals: Vec<Value> = Vec::new();
    for r in rows {
        let v = eval_row(expr, r)?;
        if !v.is_null() {
            vals.push(v);
        }
    }
    // DISTINCT folds over the unique non-NULL values (e.g. COUNT(DISTINCT x)).
    if distinct {
        let mut seen: HashSet<String> = HashSet::new();
        vals.retain(|v| seen.insert(value_key(v)));
    }
    if matches!(func, AggFunc::GroupConcat) {
        if vals.is_empty() {
            return Ok(Value::Null);
        }
        let sep = sep.unwrap_or(",");
        let joined = vals
            .iter()
            .map(|v| v.render().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(sep);
        return Ok(Value::Text(joined));
    }
    Ok(match func {
        AggFunc::JsonAgg | AggFunc::GroupConcat => unreachable!("handled above"),
        AggFunc::Count => Value::Int(vals.len() as i64),
        AggFunc::Min => vals
            .into_iter()
            .reduce(|a, b| {
                if null_aware_cmp(&b, &a) == Ordering::Less {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        AggFunc::Max => vals
            .into_iter()
            .reduce(|a, b| {
                if null_aware_cmp(&b, &a) == Ordering::Greater {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(Value::Null),
        AggFunc::Sum => {
            if vals.is_empty() {
                Value::Null
            } else if vals.iter().all(|v| matches!(v, Value::Int(_))) {
                Value::Int(vals.iter().map(li).sum())
            } else {
                Value::Real(vals.iter().map(|v| num(v).unwrap_or(0.0)).sum())
            }
        }
        AggFunc::Avg => {
            if vals.is_empty() {
                Value::Null
            } else {
                let sum: f64 = vals.iter().map(|v| num(v).unwrap_or(0.0)).sum();
                Value::Real(sum / vals.len() as f64)
            }
        }
    })
}

fn null_aware_cmp(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less, // NULLs sort first
        (false, true) => Ordering::Greater,
        (false, false) => a.sql_cmp(b).unwrap_or(Ordering::Equal),
    }
}

fn agg_name(f: AggFunc) -> String {
    match f {
        AggFunc::Count => "count",
        AggFunc::Sum => "sum",
        AggFunc::Min => "min",
        AggFunc::Max => "max",
        AggFunc::Avg => "avg",
        AggFunc::JsonAgg => "json_agg",
        AggFunc::GroupConcat => "group_concat",
    }
    .to_string()
}

/// The default output column name Postgres would assign an unaliased item: the
/// column for a bare column ref, the function name for a call/aggregate, else a
/// positional `?columnN?`-style fallback.
fn column_name(expr: &Expr, alias: &Option<String>, idx: usize) -> String {
    if let Some(a) = alias {
        return a.clone();
    }
    match expr {
        Expr::Column(c) => c.clone(),
        Expr::Qualified(_, c) => c.clone(),
        Expr::Aggregate { func, .. } => agg_name(*func),
        Expr::Func { name, .. } => name.to_ascii_lowercase(),
        Expr::Cast { e, .. } => column_name(e, &None, idx),
        _ => format!("col{}", idx + 1),
    }
}

/// Dispatch a scalar function call. Unknown functions are an error (rather than
/// silently NULL) so typos surface; the set here covers the common standard SQL
/// helpers plus the JSON builders PostgREST-style projections lean on.
fn call_func(name: &str, args: Vec<Value>) -> Result<Value> {
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        // NULL handling.
        "coalesce" => Ok(args
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null)),
        "nullif" => {
            let a = args.first().cloned().unwrap_or(Value::Null);
            let b = args.get(1).cloned().unwrap_or(Value::Null);
            Ok(if a.sql_eq(&b) == Some(true) {
                Value::Null
            } else {
                a
            })
        }
        // `ifnull(a, b)` / SQLite alias of two-arg coalesce.
        "ifnull" => {
            let a = args.first().cloned().unwrap_or(Value::Null);
            Ok(if a.is_null() {
                args.get(1).cloned().unwrap_or(Value::Null)
            } else {
                a
            })
        }
        // `iif(cond, a, b)` (SQLite/T-SQL): the ternary conditional.
        "iif" | "if" => {
            if args.len() != 3 {
                return Err(EngineError::sql("iif() takes exactly three arguments"));
            }
            Ok(match args[0].as_bool() {
                Some(true) => args[1].clone(),
                _ => args[2].clone(),
            })
        }
        // `greatest`/`least`: the max/min non-NULL argument (NULLs ignored; all
        // NULL → NULL), matching Postgres.
        "greatest" => Ok(extreme(args, Ordering::Greater)),
        "least" => Ok(extreme(args, Ordering::Less)),
        // Strings.
        "lower" => Ok(map_text(args, |s| s.to_lowercase())),
        "upper" => Ok(map_text(args, |s| s.to_uppercase())),
        "trim" | "btrim" => Ok(map_text(args, |s| s.trim().to_string())),
        "ltrim" => Ok(map_text(args, |s| s.trim_start().to_string())),
        "rtrim" => Ok(map_text(args, |s| s.trim_end().to_string())),
        "length" | "char_length" | "character_length" => Ok(match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Int(text_of(v).chars().count() as i64),
        }),
        "concat" => Ok(Value::Text(
            args.iter()
                .filter(|v| !v.is_null())
                .map(text_of)
                .collect::<Vec<_>>()
                .concat(),
        )),
        // Numbers.
        "abs" => Ok(match args.into_iter().next() {
            Some(Value::Int(i)) => Value::Int(i.wrapping_abs()),
            Some(Value::Real(r)) => Value::Real(r.abs()),
            Some(Value::Null) | None => Value::Null,
            Some(other) => return Err(EngineError::sql(format!("abs() of {}", other.type_name()))),
        }),
        // round(x) → integer; round(x, n) → real to n decimal places.
        "round" => round_func(&args),
        "substr" | "substring" => Ok(substr_func(&args)),
        "replace" => Ok(match (args.first(), args.get(1), args.get(2)) {
            (Some(s), Some(f), Some(t)) if !s.is_null() && !f.is_null() && !t.is_null() => {
                Value::Text(text_of(s).replace(&text_of(f), &text_of(t)))
            }
            _ => Value::Null,
        }),
        // 1-based index of `needle` in `haystack`, 0 if absent (SQLite instr /
        // Postgres strpos with the argument order each expects).
        "instr" => Ok(instr_func(args.first(), args.get(1))),
        "strpos" => Ok(instr_func(args.first(), args.get(1))),
        "repeat" => Ok(match (args.first(), num_arg(&args, 1)) {
            (Some(s), Some(n)) if !s.is_null() => {
                Value::Text(text_of(s).repeat((n as i64).max(0) as usize))
            }
            _ => Value::Null,
        }),
        "reverse" => Ok(map_text(args, |s| s.chars().rev().collect())),
        "left" => Ok(side_func(&args, true)),
        "right" => Ok(side_func(&args, false)),
        "lpad" => Ok(pad_func(&args, true)),
        "rpad" => Ok(pad_func(&args, false)),
        // Math (each propagates NULL; non-numeric is an error).
        "ceil" | "ceiling" => unary_real(&args, "ceil", f64::ceil),
        "floor" => unary_real(&args, "floor", f64::floor),
        "sqrt" => unary_real(&args, "sqrt", f64::sqrt),
        "exp" => unary_real(&args, "exp", f64::exp),
        "ln" => unary_real(&args, "ln", f64::ln),
        "log" | "log10" => unary_real(&args, "log", f64::log10),
        "sign" => Ok(match num_arg(&args, 0) {
            Some(n) => Value::Int(n.partial_cmp(&0.0).map_or(0, |o| o as i64)),
            None => Value::Null,
        }),
        "trunc" => Ok(match num_arg(&args, 0) {
            Some(n) => Value::Real(n.trunc()),
            None => Value::Null,
        }),
        "power" | "pow" => Ok(match (num_arg(&args, 0), num_arg(&args, 1)) {
            (Some(a), Some(b)) => Value::Real(a.powf(b)),
            _ => Value::Null,
        }),
        "mod" => Ok(match (num_arg(&args, 0), num_arg(&args, 1)) {
            (Some(_), Some(0.0)) => Value::Null,
            (Some(a), Some(b)) => Value::Int((a as i64) % (b as i64)),
            _ => Value::Null,
        }),
        "pi" => Ok(Value::Real(std::f64::consts::PI)),
        // Date / time (UTC; SQLite text/epoch model — see [`crate::datetime`]).
        "now"
        | "current_timestamp"
        | "transaction_timestamp"
        | "statement_timestamp"
        | "clock_timestamp"
        | "localtimestamp" => Ok(Value::Text(datetime::format_timestamp(
            datetime::now_epoch(),
        ))),
        "current_date" => Ok(Value::Text(datetime::format_date(datetime::now_epoch()))),
        "current_time" | "localtime" => {
            Ok(Value::Text(datetime::format_time(datetime::now_epoch())))
        }
        "date" => Ok(map_epoch(args.first(), datetime::format_date)),
        "datetime" => Ok(map_epoch(args.first(), datetime::format_timestamp)),
        "time" => Ok(map_epoch(args.first(), datetime::format_time)),
        "unixepoch" => Ok(match value_to_epoch(args.first()) {
            Some(e) => Value::Int(e),
            None => Value::Null,
        }),
        "date_trunc" => date_trunc_func(&args),
        "extract" => Ok(extract_func(&args)),
        "strftime" => Ok(strftime_func(&args)),
        // UUID / misc.
        "gen_random_uuid" | "uuid_generate_v4" => Ok(Value::Text(gen_uuid_v4())),
        "random" => Ok(Value::Real((rng_u64() >> 11) as f64 / (1u64 << 53) as f64)),
        "typeof" => Ok(Value::Text(match args.first() {
            Some(v) => v.type_name().to_ascii_lowercase(),
            None => "null".to_string(),
        })),
        "hex" => Ok(match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(Value::Blob(b)) => Value::Text(to_hex(b)),
            Some(v) => Value::Text(to_hex(text_of(v).as_bytes())),
        }),
        // JSON accessors (the engine stores JSON as text — see [`crate::json`]).
        "json_get" => Ok(json_get(args.first(), args.get(1), false)),
        "json_get_text" => Ok(json_get(args.first(), args.get(1), true)),
        "json_extract" => Ok(json_extract_func(args.first(), args.get(1))),
        "json_array" | "json_build_array" | "jsonb_build_array" => Ok(Value::Text(format!(
            "[{}]",
            args.iter().map(value_to_json).collect::<Vec<_>>().join(",")
        ))),
        // JSON builders (best-effort; the engine has no native json type, so the
        // result is JSON-encoded text — see [`value_to_json`]).
        "to_json" | "to_jsonb" => Ok(Value::Text(value_to_json(
            args.first().unwrap_or(&Value::Null),
        ))),
        "json_build_object" | "jsonb_build_object" => json_build_object(&args),
        other => Err(EngineError::sql(format!("unknown function: {other}"))),
    }
}

/// A numeric argument as `f64` (Int/Real, or numeric text); `None` for NULL,
/// missing, or non-numeric.
fn num_arg(args: &[Value], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(Value::Int(n)) => Some(*n as f64),
        Some(Value::Real(r)) => Some(*r),
        Some(Value::Text(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Apply a unary real math function, propagating NULL and erroring on non-numeric.
fn unary_real(args: &[Value], name: &str, f: impl Fn(f64) -> f64) -> Result<Value> {
    match args.first() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(Value::Int(n)) => Ok(Value::Real(f(*n as f64))),
        Some(Value::Real(r)) => Ok(Value::Real(f(*r))),
        Some(other) => Err(EngineError::sql(format!(
            "{name}() of {}",
            other.type_name()
        ))),
    }
}

/// `round(x)` → nearest integer; `round(x, n)` → real to `n` decimal places.
fn round_func(args: &[Value]) -> Result<Value> {
    let x = match args.first() {
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(Value::Int(i)) if args.len() < 2 => return Ok(Value::Int(*i)),
        Some(Value::Int(i)) => *i as f64,
        Some(Value::Real(r)) => *r,
        Some(other) => {
            return Err(EngineError::sql(format!(
                "round() of {}",
                other.type_name()
            )))
        }
    };
    match num_arg(args, 1) {
        Some(n) => {
            let f = 10f64.powi(n as i32);
            Ok(Value::Real((x * f).round() / f))
        }
        None => Ok(Value::Int(x.round() as i64)),
    }
}

/// `substr(s, start [, len])` — 1-based, negative `start` counts from the end
/// (SQLite semantics). NULL propagates.
fn substr_func(args: &[Value]) -> Value {
    let s = match args.first() {
        Some(Value::Null) | None => return Value::Null,
        Some(v) => text_of(v),
    };
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let Some(start) = num_arg(args, 1).map(|f| f as i64) else {
        return Value::Null;
    };
    let from1 = if start < 0 { n + start + 1 } else { start };
    let begin = (from1 - 1).clamp(0, n) as usize;
    let end = match args.get(2) {
        Some(Value::Null) => return Value::Null,
        Some(_) => {
            let len = num_arg(args, 2).unwrap_or(0.0) as i64;
            ((from1 - 1 + len).clamp(0, n)) as usize
        }
        None => chars.len(),
    };
    let end = end.max(begin);
    Value::Text(chars[begin..end].iter().collect())
}

/// 1-based index of `needle` in `haystack` (0 = absent); NULL propagates.
fn instr_func(haystack: Option<&Value>, needle: Option<&Value>) -> Value {
    match (haystack, needle) {
        (Some(h), Some(n)) if !h.is_null() && !n.is_null() => {
            let hs = text_of(h);
            let nd = text_of(n);
            match hs.find(&nd) {
                Some(byte_idx) => Value::Int(hs[..byte_idx].chars().count() as i64 + 1),
                None => Value::Int(0),
            }
        }
        _ => Value::Null,
    }
}

/// `left(s, n)` / `right(s, n)` — first/last `n` characters (negative `n` drops
/// from the far end, Postgres-style). NULL propagates.
fn side_func(args: &[Value], left: bool) -> Value {
    let s = match args.first() {
        Some(Value::Null) | None => return Value::Null,
        Some(v) => text_of(v),
    };
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let Some(k) = num_arg(args, 1).map(|f| f as i64) else {
        return Value::Null;
    };
    let take = if k >= 0 { k.min(n) } else { (n + k).max(0) } as usize;
    let slice: String = if left {
        chars[..take].iter().collect()
    } else {
        chars[n as usize - take..].iter().collect()
    };
    Value::Text(slice)
}

/// `lpad`/`rpad(s, len [, fill])` to `len` characters with `fill` (default space).
fn pad_func(args: &[Value], left: bool) -> Value {
    let s = match args.first() {
        Some(Value::Null) | None => return Value::Null,
        Some(v) => text_of(v),
    };
    let Some(len) = num_arg(args, 1).map(|f| f as i64) else {
        return Value::Null;
    };
    let len = len.max(0) as usize;
    let fill = match args.get(2) {
        Some(v) if !v.is_null() => text_of(v),
        _ => " ".to_string(),
    };
    let chars: Vec<char> = s.chars().collect();
    if chars.len() >= len || fill.is_empty() {
        return Value::Text(chars.into_iter().take(len).collect());
    }
    let fill_chars: Vec<char> = fill.chars().collect();
    let pad: String = (0..len - chars.len())
        .map(|i| fill_chars[i % fill_chars.len()])
        .collect();
    Value::Text(if left {
        format!("{pad}{s}")
    } else {
        format!("{s}{pad}")
    })
}

/// Evaluate a standalone constant expression from SQL text — used for stored
/// `DEFAULT` clauses backfilled by `ALTER TABLE ADD COLUMN` (stage 6D).
pub(crate) fn eval_const_sql(sql: &str) -> Result<Value> {
    let expr = crate::sql::parse_expr(sql)?;
    eval(&expr, &EvalCtx::root(&[]))
}

/// A composite key string for a tuple of columns (NULL-distinct uniqueness
/// keying), reusing the per-value [`value_key`] encoding.
fn composite_key(vals: &[Value], cols: &[usize]) -> String {
    let mut k = String::new();
    for &c in cols {
        k.push_str(&value_key(&vals[c]));
        k.push('\u{1}');
    }
    k
}

/// Enforce every non-primary-key `UNIQUE` set against the rows being written:
/// each `new_row` must not collide with a committed row (other than the
/// `exclude`d vids it is replacing) or with another `new_row`. A set with any
/// NULL key column is exempt (SQL treats NULLs as distinct).
fn check_secondary_uniques(
    store: &Store,
    table: &str,
    schema: &TableSchema,
    new_rows: &[&[Value]],
    exclude: &[u64],
    owner: u64,
) -> Result<()> {
    let pk = schema.primary_key_indices();
    for set in schema.unique_sets() {
        if set == pk {
            continue; // the primary key is enforced on its own path
        }
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(t) = store.table(table) {
            for v in &t.rows {
                if exclude.contains(&v.vid) || !v.visible_to_writer(store.committed_lsn, owner) {
                    continue;
                }
                if set.iter().any(|&i| v.values[i].is_null()) {
                    continue;
                }
                seen.insert(composite_key(&v.values, &set));
            }
        }
        for row in new_rows {
            if set.iter().any(|&i| row[i].is_null()) {
                continue;
            }
            if !seen.insert(composite_key(row, &set)) {
                let names: Vec<&str> = set
                    .iter()
                    .map(|&i| schema.columns[i].name.as_str())
                    .collect();
                return Err(EngineError::constraint(format!(
                    "UNIQUE constraint failed: {}.{}",
                    table,
                    names.join(",")
                )));
            }
        }
    }
    Ok(())
}

/// A timestamp value (Int epoch, or ISO-8601 / `'now'` text) as epoch seconds.
fn value_to_epoch(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int(i)) => Some(*i),
        Some(Value::Real(r)) => Some(*r as i64),
        Some(Value::Text(s)) => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("now") {
                Some(datetime::now_epoch())
            } else {
                datetime::parse_epoch_secs(t).or_else(|| t.parse::<i64>().ok())
            }
        }
        _ => None,
    }
}

/// Map a timestamp argument through a formatter, propagating NULL/parse failure.
fn map_epoch(v: Option<&Value>, f: impl Fn(i64) -> String) -> Value {
    match value_to_epoch(v) {
        Some(e) => Value::Text(f(e)),
        None => Value::Null,
    }
}

fn date_trunc_func(args: &[Value]) -> Result<Value> {
    let unit = match args.first() {
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => text_of(v),
    };
    let Some(e) = value_to_epoch(args.get(1)) else {
        return Ok(Value::Null);
    };
    match datetime::date_trunc(&unit, e) {
        Some(t) => Ok(Value::Text(datetime::format_timestamp(t))),
        None => Err(EngineError::sql(format!("unknown date_trunc unit: {unit}"))),
    }
}

/// `extract('field', ts)` (desugared from `EXTRACT(field FROM ts)`).
fn extract_func(args: &[Value]) -> Value {
    let field = match args.first() {
        Some(v) if !v.is_null() => text_of(v).to_ascii_lowercase(),
        _ => return Value::Null,
    };
    let Some(e) = value_to_epoch(args.get(1)) else {
        return Value::Null;
    };
    let (y, m, d, hh, mm, ss) = datetime::parts(e);
    Value::Int(match field.as_str() {
        "year" => y,
        "month" => m,
        "day" => d,
        "hour" => hh,
        "minute" => mm,
        "second" => ss,
        "dow" => datetime::day_of_week(e),
        "doy" => datetime::day_of_year(e),
        "quarter" => (m - 1) / 3 + 1,
        "epoch" => e,
        _ => return Value::Null,
    })
}

/// A small `strftime` subset over the UTC timestamp.
fn strftime_func(args: &[Value]) -> Value {
    let fmt = match args.first() {
        Some(v) if !v.is_null() => text_of(v),
        _ => return Value::Null,
    };
    let Some(e) = value_to_epoch(args.get(1)) else {
        return Value::Null;
    };
    let (y, m, d, hh, mm, ss) = datetime::parts(e);
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('m') => out.push_str(&format!("{m:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('H') => out.push_str(&format!("{hh:02}")),
            Some('M') => out.push_str(&format!("{mm:02}")),
            Some('S') => out.push_str(&format!("{ss:02}")),
            Some('s') => out.push_str(&e.to_string()),
            Some('j') => out.push_str(&format!("{:03}", datetime::day_of_year(e))),
            Some('w') => out.push_str(&datetime::day_of_week(e).to_string()),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    Value::Text(out)
}

/// `json -> key` / `json ->> key`: navigate JSON text by string key (object) or
/// integer index (array); `as_text` selects the `->>` (text) vs `->` (json) form.
fn json_get(doc: Option<&Value>, key: Option<&Value>, as_text: bool) -> Value {
    let (Some(doc), Some(key)) = (doc, key) else {
        return Value::Null;
    };
    if doc.is_null() || key.is_null() {
        return Value::Null;
    }
    let Some(parsed) = crate::json::Json::parse(&text_of(doc)) else {
        return Value::Null;
    };
    let sub = match key {
        Value::Int(i) => parsed.get_index(*i),
        v => parsed.get_key(&text_of(v)),
    };
    match sub {
        None => Value::Null,
        Some(j) if as_text => j.as_text().map(Value::Text).unwrap_or(Value::Null),
        Some(j) => Value::Text(j.to_json_text()),
    }
}

/// `json_extract(json, path)` — navigate an SQLite-style `$.a.b[0]` path and
/// return the value scalar-typed (string→text, number→int/real, bool→0/1).
fn json_extract_func(doc: Option<&Value>, path: Option<&Value>) -> Value {
    let (Some(doc), Some(path)) = (doc, path) else {
        return Value::Null;
    };
    if doc.is_null() || path.is_null() {
        return Value::Null;
    }
    let Some(parsed) = crate::json::Json::parse(&text_of(doc)) else {
        return Value::Null;
    };
    match parsed.extract_path(&text_of(path)) {
        None | Some(crate::json::Json::Null) => Value::Null,
        Some(crate::json::Json::Bool(b)) => Value::Int(*b as i64),
        Some(crate::json::Json::Num(n)) => {
            if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e15 {
                Value::Int(*n as i64)
            } else {
                Value::Real(*n)
            }
        }
        Some(crate::json::Json::Str(s)) => Value::Text(s.clone()),
        Some(other) => Value::Text(other.to_json_text()),
    }
}

/// Uppercase hex of a byte slice (SQLite `hex`).
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

/// A non-cryptographic 64-bit random draw — a splitmix64 of a per-call counter,
/// the wall clock, and the pid. Used by `random()`/`gen_random_uuid()`; the
/// concrete drawn value is what lands in the WAL, so replay never re-rolls.
fn rng_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut x = nanos ^ c.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((std::process::id() as u64) << 40);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A random RFC-4122 v4 UUID string.
fn gen_uuid_v4() -> String {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&rng_u64().to_le_bytes());
    b[8..].copy_from_slice(&rng_u64().to_le_bytes());
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // variant 10
    let h = to_hex(&b).to_lowercase();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Reduce arguments to the one comparing `want` (Greatest/Least), skipping NULLs
/// (Postgres semantics); all-NULL or empty → NULL.
fn extreme(args: Vec<Value>, want: Ordering) -> Value {
    args.into_iter()
        .filter(|v| !v.is_null())
        .reduce(|a, b| if a.sql_cmp(&b) == Some(want) { a } else { b })
        .unwrap_or(Value::Null)
}

/// Apply `f` to a single text argument, propagating NULL and rendering non-text
/// scalars to their text form first.
fn map_text(args: Vec<Value>, f: impl Fn(&str) -> String) -> Value {
    match args.into_iter().next() {
        Some(Value::Null) | None => Value::Null,
        Some(v) => Value::Text(f(&text_of(&v))),
    }
}

/// Render a value to its text form for string functions (NULL → empty string;
/// callers handle NULL propagation before calling where it matters).
fn text_of(v: &Value) -> String {
    v.render().unwrap_or_default()
}

/// `json_build_object(k1, v1, k2, v2, …)` → a JSON object. Keys render to text;
/// values are JSON-encoded.
fn json_build_object(args: &[Value]) -> Result<Value> {
    if args.len() % 2 != 0 {
        return Err(EngineError::sql(
            "json_build_object requires an even number of arguments",
        ));
    }
    let mut parts = Vec::with_capacity(args.len() / 2);
    for pair in args.chunks(2) {
        let key = json_quote(&text_of(&pair[0]));
        parts.push(format!("{key}:{}", value_to_json(&pair[1])));
    }
    Ok(Value::Text(format!("{{{}}}", parts.join(","))))
}

/// Encode a value as a JSON fragment. NB: the engine has no json type, so a
/// `Text` value is encoded as a JSON string — a value that is itself already
/// JSON is therefore re-quoted. (Faithful nested-JSON output is part of the
/// embedding work that needs the real PostgREST corpus.)
fn value_to_json(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Real(r) => {
            if r.is_finite() {
                format!("{r}")
            } else {
                "null".to_string()
            }
        }
        Value::Text(s) => json_quote(s),
        Value::Blob(b) => json_quote(&crate::value::base64_encode(b)),
        Value::Vector(_) => {
            let xs = v.as_vector().unwrap();
            let parts: Vec<String> = xs.iter().map(|x| format!("{x}")).collect();
            format!("[{}]", parts.join(","))
        }
    }
}

/// Quote and escape a string as a JSON string literal. (The `'\"'` char literals
/// are written escaped so the `lizard` complexity tool's tokenizer doesn't
/// mistake the inner quote for a string delimiter.)
fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\"');
    for c in s.chars() {
        match c {
            '\"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\"');
    out
}

// ---- writes (validate fully, then apply: atomic per statement) -------------

/// The outcome of a write statement: the WAL ops to make durable, the affected-
/// row count, and — when the statement carried `RETURNING` — the projected rows.
#[derive(Default)]
pub struct Mutation {
    pub wal: Vec<WalOp>,
    pub changes: i64,
    pub result: Option<ResultSet>,
}

/// Project affected rows (full table-row value vectors) through a `RETURNING`
/// list — `*` expands the table's columns, expressions evaluate per row.
fn project_returning(
    items: &[SelItem],
    schema: &TableSchema,
    rows: &[Vec<Value>],
    params: &[Value],
) -> Result<ResultSet> {
    let mut columns = Vec::new();
    let mut types: Vec<ColumnType> = Vec::new();
    let mut plan: Vec<ProjItem> = Vec::new();
    for item in items {
        match item {
            SelItem::Star { .. } => {
                for c in &schema.columns {
                    columns.push(c.name.clone());
                    types.push(c.ty);
                    plan.push(ProjItem::Column(schema.column_index(&c.name).unwrap()));
                }
            }
            SelItem::Expr { expr, alias } => {
                columns.push(column_name(expr, alias, columns.len()));
                types.push(expr_type(expr, Some(schema)));
                plan.push(ProjItem::Expr(expr.clone()));
            }
        }
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for r in rows {
        let mut out = Vec::with_capacity(plan.len());
        for p in &plan {
            match p {
                ProjItem::Column(i) => out.push(r[*i].clone()),
                ProjItem::Expr(e) => {
                    let ctx = EvalCtx::row(r, schema, params);
                    out.push(eval(e, &ctx)?);
                }
            }
        }
        out_rows.push(out);
    }
    Ok(ResultSet {
        columns,
        types,
        rows: out_rows,
    })
}

/// Enforce parsed `CHECK` predicates against a fully-formed row. A predicate
/// fails only when it evaluates to false; NULL/unknown passes (SQL semantics).
fn check_row_constraints(checks: &[Expr], schema: &TableSchema, vals: &[Value]) -> Result<()> {
    for c in checks {
        let ctx = EvalCtx::row(vals, schema, &[]);
        if eval(c, &ctx)?.as_bool() == Some(false) {
            return Err(EngineError::constraint(format!(
                "CHECK constraint failed: {}",
                schema.name
            )));
        }
    }
    Ok(())
}

/// NOT NULL enforcement over a fully-formed row (insert/update validation pass).
fn check_not_null(schema: &TableSchema, vals: &[Value], table: &str) -> Result<()> {
    for (i, c) in schema.columns.iter().enumerate() {
        if c.not_null && vals[i].is_null() {
            return Err(EngineError::constraint(format!(
                "NOT NULL constraint failed: {}.{}",
                table, c.name
            )));
        }
    }
    Ok(())
}

/// Stamp `old_vid` as superseded by this writer and emit its delete WAL op.
fn supersede(store: &mut Store, table: &str, old_vid: u64, owner: u64, wal: &mut Vec<WalOp>) {
    if let Some(rv) = store.table_mut(table).unwrap().version_mut(old_vid) {
        rv.delete_lsn = PENDING;
        rv.owner = owner;
    }
    wal.push(WalOp::Delete {
        table: table.to_string(),
        vid: old_vid,
    });
}

/// Append a new PENDING row version to `table`, emit its insert WAL op, and
/// return the allocated vid.
fn append_version(
    store: &mut Store,
    table: &str,
    vals: &[Value],
    owner: u64,
    wal: &mut Vec<WalOp>,
) -> u64 {
    let t = store.table_mut(table).unwrap();
    let vid = t.alloc_vid();
    t.rows.push(RowVersion {
        vid,
        values: vals.to_vec(),
        create_lsn: PENDING,
        delete_lsn: 0,
        owner,
    });
    wal.push(WalOp::Insert {
        table: table.to_string(),
        vid,
        values: vals.to_vec(),
    });
    vid
}

/// For a table with primary-key columns `pk` (possibly composite): composite
/// keys live in committed state mapped to their row vid (the rows an upsert may
/// replace/update), plus the keys a concurrent in-flight writer is inserting (a
/// clash there is retryable).
fn pk_index(
    store: &Store,
    table: &str,
    pk: &[usize],
    me: u64,
) -> (HashMap<String, u64>, HashSet<String>) {
    let mut committed = HashMap::new();
    let mut pending = HashSet::new();
    if let Some(t) = store.table(table) {
        for v in &t.rows {
            if v.visible_to_writer(store.committed_lsn, me) {
                committed.insert(composite_key(&v.values, pk), v.vid);
            } else if v.create_lsn == PENDING && v.owner != me && v.delete_lsn == 0 {
                pending.insert(composite_key(&v.values, pk));
            }
        }
    }
    (committed, pending)
}

/// Per-statement write context: the MVCC snapshot, the in-flight writer id, and
/// the bound parameters — bundled so the write entry points stay within the
/// argument-count budget the complexity gate enforces.
pub struct WriteCtx<'a> {
    pub snapshot: u64,
    pub owner: u64,
    pub params: &'a [Value],
}

pub fn run_insert(
    store: &mut Store,
    table: &str,
    columns: &Option<Vec<String>>,
    source: &InsertSource,
    on_conflict: &OnConflict,
    returning: Option<&[SelItem]>,
    wc: &WriteCtx,
) -> Result<Mutation> {
    let schema = store
        .table(table)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table}")))?
        .schema
        .clone();

    let slots = build_insert_slots(store, &schema, columns, source, wc)?;
    let staged = finalize_insert_rows(store, &schema, table, slots)?;

    // Secondary UNIQUE is enforced for plain inserts (upserts resolve on the PK).
    if matches!(on_conflict, OnConflict::Error) {
        let rows: Vec<&[Value]> = staged.iter().map(|v| v.as_slice()).collect();
        check_secondary_uniques(store, table, &schema, &rows, &[], wc.owner)?;
    }
    apply_insert(store, &schema, table, staged, on_conflict, returning, wc)
}

/// The columns an `INSERT`'s source values map onto (all, in order, by default).
fn insert_targets(schema: &TableSchema, columns: &Option<Vec<String>>) -> Result<Vec<usize>> {
    match columns {
        Some(cols) => cols
            .iter()
            .map(|c| {
                schema
                    .column_index(c)
                    .ok_or_else(|| EngineError::sql(format!("no such column: {c}")))
            })
            .collect(),
        None => Ok((0..schema.columns.len()).collect()),
    }
}

/// Build per-row column slots from the row source; `None` means "use the
/// column's default" (a `DEFAULT` keyword, an omitted column, or `DEFAULT VALUES`).
fn build_insert_slots(
    store: &mut Store,
    schema: &TableSchema,
    columns: &Option<Vec<String>>,
    source: &InsertSource,
    wc: &WriteCtx,
) -> Result<Vec<Vec<Option<Value>>>> {
    let ncols = schema.columns.len();
    let targets = insert_targets(schema, columns)?;
    let mut slots: Vec<Vec<Option<Value>>> = Vec::new();
    match source {
        InsertSource::Values(rows) => {
            let ctx = EvalCtx::root(wc.params);
            for exprs in rows {
                let mut slot: Vec<Option<Value>> = vec![None; ncols];
                if !(exprs.is_empty() && columns.is_none()) {
                    if exprs.len() != targets.len() {
                        return Err(EngineError::sql(format!(
                            "INSERT has {} target column(s) but {} value(s)",
                            targets.len(),
                            exprs.len()
                        )));
                    }
                    for (&idx, e) in targets.iter().zip(exprs) {
                        slot[idx] = match e {
                            Expr::Default => None,
                            _ => Some(eval(e, &ctx)?),
                        };
                    }
                }
                slots.push(slot);
            }
        }
        // INSERT … SELECT: the query sees this writer's snapshot (committed +
        // its own pending), feeding its result rows into the same staging loop.
        InsertSource::Select(sel) => {
            for r in run_select(store, sel, wc.snapshot, Some(wc.owner), wc.params)?.rows {
                if r.len() != targets.len() {
                    return Err(EngineError::sql(format!(
                        "INSERT … SELECT produced {} column(s) for {} target(s)",
                        r.len(),
                        targets.len()
                    )));
                }
                let mut slot: Vec<Option<Value>> = vec![None; ncols];
                for (&idx, v) in targets.iter().zip(r) {
                    slot[idx] = Some(v);
                }
                slots.push(slot);
            }
        }
    }
    Ok(slots)
}

/// Finalize staged slots: fill defaults / autoincrement, coerce, and validate
/// (NOT NULL, vector dims, CHECK), advancing the table's autoincrement counter.
fn finalize_insert_rows(
    store: &mut Store,
    schema: &TableSchema,
    table: &str,
    slots: Vec<Vec<Option<Value>>>,
) -> Result<Vec<Vec<Value>>> {
    let ncols = schema.columns.len();
    let check_exprs: Vec<Expr> = schema
        .checks
        .iter()
        .map(|s| crate::sql::parse_expr(s))
        .collect::<Result<_>>()?;
    let mut autoinc = store.table(table).map(|t| t.next_autoinc).unwrap_or(1);
    let mut staged: Vec<Vec<Value>> = Vec::with_capacity(slots.len());
    for slot in slots {
        let mut vals = vec![Value::Null; ncols];
        for (i, c) in schema.columns.iter().enumerate() {
            vals[i] = finalize_cell(c, &slot[i], &mut autoinc)?;
        }
        check_not_null(schema, &vals, table)?;
        check_vector_dims(schema, &vals, table)?;
        check_row_constraints(&check_exprs, schema, &vals)?;
        staged.push(vals);
    }
    // Persist the advanced autoincrement counter (single-writer, so no race).
    if let Some(t) = store.table_mut(table) {
        t.next_autoinc = autoinc;
    }
    Ok(staged)
}

/// Resolve one column cell: a provided value (coerced; bumping the autoincrement
/// counter past it), or its default — autoincrement, `DEFAULT` expr, or NULL.
fn finalize_cell(
    c: &crate::catalog::Column,
    slot: &Option<Value>,
    autoinc: &mut i64,
) -> Result<Value> {
    Ok(match slot {
        Some(v) => {
            let v = c.ty.coerce(v.clone());
            if c.autoincrement {
                if let Value::Int(n) = &v {
                    *autoinc = (*autoinc).max(n + 1);
                }
            }
            v
        }
        None if c.autoincrement => {
            let a = *autoinc;
            *autoinc += 1;
            Value::Int(a)
        }
        None => match &c.default_sql {
            Some(d) => c.ty.coerce(eval_const_sql(d)?),
            None => Value::Null,
        },
    })
}

/// Read-only context shared by the staged-INSERT row appliers: the table schema,
/// its name, and the per-statement write context. Bundled so the row-apply
/// helpers stay within the argument-count budget the complexity gate enforces.
struct ApplyCtx<'a, 'p> {
    schema: &'a TableSchema,
    table: &'a str,
    wc: &'a WriteCtx<'p>,
}

/// Resolve the primary-key conflict action and apply the staged rows.
fn apply_insert(
    store: &mut Store,
    schema: &TableSchema,
    table: &str,
    staged: Vec<Vec<Value>>,
    on_conflict: &OnConflict,
    returning: Option<&[SelItem]>,
    wc: &WriteCtx,
) -> Result<Mutation> {
    let maintain = store.table_has_index(table);
    let mut wal = Vec::new();
    let mut affected: Vec<Vec<Value>> = Vec::new();
    let mut new_index_rows: Vec<(u64, Vec<Value>)> = Vec::new();
    let pk_cols = schema.primary_key_indices();
    let actx = ApplyCtx { schema, table, wc };

    if pk_cols.is_empty() {
        // No primary key → no conflict is possible; straight inserts.
        for vals in staged {
            let vid = append_version(store, table, &vals, wc.owner, &mut wal);
            if maintain {
                new_index_rows.push((vid, vals.clone()));
            }
            affected.push(vals);
        }
    } else {
        let pk_name = pk_names(schema, &pk_cols);
        let (mut live_vid, pending) = pk_index(store, table, &pk_cols, wc.owner);
        for vals in staged {
            let key = composite_key(&vals, &pk_cols);
            if pending.contains(&key) {
                return Err(EngineError::new(
                    crate::error::EngineStatus::ErrConflict,
                    format!("write conflict: {table}.{pk_name} is being inserted by a concurrent transaction"),
                ));
            }
            let clash = live_vid.get(&key).copied();
            let Some(produced) =
                apply_insert_row(store, &actx, vals, clash, on_conflict, &pk_name, &mut wal)?
            else {
                continue; // DO NOTHING skipped this row
            };
            let vid = *store
                .table(table)
                .and_then(|t| t.rows.last())
                .map(|r| &r.vid)
                .unwrap();
            live_vid.insert(key, vid);
            if maintain {
                new_index_rows.push((vid, produced.clone()));
            }
            affected.push(produced);
        }
    }
    for (vid, vals) in &new_index_rows {
        store.index_row_inserted(table, *vid, vals);
    }
    let changes = affected.len() as i64;
    let result = match returning {
        Some(items) => Some(project_returning(items, schema, &affected, wc.params)?),
        None => None,
    };
    Ok(Mutation {
        wal,
        changes,
        result,
    })
}

/// Apply one staged row given its primary-key clash (if any) and the conflict
/// action, appending the inserted/updated version. Returns the produced row, or
/// `None` when the row was skipped (`DO NOTHING`).
fn apply_insert_row(
    store: &mut Store,
    actx: &ApplyCtx,
    vals: Vec<Value>,
    clash: Option<u64>,
    on_conflict: &OnConflict,
    pk_name: &str,
    wal: &mut Vec<WalOp>,
) -> Result<Option<Vec<Value>>> {
    let table = actx.table;
    let Some(old_vid) = clash else {
        append_version(store, table, &vals, actx.wc.owner, wal);
        return Ok(Some(vals));
    };
    match on_conflict {
        OnConflict::Error => Err(EngineError::constraint(format!(
            "UNIQUE constraint failed: {table}.{pk_name}"
        ))),
        OnConflict::Nothing => Ok(None),
        OnConflict::Replace => {
            supersede(store, table, old_vid, actx.wc.owner, wal);
            append_version(store, table, &vals, actx.wc.owner, wal);
            Ok(Some(vals))
        }
        OnConflict::Update { sets, filter } => {
            upsert_update(store, actx, old_vid, &vals, sets, filter, wal)
        }
    }
}

/// The `ON CONFLICT DO UPDATE` branch: compute the new row from the existing one
/// (with `excluded.*` bound to the proposed insert), validate, and apply it.
fn upsert_update(
    store: &mut Store,
    actx: &ApplyCtx,
    old_vid: u64,
    excluded: &[Value],
    sets: &[(String, Expr)],
    filter: &Option<Expr>,
    wal: &mut Vec<WalOp>,
) -> Result<Option<Vec<Value>>> {
    let (schema, table) = (actx.schema, actx.table);
    let existing = store
        .table(table)
        .and_then(|t| t.version(old_vid))
        .map(|r| r.values.clone())
        .ok_or_else(|| EngineError::sql("ON CONFLICT DO UPDATE cannot affect one row twice"))?;
    let ctx = EvalCtx {
        row: Some(&existing),
        schema: Some(schema),
        params: actx.wc.params,
        group: None,
        excluded: Some(excluded),
        cols: None,
    };
    if let Some(f) = filter {
        if !eval(f, &ctx)?.as_bool().unwrap_or(false) {
            return Ok(None);
        }
    }
    let mut nv = existing.clone();
    for (col, expr) in sets {
        let idx = schema
            .column_index(col)
            .ok_or_else(|| EngineError::sql(format!("no such column: {col}")))?;
        nv[idx] = schema.columns[idx].ty.coerce(eval(expr, &ctx)?);
    }
    check_not_null(schema, &nv, table)?;
    check_vector_dims(schema, &nv, table)?;
    supersede(store, table, old_vid, actx.wc.owner, wal);
    append_version(store, table, &nv, actx.wc.owner, wal);
    Ok(Some(nv))
}

/// The primary-key column names joined for an error message.
fn pk_names(schema: &TableSchema, pk_cols: &[usize]) -> String {
    pk_cols
        .iter()
        .map(|&i| schema.columns[i].name.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Enforce the declared dimension of every `vector(n)` column (spec 12 — vectors
/// are validated on insert). NULL is permitted unless a NOT NULL says otherwise.
fn check_vector_dims(schema: &TableSchema, vals: &[Value], table: &str) -> Result<()> {
    for (i, c) in schema.columns.iter().enumerate() {
        let ColumnType::Vector(dim) = c.ty else {
            continue;
        };
        match &vals[i] {
            Value::Null => {}
            Value::Vector(v) if v.len() == dim as usize => {}
            Value::Vector(v) => {
                return Err(EngineError::constraint(format!(
                    "vector dimension mismatch on {}.{}: expected {}, got {}",
                    table,
                    c.name,
                    dim,
                    v.len()
                )))
            }
            other => {
                return Err(EngineError::constraint(format!(
                    "column {}.{} is vector({}) but value is {}",
                    table,
                    c.name,
                    dim,
                    other.type_name()
                )))
            }
        }
    }
    Ok(())
}

pub fn run_delete(
    store: &mut Store,
    table: &str,
    filter: &Option<Expr>,
    returning: Option<&[SelItem]>,
    wc: &WriteCtx,
) -> Result<Mutation> {
    let schema = store
        .table(table)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table}")))?
        .schema
        .clone();
    let t = store.table(table).unwrap();
    let mut victims = Vec::new();
    // RETURNING projects the deleted (old) row values.
    let mut affected: Vec<Vec<Value>> = Vec::new();
    for v in &t.rows {
        if !v.visible_to_writer(wc.snapshot, wc.owner) {
            continue;
        }
        let ctx = EvalCtx::row(&v.values, &schema, wc.params);
        if predicate(filter, &ctx)? {
            check_no_conflict(v, wc.snapshot, wc.owner)?;
            victims.push(v.vid);
            if returning.is_some() {
                affected.push(v.values.clone());
            }
        }
    }
    let mut wal = Vec::with_capacity(victims.len());
    for vid in &victims {
        supersede(store, table, *vid, wc.owner, &mut wal);
    }
    let changes = victims.len() as i64;
    let result = match returning {
        Some(items) => Some(project_returning(items, &schema, &affected, wc.params)?),
        None => None,
    };
    Ok(Mutation {
        wal,
        changes,
        result,
    })
}

pub fn run_update(
    store: &mut Store,
    table: &str,
    sets: &[(String, Expr)],
    filter: &Option<Expr>,
    returning: Option<&[SelItem]>,
    wc: &WriteCtx,
) -> Result<Mutation> {
    let schema = store
        .table(table)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table}")))?
        .schema
        .clone();
    let updates = compute_updates(store, &schema, table, sets, filter, wc)?;
    check_update_uniques(store, &schema, table, &updates, wc.owner)?;
    apply_updates(store, &schema, table, updates, returning, wc)
}

/// Compute `(old_vid, new_values)` for every row matching the UPDATE filter,
/// validating NOT NULL / vector dims / CHECK. No mutation happens here.
fn compute_updates(
    store: &Store,
    schema: &TableSchema,
    table: &str,
    sets: &[(String, Expr)],
    filter: &Option<Expr>,
    wc: &WriteCtx,
) -> Result<Vec<(u64, Vec<Value>)>> {
    // Resolve assignment target indices once.
    let mut targets = Vec::with_capacity(sets.len());
    for (col, expr) in sets {
        let idx = schema
            .column_index(col)
            .ok_or_else(|| EngineError::sql(format!("no such column: {col}")))?;
        targets.push((idx, expr));
    }
    let check_exprs: Vec<Expr> = schema
        .checks
        .iter()
        .map(|s| crate::sql::parse_expr(s))
        .collect::<Result<_>>()?;

    let t = store.table(table).unwrap();
    let mut updates: Vec<(u64, Vec<Value>)> = Vec::new();
    for v in &t.rows {
        if !v.visible_to_writer(wc.snapshot, wc.owner) {
            continue;
        }
        let ctx = EvalCtx::row(&v.values, schema, wc.params);
        if !predicate(filter, &ctx)? {
            continue;
        }
        check_no_conflict(v, wc.snapshot, wc.owner)?;
        let mut nv = v.values.clone();
        for (idx, expr) in &targets {
            nv[*idx] = schema.columns[*idx].ty.coerce(eval(expr, &ctx)?);
        }
        check_not_null(schema, &nv, table)?;
        check_vector_dims(schema, &nv, table)?;
        check_row_constraints(&check_exprs, schema, &nv)?;
        updates.push((v.vid, nv));
    }
    Ok(updates)
}

/// Enforce PRIMARY KEY and secondary UNIQUE for the rows an UPDATE rewrites
/// (against other rows, concurrent pending inserts, and within the statement).
fn check_update_uniques(
    store: &Store,
    schema: &TableSchema,
    table: &str,
    updates: &[(u64, Vec<Value>)],
    owner: u64,
) -> Result<()> {
    let updated_vids: Vec<u64> = updates.iter().map(|(vid, _)| *vid).collect();
    let pk_cols = schema.primary_key_indices();
    if !pk_cols.is_empty() {
        let (mut committed, pending) = pk_keys(store, table, &pk_cols, owner, &updated_vids);
        let pk_name = pk_names(schema, &pk_cols);
        for (_, nv) in updates {
            let key = composite_key(nv, &pk_cols);
            if pending.contains(&key) {
                return Err(EngineError::new(
                    crate::error::EngineStatus::ErrConflict,
                    format!(
                        "write conflict: {table}.{pk_name} is being written by a concurrent transaction"
                    ),
                ));
            }
            if !committed.insert(key) {
                return Err(EngineError::constraint(format!(
                    "UNIQUE constraint failed: {table}.{pk_name}"
                )));
            }
        }
    }
    let new_rows: Vec<&[Value]> = updates.iter().map(|(_, nv)| nv.as_slice()).collect();
    check_secondary_uniques(store, table, schema, &new_rows, &updated_vids, owner)
}

/// Apply the computed UPDATE rows: supersede each old version, append the new
/// one, maintain any indexes, and project RETURNING.
fn apply_updates(
    store: &mut Store,
    schema: &TableSchema,
    table: &str,
    updates: Vec<(u64, Vec<Value>)>,
    returning: Option<&[SelItem]>,
    wc: &WriteCtx,
) -> Result<Mutation> {
    let maintain = store.table_has_index(table);
    let n = updates.len() as i64;
    let mut wal = Vec::new();
    let mut new_index_rows: Vec<(u64, Vec<Value>)> = Vec::new();
    let mut affected: Vec<Vec<Value>> = Vec::new();
    for (old_vid, nv) in updates {
        supersede(store, table, old_vid, wc.owner, &mut wal);
        let new_vid = append_version(store, table, &nv, wc.owner, &mut wal);
        if maintain {
            new_index_rows.push((new_vid, nv.clone()));
        }
        affected.push(nv);
    }
    for (vid, vals) in &new_index_rows {
        store.index_row_inserted(table, *vid, vals);
    }
    let result = match returning {
        Some(items) => Some(project_returning(items, schema, &affected, wc.params)?),
        None => None,
    };
    Ok(Mutation {
        wal,
        changes: n,
        result,
    })
}

/// PK keys that would clash with a new value, split into two buckets:
///
/// * `committed` — keys of rows live in the latest committed state (checked at
///   `committed_lsn`, not the possibly-stale txn snapshot, so a value committed
///   by another transaction after we began is still seen) plus our own pending
///   inserts. A clash here is a hard `UNIQUE` violation.
/// * `pending` — keys another *concurrent* in-flight writer is inserting. A
///   clash here is a (retryable) conflict, since the key may be free on retry.
///
/// Rows whose vid is in `exclude` (e.g. the rows an UPDATE is rewriting) are
/// skipped.
fn pk_keys(
    store: &Store,
    table: &str,
    pk: &[usize],
    me: u64,
    exclude: &[u64],
) -> (HashSet<String>, HashSet<String>) {
    let mut committed = HashSet::new();
    let mut pending = HashSet::new();
    if let Some(t) = store.table(table) {
        for v in &t.rows {
            if exclude.contains(&v.vid) {
                continue;
            }
            if v.visible_to_writer(store.committed_lsn, me) {
                committed.insert(composite_key(&v.values, pk));
            } else if v.create_lsn == PENDING && v.owner != me && v.delete_lsn == 0 {
                // A live pending insert owned by a concurrent in-flight writer.
                pending.insert(composite_key(&v.values, pk));
            }
        }
    }
    (committed, pending)
}

/// First-committer-wins, extended for concurrent in-flight writers. A row this
/// writer means to modify must not (a) already carry a committed supersede newer
/// than our snapshot — another transaction committed a change to it after we
/// began — or (b) carry a pending modification owned by a different concurrent
/// writer. Either aborts with `ENGINE_ERR_CONFLICT` so callers retry on a fresh
/// snapshot (the second case makes same-row writes first-toucher-wins, never a
/// lost update).
fn check_no_conflict(v: &RowVersion, snapshot: u64, me: u64) -> Result<()> {
    let committed_conflict =
        v.delete_lsn != 0 && v.delete_lsn != PENDING && v.delete_lsn > snapshot;
    let pending_conflict = v.delete_lsn == PENDING && v.owner != me;
    if committed_conflict || pending_conflict {
        return Err(EngineError::new(
            crate::error::EngineStatus::ErrConflict,
            "write conflict: row modified by a concurrent transaction",
        ));
    }
    Ok(())
}

fn value_key(v: &Value) -> String {
    match v {
        Value::Null => "\0null".to_string(),
        Value::Int(i) => format!("i{i}"),
        Value::Real(r) => format!("r{}", r.to_bits()),
        Value::Text(s) => format!("t{s}"),
        Value::Blob(b) => format!("b{}", crate::value::base64_encode(b)),
        Value::Vector(_) => format!("v{}", crate::value::format_vector(v.as_vector().unwrap())),
    }
}

/// The relational (multi-source) executor: joins, derived tables, CTEs, set
/// operations, `DISTINCT`, and non-correlated subqueries. It materializes the
/// `FROM` into a single [`Relation`] with a column namespace, then runs the same
/// filter / group / project / order pipeline the single-table path does, but
/// resolving column references against that namespace (spec 16 §6B). A nested
/// module so it can reuse the parent's private evaluator helpers directly.
mod relational {
    use super::*;
    use std::collections::HashMap;

    /// A materialized intermediate: column metadata + value rows.
    struct Relation {
        cols: Vec<RelCol>,
        rows: Vec<Vec<Value>>,
    }

    /// Run a full query: materialize CTEs, evaluate the leading core, fold in any
    /// set-operation arms, then apply the outer `ORDER BY`/`LIMIT`/`OFFSET`.
    pub(super) fn run_query(
        store: &Store,
        sel: &SelectStmt,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<ResultSet> {
        let mut ctes: HashMap<String, Relation> = HashMap::new();
        for cte in &sel.with {
            let rs = run_query(store, &cte.query, snapshot, writer, params)?;
            ctes.insert(
                cte.name.to_ascii_lowercase(),
                relation_from_result(&cte.name, rs),
            );
        }

        if sel.set_ops.is_empty() {
            return eval_core(store, sel, snapshot, writer, params, &ctes, true);
        }

        // Set-op chain: each core without its tail, combined, then ordered/limited.
        let mut acc = eval_core(store, sel, snapshot, writer, params, &ctes, false)?;
        for part in &sel.set_ops {
            let rhs = eval_core(store, &part.query, snapshot, writer, params, &ctes, true)?;
            acc = combine_set_op(acc, rhs, part.op, part.all)?;
        }
        apply_output_order_limit(&mut acc, sel, params)?;
        Ok(acc)
    }

    fn relation_from_result(alias: &str, rs: ResultSet) -> Relation {
        let cols = rs
            .columns
            .iter()
            .zip(&rs.types)
            .map(|(n, t)| RelCol {
                table: Some(alias.to_string()),
                name: n.clone(),
                ty: *t,
            })
            .collect();
        Relation {
            cols,
            rows: rs.rows,
        }
    }

    /// Evaluate one select core (from/where/group/having/projection/distinct).
    /// `apply_tail` controls whether this core's own ORDER BY/LIMIT/OFFSET apply
    /// (false for the leading arm of a set-op chain — the outer clause wins).
    fn eval_core(
        store: &Store,
        sel: &SelectStmt,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
        ctes: &HashMap<String, Relation>,
        apply_tail: bool,
    ) -> Result<ResultSet> {
        let rel = build_from(store, sel.from.as_ref(), snapshot, writer, params, ctes)?;
        let cols = &rel.cols;

        // Fold non-correlated subqueries to literals before evaluating.
        let filter = resolve_opt(&sel.filter, store, snapshot, writer, params)?;
        let items = resolve_items(&sel.items, store, snapshot, writer, params)?;
        let group_by = resolve_list(&sel.group_by, store, snapshot, writer, params)?;
        let having = resolve_opt(&sel.having, store, snapshot, writer, params)?;

        // WHERE.
        let mut kept: Vec<usize> = Vec::new();
        for (i, row) in rel.rows.iter().enumerate() {
            let ctx = EvalCtx::rel(row, cols, params);
            if predicate(&filter, &ctx)? {
                kept.push(i);
            }
        }

        // Output columns + types.
        let (columns, types) = output_columns(&items, cols);

        // Build (output row, group member indices) entries.
        let aggregated = agg_query(&items, &group_by, &having);
        let entries = if aggregated {
            grouped_entries(&items, &group_by, &having, cols, &rel.rows, &kept, params)?
        } else {
            let mut e = Vec::with_capacity(kept.len());
            for &i in &kept {
                let ctx = EvalCtx::rel(&rel.rows[i], cols, params);
                let mut out = Vec::new();
                for item in &items {
                    project_item(item, &ctx, cols, &rel.rows[i], &mut out)?;
                }
                e.push((out, vec![i]));
            }
            e
        };

        // ORDER BY (apply_tail) over the group-aware context, then DISTINCT, then
        // OFFSET/LIMIT.
        let order_keys = if apply_tail {
            sel.order_by
                .iter()
                .map(|k| OrderKey {
                    expr: resolve_alias_rel(&items, &k.expr),
                    asc: k.asc,
                    nulls_first: k.nulls_first,
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut rows = order_and_collect(entries, &order_keys, cols, &rel.rows, params)?;

        if sel.distinct {
            dedup_rows(&mut rows);
        }
        if apply_tail {
            let off = eval_count(&sel.offset, params)?;
            let lim = eval_count(&sel.limit, params)?;
            apply_offset_limit_owned(&mut rows, lim, off);
        }
        Ok(ResultSet {
            columns,
            types,
            rows,
        })
    }

    /// Materialize a `FROM` clause into a single relation.
    fn build_from(
        store: &Store,
        from: Option<&FromClause>,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
        ctes: &HashMap<String, Relation>,
    ) -> Result<Relation> {
        match from {
            // No FROM → a single empty row, so constant projections still produce
            // one output row.
            None => Ok(Relation {
                cols: Vec::new(),
                rows: vec![Vec::new()],
            }),
            Some(FromClause::Table { name, alias }) => {
                let src = alias.clone().unwrap_or_else(|| name.clone());
                if let Some(cte) = ctes.get(&name.to_ascii_lowercase()) {
                    let cols = retag(&cte.cols, &src);
                    return Ok(Relation {
                        cols,
                        rows: cte.rows.clone(),
                    });
                }
                let table = store
                    .table(name)
                    .ok_or_else(|| EngineError::sql(format!("no such table: {name}")))?;
                let cols = table
                    .schema
                    .columns
                    .iter()
                    .map(|c| RelCol {
                        table: Some(src.clone()),
                        name: c.name.clone(),
                        ty: c.ty,
                    })
                    .collect();
                let rows = table
                    .rows
                    .iter()
                    .filter(|v| row_visible(v, snapshot, writer))
                    .map(|v| v.values.clone())
                    .collect();
                Ok(Relation { cols, rows })
            }
            Some(FromClause::Derived { query, alias }) => {
                let rs = run_query(store, query, snapshot, writer, params)?;
                Ok(relation_from_result(alias, rs))
            }
            Some(FromClause::Join {
                left,
                right,
                kind,
                on,
                using,
            }) => {
                let l = build_from(store, Some(left), snapshot, writer, params, ctes)?;
                let r = build_from(store, Some(right), snapshot, writer, params, ctes)?;
                join(l, r, *kind, on.as_deref(), using, params)
            }
        }
    }

    fn retag(cols: &[RelCol], src: &str) -> Vec<RelCol> {
        cols.iter()
            .map(|c| RelCol {
                table: Some(src.to_string()),
                name: c.name.clone(),
                ty: c.ty,
            })
            .collect()
    }

    /// Nested-loop join of two relations.
    fn join(
        l: Relation,
        r: Relation,
        kind: JoinKind,
        on: Option<&Expr>,
        using: &[String],
        params: &[Value],
    ) -> Result<Relation> {
        let mut cols = l.cols.clone();
        cols.extend(r.cols.clone());
        let lw = l.cols.len();
        let rw = r.cols.len();
        let pairs: Vec<(usize, usize)> = using
            .iter()
            .map(|u| {
                Ok((
                    resolve_col(&l.cols, None, u)?,
                    lw + resolve_col(&r.cols, None, u)?,
                ))
            })
            .collect::<Result<_>>()?;

        let null_l = vec![Value::Null; lw];
        let null_r = vec![Value::Null; rw];
        let jc = JoinCtx {
            cols: &cols,
            on,
            pairs: &pairs,
            params,
            swapped: false,
        };
        let out = match kind {
            JoinKind::Inner | JoinKind::Cross => join_inner(&l, &r, &jc)?,
            JoinKind::Left => join_outer(&l, &r, &jc, &null_r)?,
            // Right join is left join with the operands swapped; the combined row
            // order (`[l | r]`) is preserved inside [`join_outer`].
            JoinKind::Right => join_outer(&r, &l, &jc.swapped(), &null_l)?,
            JoinKind::Full => join_full(&l, &r, &jc, &null_l, &null_r)?,
        };
        Ok(Relation { cols, rows: out })
    }

    /// The join predicate context shared by the join-kind helpers: the combined
    /// namespace, the `ON` predicate, the `USING` equality pairs, and params.
    struct JoinCtx<'a> {
        cols: &'a [RelCol],
        on: Option<&'a Expr>,
        pairs: &'a [(usize, usize)],
        params: &'a [Value],
        /// When set, the outer/inner operands are swapped (right join); the
        /// combined row is then built as `[inner | outer]` to keep column order.
        swapped: bool,
    }

    impl<'a> JoinCtx<'a> {
        fn swapped(&self) -> JoinCtx<'a> {
            JoinCtx {
                cols: self.cols,
                on: self.on,
                pairs: self.pairs,
                params: self.params,
                swapped: true,
            }
        }
        /// Build the combined `[l | r]` row honoring the swap flag, where `outer`
        /// is whichever side the caller iterates on the outside of the loop.
        fn combine(&self, outer: &[Value], inner: &[Value]) -> Vec<Value> {
            if self.swapped {
                concat(inner, outer)
            } else {
                concat(outer, inner)
            }
        }
    }

    fn join_inner(l: &Relation, r: &Relation, jc: &JoinCtx) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::new();
        for lr in &l.rows {
            for rr in &r.rows {
                let c = concat(lr, rr);
                if row_matches(&c, jc.cols, jc.on, jc.pairs, jc.params)? {
                    out.push(c);
                }
            }
        }
        Ok(out)
    }

    /// A left outer join of `outer` against `inner` (also serving right joins via
    /// [`JoinCtx::swapped`]); unmatched `outer` rows are padded with `null_inner`.
    fn join_outer(
        outer: &Relation,
        inner: &Relation,
        jc: &JoinCtx,
        null_inner: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::new();
        for orow in &outer.rows {
            let mut any = false;
            for irow in &inner.rows {
                let c = jc.combine(orow, irow);
                if row_matches(&c, jc.cols, jc.on, jc.pairs, jc.params)? {
                    out.push(c);
                    any = true;
                }
            }
            if !any {
                out.push(jc.combine(orow, null_inner));
            }
        }
        Ok(out)
    }

    fn join_full(
        l: &Relation,
        r: &Relation,
        jc: &JoinCtx,
        null_l: &[Value],
        null_r: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let mut out = Vec::new();
        let mut r_hit = vec![false; r.rows.len()];
        for lr in &l.rows {
            let mut any = false;
            for (j, rr) in r.rows.iter().enumerate() {
                let c = concat(lr, rr);
                if row_matches(&c, jc.cols, jc.on, jc.pairs, jc.params)? {
                    out.push(c);
                    any = true;
                    r_hit[j] = true;
                }
            }
            if !any {
                out.push(concat(lr, null_r));
            }
        }
        for (j, rr) in r.rows.iter().enumerate() {
            if !r_hit[j] {
                out.push(concat(null_l, rr));
            }
        }
        Ok(out)
    }

    fn concat(a: &[Value], b: &[Value]) -> Vec<Value> {
        let mut c = Vec::with_capacity(a.len() + b.len());
        c.extend_from_slice(a);
        c.extend_from_slice(b);
        c
    }

    /// Whether a combined row satisfies the join condition (USING equalities and
    /// any ON predicate).
    fn row_matches(
        combined: &[Value],
        cols: &[RelCol],
        on: Option<&Expr>,
        pairs: &[(usize, usize)],
        params: &[Value],
    ) -> Result<bool> {
        for &(li, ri) in pairs {
            if combined[li].sql_eq(&combined[ri]) != Some(true) {
                return Ok(false);
            }
        }
        match on {
            Some(e) => {
                let ctx = EvalCtx::rel(combined, cols, params);
                Ok(eval(e, &ctx)?.as_bool().unwrap_or(false))
            }
            None => Ok(true),
        }
    }

    /// Output column names + best-effort types from the select list.
    fn output_columns(items: &[SelItem], cols: &[RelCol]) -> (Vec<String>, Vec<ColumnType>) {
        let mut columns = Vec::new();
        let mut types = Vec::new();
        for item in items {
            match item {
                SelItem::Star { qualifier } => {
                    for c in cols {
                        if star_includes(qualifier, c) {
                            columns.push(c.name.clone());
                            types.push(c.ty);
                        }
                    }
                }
                SelItem::Expr { expr, alias } => {
                    columns.push(column_name(expr, alias, columns.len()));
                    types.push(expr_type_rel(expr, cols));
                }
            }
        }
        (columns, types)
    }

    fn star_includes(qualifier: &Option<String>, c: &RelCol) -> bool {
        match qualifier {
            None => true,
            Some(q) => c
                .table
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(q)),
        }
    }

    /// Project one select item into `out` for a single (non-grouped) row.
    fn project_item(
        item: &SelItem,
        ctx: &EvalCtx,
        cols: &[RelCol],
        row: &[Value],
        out: &mut Vec<Value>,
    ) -> Result<()> {
        match item {
            SelItem::Star { qualifier } => {
                for (i, c) in cols.iter().enumerate() {
                    if star_includes(qualifier, c) {
                        out.push(row[i].clone());
                    }
                }
            }
            SelItem::Expr { expr, .. } => out.push(eval(expr, ctx)?),
        }
        Ok(())
    }

    /// Group the kept rows and produce (output row, member indices) per surviving
    /// group (after `HAVING`).
    fn grouped_entries(
        items: &[SelItem],
        group_by: &[Expr],
        having: &Option<Expr>,
        cols: &[RelCol],
        rows: &[Vec<Value>],
        kept: &[usize],
        params: &[Value],
    ) -> Result<Vec<(Vec<Value>, Vec<usize>)>> {
        let mut order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in kept {
            let ctx = EvalCtx::rel(&rows[i], cols, params);
            let mut key = String::new();
            for e in group_by {
                key.push_str(&value_key(&eval(e, &ctx)?));
                key.push('\u{1}');
            }
            groups.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                Vec::new()
            });
            groups.get_mut(&key).unwrap().push(i);
        }
        // With no GROUP BY, an empty input still yields one all-rows group.
        if group_by.is_empty() && order.is_empty() {
            order.push(String::new());
            groups.insert(String::new(), Vec::new());
        }

        let mut entries = Vec::new();
        for key in &order {
            let members = groups.remove(key).unwrap();
            let view: Vec<&[Value]> = members.iter().map(|&i| rows[i].as_slice()).collect();
            let ctx = EvalCtx {
                row: view.first().copied(),
                schema: None,
                params,
                group: Some(&view),
                excluded: None,
                cols: Some(cols),
            };
            if let Some(h) = having {
                if !eval(h, &ctx)?.as_bool().unwrap_or(false) {
                    continue;
                }
            }
            let mut out = Vec::new();
            for item in items {
                match item {
                    SelItem::Star { qualifier } => {
                        let r = ctx
                            .row
                            .ok_or_else(|| EngineError::sql("SELECT * over an empty group"))?;
                        for (i, c) in cols.iter().enumerate() {
                            if star_includes(qualifier, c) {
                                out.push(r[i].clone());
                            }
                        }
                    }
                    SelItem::Expr { expr, .. } => out.push(eval(expr, &ctx)?),
                }
            }
            entries.push((out, members));
        }
        Ok(entries)
    }

    /// Sort entries by the order keys (evaluated in each entry's group-aware
    /// context) and return just the output rows.
    fn order_and_collect(
        mut entries: Vec<(Vec<Value>, Vec<usize>)>,
        keys: &[OrderKey],
        cols: &[RelCol],
        rows: &[Vec<Value>],
        params: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        if !keys.is_empty() {
            let mut err: Option<EngineError> = None;
            entries.sort_by(|a, b| {
                if err.is_some() {
                    return Ordering::Equal;
                }
                for key in keys {
                    let va = eval_in_group(&key.expr, &a.1, cols, rows, params);
                    let vb = eval_in_group(&key.expr, &b.1, cols, rows, params);
                    let (va, vb) = match (va, vb) {
                        (Ok(x), Ok(y)) => (x, y),
                        (Err(e), _) | (_, Err(e)) => {
                            err = Some(e);
                            return Ordering::Equal;
                        }
                    };
                    let ord = order_cmp(&va, &vb, key.asc, key.nulls_first);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
        Ok(entries.into_iter().map(|(o, _)| o).collect())
    }

    fn eval_in_group(
        expr: &Expr,
        members: &[usize],
        cols: &[RelCol],
        rows: &[Vec<Value>],
        params: &[Value],
    ) -> Result<Value> {
        let view: Vec<&[Value]> = members.iter().map(|&i| rows[i].as_slice()).collect();
        let ctx = EvalCtx {
            row: view.first().copied(),
            schema: None,
            params,
            group: Some(&view),
            excluded: None,
            cols: Some(cols),
        };
        eval(expr, &ctx)
    }

    /// Resolve an ORDER BY key that names an output alias to that item's expr.
    fn resolve_alias_rel(items: &[SelItem], expr: &Expr) -> Expr {
        let name = match expr {
            Expr::Column(n) => Some(n.as_str()),
            _ => None,
        };
        if let Some(name) = name {
            for item in items {
                if let SelItem::Expr {
                    expr: e,
                    alias: Some(a),
                } = item
                {
                    if a.eq_ignore_ascii_case(name) {
                        return e.clone();
                    }
                }
            }
        }
        expr.clone()
    }

    fn agg_query(items: &[SelItem], group_by: &[Expr], having: &Option<Expr>) -> bool {
        !group_by.is_empty()
            || having.is_some()
            || items
                .iter()
                .any(|i| matches!(i, SelItem::Expr { expr, .. } if expr_has_aggregate(expr)))
    }

    /// Best-effort projected-expression type against the relation namespace.
    fn expr_type_rel(expr: &Expr, cols: &[RelCol]) -> ColumnType {
        match expr {
            Expr::Column(n) => rel_type(cols, None, n).unwrap_or(ColumnType::Text),
            Expr::Qualified(t, n) => rel_type(cols, Some(t), n).unwrap_or(ColumnType::Text),
            Expr::Cast { e, target } => match target {
                CastTarget::Passthrough => expr_type_rel(e, cols),
                other => cast_column_type(*other),
            },
            Expr::Aggregate { func, arg, .. } => match func {
                AggFunc::Count => ColumnType::Integer,
                AggFunc::Avg => ColumnType::Real,
                AggFunc::JsonAgg | AggFunc::GroupConcat => ColumnType::Text,
                AggFunc::Sum | AggFunc::Min | AggFunc::Max => match arg {
                    AggArg::Expr(e) => expr_type_rel(e, cols),
                    AggArg::Star => ColumnType::Integer,
                },
            },
            other => expr_type(other, None),
        }
    }

    fn rel_type(cols: &[RelCol], table: Option<&str>, name: &str) -> Option<ColumnType> {
        resolve_col(cols, table, name).ok().map(|i| cols[i].ty)
    }

    fn dedup_rows(rows: &mut Vec<Vec<Value>>) {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        rows.retain(|r| seen.insert(row_key(r)));
    }

    fn row_key(row: &[Value]) -> String {
        let mut k = String::new();
        for v in row {
            k.push_str(&value_key(v));
            k.push('\u{1}');
        }
        k
    }

    fn apply_offset_limit_owned(
        rows: &mut Vec<Vec<Value>>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) {
        if let Some(off) = offset {
            let off = (off.max(0) as usize).min(rows.len());
            rows.drain(..off);
        }
        if let Some(limit) = limit {
            rows.truncate(limit.max(0) as usize);
        }
    }

    // ---- set operations ----------------------------------------------------

    fn combine_set_op(lhs: ResultSet, rhs: ResultSet, op: SetOp, all: bool) -> Result<ResultSet> {
        if lhs.columns.len() != rhs.columns.len() {
            return Err(EngineError::sql(
                "each set-operation query must return the same number of columns",
            ));
        }
        let columns = lhs.columns;
        let types = lhs.types;
        let l = lhs.rows;
        let r = rhs.rows;
        let rows = match op {
            SetOp::Union => {
                let mut out = l;
                out.extend(r);
                if !all {
                    dedup_rows(&mut out);
                }
                out
            }
            SetOp::Intersect => {
                let mut out = Vec::new();
                if all {
                    let mut counts: HashMap<String, usize> = HashMap::new();
                    for row in &r {
                        *counts.entry(row_key(row)).or_default() += 1;
                    }
                    for row in l {
                        let k = row_key(&row);
                        if let Some(c) = counts.get_mut(&k) {
                            if *c > 0 {
                                *c -= 1;
                                out.push(row);
                            }
                        }
                    }
                } else {
                    let rset: std::collections::HashSet<String> =
                        r.iter().map(|x| row_key(x)).collect();
                    let mut seen = std::collections::HashSet::new();
                    for row in l {
                        let k = row_key(&row);
                        if rset.contains(&k) && seen.insert(k) {
                            out.push(row);
                        }
                    }
                }
                out
            }
            SetOp::Except => {
                let mut out = Vec::new();
                if all {
                    let mut counts: HashMap<String, usize> = HashMap::new();
                    for row in &r {
                        *counts.entry(row_key(row)).or_default() += 1;
                    }
                    for row in l {
                        let k = row_key(&row);
                        match counts.get_mut(&k) {
                            Some(c) if *c > 0 => *c -= 1,
                            _ => out.push(row),
                        }
                    }
                } else {
                    let rset: std::collections::HashSet<String> =
                        r.iter().map(|x| row_key(x)).collect();
                    let mut seen = std::collections::HashSet::new();
                    for row in l {
                        let k = row_key(&row);
                        if !rset.contains(&k) && seen.insert(k) {
                            out.push(row);
                        }
                    }
                }
                out
            }
        };
        Ok(ResultSet {
            columns,
            types,
            rows,
        })
    }

    /// Apply the outer ORDER BY / LIMIT / OFFSET to a set-operation result. Order
    /// keys must reference an output column (by name or 1-based ordinal).
    fn apply_output_order_limit(
        rs: &mut ResultSet,
        sel: &SelectStmt,
        params: &[Value],
    ) -> Result<()> {
        if !sel.order_by.is_empty() {
            let mut keys: Vec<(usize, bool, bool)> = Vec::new();
            for k in &sel.order_by {
                let idx = match &k.expr {
                    Expr::Int(n) => (*n as usize)
                        .checked_sub(1)
                        .filter(|i| *i < rs.columns.len())
                        .ok_or_else(|| EngineError::sql("ORDER BY position out of range"))?,
                    Expr::Column(name) | Expr::Qualified(_, name) => rs
                        .columns
                        .iter()
                        .position(|c| c.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            EngineError::sql(format!("ORDER BY column not in result: {name}"))
                        })?,
                    _ => {
                        return Err(EngineError::sql(
                            "ORDER BY in a set operation must name an output column",
                        ))
                    }
                };
                keys.push((idx, k.asc, k.nulls_first));
            }
            rs.rows.sort_by(|a, b| {
                for &(i, asc, nf) in &keys {
                    let ord = order_cmp(&a[i], &b[i], asc, nf);
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                Ordering::Equal
            });
        }
        let off = eval_count(&sel.offset, params)?;
        let lim = eval_count(&sel.limit, params)?;
        apply_offset_limit_owned(&mut rs.rows, lim, off);
        Ok(())
    }

    // ---- subquery folding (non-correlated) ---------------------------------

    fn resolve_opt(
        e: &Option<Expr>,
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<Option<Expr>> {
        match e {
            Some(e) => Ok(Some(resolve_sub(e, store, snapshot, writer, params)?)),
            None => Ok(None),
        }
    }

    fn resolve_list(
        list: &[Expr],
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<Vec<Expr>> {
        list.iter()
            .map(|e| resolve_sub(e, store, snapshot, writer, params))
            .collect()
    }

    fn resolve_items(
        items: &[SelItem],
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<Vec<SelItem>> {
        items
            .iter()
            .map(|i| match i {
                SelItem::Star { qualifier } => Ok(SelItem::Star {
                    qualifier: qualifier.clone(),
                }),
                SelItem::Expr { expr, alias } => Ok(SelItem::Expr {
                    expr: resolve_sub(expr, store, snapshot, writer, params)?,
                    alias: alias.clone(),
                }),
            })
            .collect()
    }

    /// Replace non-correlated subquery nodes with pre-computed literals. A
    /// subquery that references an outer column fails to resolve here and is
    /// surfaced as an error (correlated subqueries are out of scope). Outer CTEs
    /// are not visible to expression subqueries — only to `FROM` references.
    fn resolve_sub(
        e: &Expr,
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<Expr> {
        match e {
            Expr::ScalarSubquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                resolve_subquery_node(e, store, snapshot, writer, params)
            }
            _ => resolve_children(e, store, snapshot, writer, params),
        }
    }

    /// Fold a top-level subquery node (scalar / `EXISTS` / `IN`) into a literal or
    /// literal list by running it once at this snapshot (non-correlated only).
    fn resolve_subquery_node(
        e: &Expr,
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<Expr> {
        Ok(match e {
            Expr::ScalarSubquery(q) => {
                let rs = run_query(store, q, snapshot, writer, params)?;
                if rs.columns.len() != 1 {
                    return Err(EngineError::sql(
                        "a scalar subquery must return exactly one column",
                    ));
                }
                match rs.rows.len() {
                    0 => Expr::Lit(Value::Null),
                    1 => Expr::Lit(rs.rows.into_iter().next().unwrap().pop().unwrap()),
                    _ => {
                        return Err(EngineError::sql(
                            "more than one row returned by a scalar subquery",
                        ))
                    }
                }
            }
            Expr::Exists { query, negated } => {
                let rs = run_query(store, query, snapshot, writer, params)?;
                let present = !rs.rows.is_empty();
                Expr::Lit(Value::Int((present ^ negated) as i64))
            }
            Expr::InSubquery { e, query, negated } => {
                let rs = run_query(store, query, snapshot, writer, params)?;
                if rs.columns.len() != 1 {
                    return Err(EngineError::sql(
                        "a subquery used with IN must return exactly one column",
                    ));
                }
                let list = rs
                    .rows
                    .into_iter()
                    .map(|mut r| Expr::Lit(r.pop().unwrap()))
                    .collect();
                Expr::InList {
                    e: Box::new(resolve_sub(e, store, snapshot, writer, params)?),
                    list,
                    negated: *negated,
                }
            }
            _ => unreachable!("resolve_subquery_node called on a non-subquery node"),
        })
    }

    /// Recurse subquery folding into a composite expression's children; leaves
    /// clone unchanged. The `Option`/`AggArg` children go through the
    /// [`resolve_box_opt`]/[`resolve_agg_arg`] helpers so this match stays flat.
    fn resolve_children(
        e: &Expr,
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<Expr> {
        let rec = |x: &Expr| -> Result<Expr> { resolve_sub(x, store, snapshot, writer, params) };
        let bx = |x: &Expr| -> Result<Box<Expr>> { Ok(Box::new(rec(x)?)) };
        let opt = |x: &Option<Box<Expr>>| resolve_box_opt(x, store, snapshot, writer, params);
        Ok(match e {
            Expr::Binary { op, l, r } => Expr::Binary {
                op: *op,
                l: bx(l)?,
                r: bx(r)?,
            },
            Expr::Unary { op, e } => Expr::Unary { op: *op, e: bx(e)? },
            Expr::IsNull { e, negated } => Expr::IsNull {
                e: bx(e)?,
                negated: *negated,
            },
            Expr::Like {
                e,
                pattern,
                escape,
                negated,
                insensitive,
            } => Expr::Like {
                e: bx(e)?,
                pattern: bx(pattern)?,
                escape: opt(escape)?,
                negated: *negated,
                insensitive: *insensitive,
            },
            Expr::InList { e, list, negated } => Expr::InList {
                e: bx(e)?,
                list: list.iter().map(&rec).collect::<Result<_>>()?,
                negated: *negated,
            },
            Expr::Between { e, lo, hi, negated } => Expr::Between {
                e: bx(e)?,
                lo: bx(lo)?,
                hi: bx(hi)?,
                negated: *negated,
            },
            Expr::Case {
                operand,
                whens,
                els,
            } => Expr::Case {
                operand: opt(operand)?,
                whens: whens
                    .iter()
                    .map(|(c, r)| Ok((rec(c)?, rec(r)?)))
                    .collect::<Result<_>>()?,
                els: opt(els)?,
            },
            Expr::Cast { e, target } => Expr::Cast {
                e: bx(e)?,
                target: *target,
            },
            Expr::Func { name, args } => Expr::Func {
                name: name.clone(),
                args: args.iter().map(&rec).collect::<Result<_>>()?,
            },
            Expr::Aggregate {
                func,
                arg,
                distinct,
                sep,
            } => Expr::Aggregate {
                func: *func,
                arg: resolve_agg_arg(arg, store, snapshot, writer, params)?,
                distinct: *distinct,
                sep: opt(sep)?,
            },
            other => other.clone(),
        })
    }

    /// Fold subqueries inside an optional boxed child (`None` stays `None`).
    fn resolve_box_opt(
        x: &Option<Box<Expr>>,
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<Option<Box<Expr>>> {
        match x {
            Some(e) => Ok(Some(Box::new(resolve_sub(
                e, store, snapshot, writer, params,
            )?))),
            None => Ok(None),
        }
    }

    /// Fold subqueries inside an aggregate argument (`*` has none to fold).
    fn resolve_agg_arg(
        arg: &AggArg,
        store: &Store,
        snapshot: u64,
        writer: Option<u64>,
        params: &[Value],
    ) -> Result<AggArg> {
        Ok(match arg {
            AggArg::Star => AggArg::Star,
            AggArg::Expr(e) => {
                AggArg::Expr(Box::new(resolve_sub(e, store, snapshot, writer, params)?))
            }
        })
    }
}
