//! Executor: evaluates expressions and runs statements against the MVCC store.
//!
//! Reads acquire an MVCC snapshot and filter row versions by visibility; writes
//! validate fully *before* mutating so each statement is atomic (a mid-statement
//! constraint failure leaves the store untouched). Mutations stamp new versions
//! `PENDING` and emit [`WalOp`]s; durability and publish-at-commit-LSN are the
//! transaction manager's job (see [`crate::conn`]).

use crate::catalog::TableSchema;
use crate::error::{EngineError, Result};
use crate::sql::{AggArg, AggFunc, BinOp, Expr, SelItem, SelectStmt, UnOp};
use crate::store::{RowVersion, Store, PENDING};
use crate::value::Value;
use crate::wal::WalOp;
use std::cmp::Ordering;
use std::collections::HashSet;

/// A buffered query result. Cells render to the string-only C ABI on demand.
#[derive(Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<String>,
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
    }
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

pub fn run_select(
    store: &Store,
    sel: &SelectStmt,
    snapshot: u64,
    as_writer: bool,
    params: &[Value],
) -> Result<ResultSet> {
    let Some(table_name) = &sel.from else {
        return constant_select(sel, params);
    };

    let table = store
        .table(table_name)
        .ok_or_else(|| EngineError::sql(format!("no such table: {table_name}")))?;
    let schema = &table.schema;

    // Visible rows passing WHERE.
    let mut matched: Vec<&Vec<Value>> = Vec::new();
    for v in &table.rows {
        let visible = if as_writer {
            v.visible_to_writer(snapshot)
        } else {
            v.visible_to_reader(snapshot)
        };
        if !visible {
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

    // ORDER BY (evaluated on the matched rows).
    if !sel.order_by.is_empty() {
        let keys = &sel.order_by;
        let mut indexed: Vec<usize> = (0..matched.len()).collect();
        let mut err: Option<EngineError> = None;
        indexed.sort_by(|&a, &b| {
            if err.is_some() {
                return Ordering::Equal;
            }
            for (expr, asc) in keys {
                let ca = EvalCtx {
                    row: Some(matched[a]),
                    schema: Some(schema),
                    params,
                };
                let cb = EvalCtx {
                    row: Some(matched[b]),
                    schema: Some(schema),
                    params,
                };
                let va = match eval(expr, &ca) {
                    Ok(v) => v,
                    Err(e) => {
                        err = Some(e);
                        return Ordering::Equal;
                    }
                };
                let vb = match eval(expr, &cb) {
                    Ok(v) => v,
                    Err(e) => {
                        err = Some(e);
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
        });
        if let Some(e) = err {
            return Err(e);
        }
        let reordered: Vec<&Vec<Value>> = indexed.into_iter().map(|i| matched[i]).collect();
        matched = reordered;
    }

    if let Some(limit) = sel.limit {
        let n = limit.max(0) as usize;
        matched.truncate(n);
    }

    // Projection + column names.
    let mut columns = Vec::new();
    let mut plan: Vec<ProjItem> = Vec::new();
    for item in &sel.items {
        match item {
            SelItem::Star => {
                for c in &schema.columns {
                    columns.push(c.name.clone());
                    plan.push(ProjItem::Column(schema.column_index(&c.name).unwrap()));
                }
            }
            SelItem::Expr { expr, alias } => {
                columns.push(column_name(expr, alias, columns.len()));
                plan.push(ProjItem::Expr(expr.clone()));
            }
            SelItem::Aggregate { .. } => unreachable!("aggregate handled above"),
        }
    }

    let mut rows = Vec::with_capacity(matched.len());
    for vals in &matched {
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

    Ok(ResultSet { columns, rows })
}

enum ProjItem {
    Column(usize),
    Expr(Expr),
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
    let mut row = Vec::new();
    for item in &sel.items {
        if let SelItem::Expr { expr, alias } = item {
            columns.push(column_name(expr, alias, columns.len()));
            row.push(eval(expr, &ctx)?);
        }
    }
    Ok(ResultSet {
        columns,
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
    let mut row = Vec::new();
    for item in &sel.items {
        if let SelItem::Aggregate { func, arg, alias } = item {
            columns.push(alias.clone().unwrap_or_else(|| agg_name(*func)));
            row.push(compute_aggregate(*func, arg, schema, matched, params)?);
        }
    }
    // LIMIT 0 suppresses the single aggregate row.
    let rows = if sel.limit == Some(0) {
        vec![]
    } else {
        vec![row]
    };
    Ok(ResultSet { columns, rows })
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
        // NOT NULL.
        for (i, c) in schema.columns.iter().enumerate() {
            if c.not_null && vals[i].is_null() {
                return Err(EngineError::constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    table, c.name
                )));
            }
        }
        staged.push(vals);
    }

    // PRIMARY KEY uniqueness (vs existing visible rows + within this batch).
    if let Some(pk) = schema.primary_key_index() {
        let mut seen = existing_pk_keys(store, table, pk, &[]);
        for vals in &staged {
            let key = value_key(&vals[pk]);
            if !seen.insert(key) {
                return Err(EngineError::constraint(format!(
                    "UNIQUE constraint failed: {}.{}",
                    table, schema.columns[pk].name
                )));
            }
        }
    }

    // Apply.
    let t = store.table_mut(table).unwrap();
    let mut wal = Vec::with_capacity(staged.len());
    let n = staged.len() as i64;
    for vals in staged {
        let vid = t.alloc_vid();
        wal.push(WalOp::Insert {
            table: table.to_string(),
            vid,
            values: vals.clone(),
        });
        t.rows.push(RowVersion {
            vid,
            values: vals,
            create_lsn: PENDING,
            delete_lsn: 0,
        });
    }
    Ok((wal, n))
}

pub fn run_delete(
    store: &mut Store,
    table: &str,
    filter: &Option<Expr>,
    snapshot: u64,
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
        if !v.visible_to_writer(snapshot) {
            continue;
        }
        let ctx = EvalCtx {
            row: Some(&v.values),
            schema: Some(&schema),
            params,
        };
        if predicate(filter, &ctx)? {
            check_no_conflict(v, snapshot)?;
            victims.push(v.vid);
        }
    }
    let t = store.table_mut(table).unwrap();
    let mut wal = Vec::with_capacity(victims.len());
    for vid in &victims {
        if let Some(rv) = t.version_mut(*vid) {
            rv.delete_lsn = PENDING;
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
        if !v.visible_to_writer(snapshot) {
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
        check_no_conflict(v, snapshot)?;
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
        updates.push((v.vid, nv));
    }

    // PRIMARY KEY uniqueness for changed keys.
    if let Some(pk) = schema.primary_key_index() {
        let updated_vids: Vec<u64> = updates.iter().map(|(vid, _)| *vid).collect();
        let mut keys = existing_pk_keys(store, table, pk, &updated_vids);
        for (_, nv) in &updates {
            let key = value_key(&nv[pk]);
            if !keys.insert(key) {
                return Err(EngineError::constraint(format!(
                    "UNIQUE constraint failed: {}.{}",
                    table, schema.columns[pk].name
                )));
            }
        }
    }

    // Apply: supersede each old version, append the new one.
    let n = updates.len() as i64;
    let t = store.table_mut(table).unwrap();
    let mut wal = Vec::new();
    for (old_vid, nv) in updates {
        if let Some(rv) = t.version_mut(old_vid) {
            rv.delete_lsn = PENDING;
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
        t.rows.push(RowVersion {
            vid: new_vid,
            values: nv,
            create_lsn: PENDING,
            delete_lsn: 0,
        });
    }
    Ok((wal, n))
}

/// PK keys of writer-visible rows, excluding rows whose vid is in `exclude`.
fn existing_pk_keys(store: &Store, table: &str, pk: usize, exclude: &[u64]) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Some(t) = store.table(table) {
        for v in &t.rows {
            if exclude.contains(&v.vid) {
                continue;
            }
            // Use the writer snapshot via committed_lsn (pending rows belong to us).
            if v.visible_to_writer(store.committed_lsn) {
                set.insert(value_key(&v.values[pk]));
            }
        }
    }
    set
}

/// First-committer-wins: a row this writer means to modify must not already
/// carry a committed supersede newer than the writer's snapshot (i.e. another
/// transaction committed a change to it after we began). Aborts with
/// `ENGINE_ERR_CONFLICT` so callers can retry on a fresh snapshot.
fn check_no_conflict(v: &RowVersion, snapshot: u64) -> Result<()> {
    if v.delete_lsn != 0 && v.delete_lsn != PENDING && v.delete_lsn > snapshot {
        return Err(EngineError::new(
            crate::error::EngineStatus::ErrConflict,
            "write conflict: row modified by a concurrent committed transaction",
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
    }
}
