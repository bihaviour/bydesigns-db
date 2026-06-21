//! Executor: evaluates expressions and runs statements against the MVCC store.
//!
//! Reads acquire an MVCC snapshot and filter row versions by visibility; writes
//! validate fully *before* mutating so each statement is atomic (a mid-statement
//! constraint failure leaves the store untouched). Mutations stamp new versions
//! `PENDING` and emit [`WalOp`]s; durability and publish-at-commit-LSN are the
//! transaction manager's job (see [`crate::conn`]).

use crate::catalog::TableSchema;
use crate::error::{EngineError, Result};
use crate::sql::{AggArg, AggFunc, BinOp, CastTarget, Expr, SelItem, SelectStmt, UnOp};
use crate::store::{RowVersion, Store, PENDING};
use crate::value::{parse_vector, ColumnType, Value};
use crate::vector::{distance, Metric};
use crate::wal::WalOp;
use std::cmp::Ordering;
use std::collections::HashSet;

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
}

// ---- expression evaluation ------------------------------------------------

fn eval(e: &Expr, ctx: &EvalCtx) -> Result<Value> {
    match e {
        Expr::Null => Ok(Value::Null),
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Real(r) => Ok(Value::Real(*r)),
        Expr::Str(s) => Ok(Value::Text(s.clone())),
        Expr::Vector(v) => Ok(Value::Vector(v.clone())),
        Expr::Param(idx) => ctx
            .params
            .get(idx - 1)
            .cloned()
            .ok_or_else(|| EngineError::misuse(format!("missing bound parameter ?{idx}"))),
        Expr::Column(name) => {
            let schema = ctx.schema.ok_or_else(|| {
                EngineError::sql(format!("column {name} referenced with no table"))
            })?;
            let row = ctx
                .row
                .ok_or_else(|| EngineError::sql(format!("column {name} referenced with no row")))?;
            let idx = schema
                .column_index(name)
                .ok_or_else(|| EngineError::sql(format!("no such column: {name}")))?;
            Ok(row[idx].clone())
        }
        Expr::Unary { op, e } => {
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
        Expr::IsNull { e, negated } => {
            let v = eval(e, ctx)?;
            let is_null = v.is_null();
            Ok(Value::Int((is_null ^ negated) as i64))
        }
        Expr::Like {
            e,
            pattern,
            negated,
        } => {
            let v = eval(e, ctx)?;
            let p = eval(pattern, ctx)?;
            match (v, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(s), Value::Text(pat)) => {
                    let m = like_match(&s, &pat);
                    Ok(Value::Int((m ^ negated) as i64))
                }
                _ => Ok(Value::Null),
            }
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
    }
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
        VecL2 | VecCosine | VecIp => vec_distance(op, &l, &r),
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

/// SQL `LIKE` with `%` (any run) and `_` (single char), ASCII-case-insensitive.
fn like_match(s: &str, pattern: &str) -> bool {
    let s: Vec<char> = s.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    fn go(s: &[char], p: &[char]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        match p[0] {
            '%' => {
                // collapse consecutive %, then try every split
                let mut k = 0;
                while k < p.len() && p[k] == '%' {
                    k += 1;
                }
                let rest = &p[k..];
                if rest.is_empty() {
                    return true;
                }
                (0..=s.len()).any(|i| go(&s[i..], rest))
            }
            '_' => !s.is_empty() && go(&s[1..], &p[1..]),
            c => !s.is_empty() && s[0] == c && go(&s[1..], &p[1..]),
        }
    }
    go(&s, &p)
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
    let Some(table_name) = &sel.from else {
        return constant_select(sel, params);
    };

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
        let ctx = EvalCtx {
            row: Some(&v.values),
            schema: Some(schema),
            params,
        };
        if predicate(&sel.filter, &ctx)? {
            matched.push(&v.values);
        }
    }

    let has_agg = sel
        .items
        .iter()
        .any(|i| matches!(i, SelItem::Aggregate { .. }));
    if has_agg {
        return aggregate_select(sel, schema, &matched, params);
    }

    order_matched(sel, schema, &mut matched, params)?;
    if let Some(limit) = sel.limit {
        matched.truncate(limit.max(0) as usize);
    }
    project(sel, schema, &matched, params)
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
    let keys = &sel.order_by;
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

/// Compare two rows by the ordered key list, recording the first eval error.
fn order_key_cmp(
    keys: &[(Expr, bool)],
    schema: &TableSchema,
    ra: &[Value],
    rb: &[Value],
    params: &[Value],
    err: &mut Option<EngineError>,
) -> Ordering {
    for (expr, asc) in keys {
        let ca = EvalCtx {
            row: Some(ra),
            schema: Some(schema),
            params,
        };
        let cb = EvalCtx {
            row: Some(rb),
            schema: Some(schema),
            params,
        };
        let (va, vb) = match (eval(expr, &ca), eval(expr, &cb)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => {
                *err = Some(e);
                return Ordering::Equal;
            }
        };
        let ord = null_aware_cmp(&va, &vb);
        let ord = if *asc { ord } else { ord.reverse() };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
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
            SelItem::Star => {
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
            SelItem::Aggregate { .. } => {
                return Err(EngineError::sql("aggregate not allowed in this position"))
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
                    let ctx = EvalCtx {
                        row: Some(vals),
                        schema: Some(schema),
                        params,
                    };
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
/// exists (the executor checks that next).
fn knn_plan(sel: &SelectStmt) -> Option<KnnPlan<'_>> {
    let limit = sel.limit?;
    if limit < 0 || sel.order_by.len() != 1 {
        return None;
    }
    let (order_expr, asc) = &sel.order_by[0];
    if !asc
        || sel
            .items
            .iter()
            .any(|i| matches!(i, SelItem::Aggregate { .. }))
    {
        return None;
    }
    let Expr::Binary { op, l, r } = order_expr else {
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
    let Some(plan) = knn_plan(sel) else {
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
    let cctx = EvalCtx {
        row: None,
        schema: None,
        params,
    };
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
        let ctx = EvalCtx {
            row: Some(&v.values),
            schema: Some(schema),
            params,
        };
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
    if sel.items.iter().any(|i| !matches!(i, SelItem::Expr { .. })) {
        return Err(EngineError::sql(
            "SELECT without FROM cannot use * or aggregates",
        ));
    }
    let ctx = EvalCtx {
        row: None,
        schema: None,
        params,
    };
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

fn aggregate_select(
    sel: &SelectStmt,
    schema: &TableSchema,
    matched: &[&Vec<Value>],
    params: &[Value],
) -> Result<ResultSet> {
    if !sel
        .items
        .iter()
        .all(|i| matches!(i, SelItem::Aggregate { .. }))
    {
        return Err(EngineError::sql(
            "cannot mix aggregates and plain columns without GROUP BY",
        ));
    }
    let mut columns = Vec::new();
    let mut types = Vec::new();
    let mut row = Vec::new();
    for item in &sel.items {
        if let SelItem::Aggregate {
            func,
            arg,
            cast,
            alias,
        } = item
        {
            columns.push(alias.clone().unwrap_or_else(|| agg_name(*func)));
            types.push(match cast {
                Some(t) => cast_column_type(*t),
                None => agg_type(*func, arg, schema),
            });
            let v = compute_aggregate(*func, arg, schema, matched, params)?;
            row.push(match cast {
                Some(t) => cast_value(v, *t)?,
                None => v,
            });
        }
    }
    // LIMIT 0 suppresses the single aggregate row.
    let rows = if sel.limit == Some(0) {
        vec![]
    } else {
        vec![row]
    };
    Ok(ResultSet {
        columns,
        types,
        rows,
    })
}

fn compute_aggregate(
    func: AggFunc,
    arg: &AggArg,
    schema: &TableSchema,
    matched: &[&Vec<Value>],
    params: &[Value],
) -> Result<Value> {
    if let (AggFunc::Count, AggArg::Star) = (func, arg) {
        return Ok(Value::Int(matched.len() as i64));
    }
    let AggArg::Expr(expr) = arg else {
        return Err(EngineError::sql("only COUNT(*) takes a star argument"));
    };
    // Evaluate the argument over non-null values.
    let mut vals: Vec<Value> = Vec::new();
    for r in matched {
        let ctx = EvalCtx {
            row: Some(r),
            schema: Some(schema),
            params,
        };
        let v = eval(expr, &ctx)?;
        if !v.is_null() {
            vals.push(v);
        }
    }
    Ok(match func {
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
    }
    .to_string()
}

fn column_name(expr: &Expr, alias: &Option<String>, idx: usize) -> String {
    if let Some(a) = alias {
        return a.clone();
    }
    match expr {
        Expr::Column(c) => c.clone(),
        _ => format!("col{}", idx + 1),
    }
}

// ---- writes (validate fully, then apply: atomic per statement) -------------

pub fn run_insert(
    store: &mut Store,
    table: &str,
    columns: &Option<Vec<String>>,
    rows: &[Vec<Expr>],
    owner: u64,
    params: &[Value],
) -> Result<(Vec<WalOp>, i64)> {
    let schema = store
        .table(table)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table}")))?
        .schema
        .clone();
    let ncols = schema.columns.len();
    let ctx = EvalCtx {
        row: None,
        schema: None,
        params,
    };

    let mut staged: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
    for row_exprs in rows {
        let mut vals = vec![Value::Null; ncols];
        match columns {
            Some(cols) => {
                if cols.len() != row_exprs.len() {
                    return Err(EngineError::sql("INSERT column/value count mismatch"));
                }
                for (c, e) in cols.iter().zip(row_exprs) {
                    let idx = schema
                        .column_index(c)
                        .ok_or_else(|| EngineError::sql(format!("no such column: {c}")))?;
                    vals[idx] = schema.columns[idx].ty.coerce(eval(e, &ctx)?);
                }
            }
            None => {
                if row_exprs.len() != ncols {
                    return Err(EngineError::sql(format!(
                        "table {table} has {ncols} columns but {} values supplied",
                        row_exprs.len()
                    )));
                }
                for (i, e) in row_exprs.iter().enumerate() {
                    vals[i] = schema.columns[i].ty.coerce(eval(e, &ctx)?);
                }
            }
        }
        // NOT NULL + vector dimension.
        for (i, c) in schema.columns.iter().enumerate() {
            if c.not_null && vals[i].is_null() {
                return Err(EngineError::constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    table, c.name
                )));
            }
        }
        check_vector_dims(&schema, &vals, table)?;
        staged.push(vals);
    }

    // PRIMARY KEY uniqueness (vs existing visible rows + concurrent pending
    // inserts + within this batch). A clash with another in-flight writer's
    // pending insert is reported as a conflict (retryable) rather than a hard
    // constraint failure, since on retry the key may be free again.
    if let Some(pk) = schema.primary_key_index() {
        let (mut committed, pending) = pk_keys(store, table, pk, owner, &[]);
        for vals in &staged {
            let key = value_key(&vals[pk]);
            if pending.contains(&key) {
                return Err(EngineError::new(
                    crate::error::EngineStatus::ErrConflict,
                    format!(
                        "write conflict: {}.{} is being inserted by a concurrent transaction",
                        table, schema.columns[pk].name
                    ),
                ));
            }
            if !committed.insert(key) {
                return Err(EngineError::constraint(format!(
                    "UNIQUE constraint failed: {}.{}",
                    table, schema.columns[pk].name
                )));
            }
        }
    }

    // Apply, then maintain any vector index on the table (spec 12).
    let maintain = store.table_has_index(table);
    let n = staged.len() as i64;
    let mut wal = Vec::with_capacity(staged.len());
    let mut inserted: Vec<(u64, Vec<Value>)> = Vec::new();
    {
        let t = store.table_mut(table).unwrap();
        for vals in staged {
            let vid = t.alloc_vid();
            wal.push(WalOp::Insert {
                table: table.to_string(),
                vid,
                values: vals.clone(),
            });
            if maintain {
                inserted.push((vid, vals.clone()));
            }
            t.rows.push(RowVersion {
                vid,
                values: vals,
                create_lsn: PENDING,
                delete_lsn: 0,
                owner,
            });
        }
    }
    for (vid, vals) in &inserted {
        store.index_row_inserted(table, *vid, vals);
    }
    Ok((wal, n))
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
    snapshot: u64,
    owner: u64,
    params: &[Value],
) -> Result<(Vec<WalOp>, i64)> {
    let schema = store
        .table(table)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table}")))?
        .schema
        .clone();
    let t = store.table(table).unwrap();
    let mut victims = Vec::new();
    for v in &t.rows {
        if !v.visible_to_writer(snapshot, owner) {
            continue;
        }
        let ctx = EvalCtx {
            row: Some(&v.values),
            schema: Some(&schema),
            params,
        };
        if predicate(filter, &ctx)? {
            check_no_conflict(v, snapshot, owner)?;
            victims.push(v.vid);
        }
    }
    let t = store.table_mut(table).unwrap();
    let mut wal = Vec::with_capacity(victims.len());
    for vid in &victims {
        if let Some(rv) = t.version_mut(*vid) {
            rv.delete_lsn = PENDING;
            rv.owner = owner;
        }
        wal.push(WalOp::Delete {
            table: table.to_string(),
            vid: *vid,
        });
    }
    Ok((wal, victims.len() as i64))
}

pub fn run_update(
    store: &mut Store,
    table: &str,
    sets: &[(String, Expr)],
    filter: &Option<Expr>,
    snapshot: u64,
    owner: u64,
    params: &[Value],
) -> Result<(Vec<WalOp>, i64)> {
    let schema = store
        .table(table)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table}")))?
        .schema
        .clone();

    // Resolve assignment target indices once.
    let mut targets = Vec::with_capacity(sets.len());
    for (col, expr) in sets {
        let idx = schema
            .column_index(col)
            .ok_or_else(|| EngineError::sql(format!("no such column: {col}")))?;
        targets.push((idx, expr));
    }

    // Compute (old_vid, new_values) for every matching row (no mutation yet).
    let t = store.table(table).unwrap();
    let mut updates: Vec<(u64, Vec<Value>)> = Vec::new();
    for v in &t.rows {
        if !v.visible_to_writer(snapshot, owner) {
            continue;
        }
        let ctx = EvalCtx {
            row: Some(&v.values),
            schema: Some(&schema),
            params,
        };
        if !predicate(filter, &ctx)? {
            continue;
        }
        check_no_conflict(v, snapshot, owner)?;
        let mut nv = v.values.clone();
        for (idx, expr) in &targets {
            nv[*idx] = schema.columns[*idx].ty.coerce(eval(expr, &ctx)?);
        }
        for (i, c) in schema.columns.iter().enumerate() {
            if c.not_null && nv[i].is_null() {
                return Err(EngineError::constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    table, c.name
                )));
            }
        }
        check_vector_dims(&schema, &nv, table)?;
        updates.push((v.vid, nv));
    }

    // PRIMARY KEY uniqueness for changed keys (vs other rows, concurrent pending
    // inserts, and within this statement).
    if let Some(pk) = schema.primary_key_index() {
        let updated_vids: Vec<u64> = updates.iter().map(|(vid, _)| *vid).collect();
        let (mut committed, pending) = pk_keys(store, table, pk, owner, &updated_vids);
        for (_, nv) in &updates {
            let key = value_key(&nv[pk]);
            if pending.contains(&key) {
                return Err(EngineError::new(
                    crate::error::EngineStatus::ErrConflict,
                    format!(
                        "write conflict: {}.{} is being written by a concurrent transaction",
                        table, schema.columns[pk].name
                    ),
                ));
            }
            if !committed.insert(key) {
                return Err(EngineError::constraint(format!(
                    "UNIQUE constraint failed: {}.{}",
                    table, schema.columns[pk].name
                )));
            }
        }
    }

    // Apply: supersede each old version, append the new one.
    let maintain = store.table_has_index(table);
    let n = updates.len() as i64;
    let mut wal = Vec::new();
    let mut inserted: Vec<(u64, Vec<Value>)> = Vec::new();
    {
        let t = store.table_mut(table).unwrap();
        for (old_vid, nv) in updates {
            if let Some(rv) = t.version_mut(old_vid) {
                rv.delete_lsn = PENDING;
                rv.owner = owner;
            }
            let new_vid = t.alloc_vid();
            wal.push(WalOp::Delete {
                table: table.to_string(),
                vid: old_vid,
            });
            wal.push(WalOp::Insert {
                table: table.to_string(),
                vid: new_vid,
                values: nv.clone(),
            });
            if maintain {
                inserted.push((new_vid, nv.clone()));
            }
            t.rows.push(RowVersion {
                vid: new_vid,
                values: nv,
                create_lsn: PENDING,
                delete_lsn: 0,
                owner,
            });
        }
    }
    for (vid, vals) in &inserted {
        store.index_row_inserted(table, *vid, vals);
    }
    Ok((wal, n))
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
    pk: usize,
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
                committed.insert(value_key(&v.values[pk]));
            } else if v.create_lsn == PENDING && v.owner != me && v.delete_lsn == 0 {
                // A live pending insert owned by a concurrent in-flight writer.
                pending.insert(value_key(&v.values[pk]));
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
