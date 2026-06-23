//! Engine-owned WAL encoding. Each [`WalOp`] serializes to one opaque
//! [`twill_storage::WalRecord`]; the storage backend stores and orders them
//! but never interprets the bytes (spec 02 / 03).
//!
//! A committing transaction emits its data ops followed by a single `Commit`
//! marker. On recovery the engine groups records up to each marker and applies
//! the group, stamping every produced row version with the marker's commit LSN.
//! Records after the last marker (an incomplete transaction) are discarded.

use crate::catalog::{Column, ForeignKey, TableSchema};
use crate::error::{EngineError, Result};
use crate::value::{ColumnType, Value};
use crate::vector::{IndexDef, IndexParams, Metric};
use twill_storage::WalRecord;

#[derive(Clone, Debug)]
pub enum WalOp {
    CreateTable {
        schema: TableSchema,
    },
    DropTable {
        name: String,
    },
    Insert {
        table: String,
        vid: u64,
        values: Vec<Value>,
    },
    Delete {
        table: String,
        vid: u64,
    },
    CreateIndex {
        def: IndexDef,
    },
    DropIndex {
        name: String,
    },
    /// `ALTER TABLE … ADD COLUMN` (stage 6D).
    AlterAddColumn {
        table: String,
        column: Column,
    },
    /// `ALTER TABLE … DROP COLUMN` (stage 6D).
    AlterDropColumn {
        table: String,
        column: String,
    },
    /// `ALTER TABLE … RENAME COLUMN` (stage 6D).
    AlterRenameColumn {
        table: String,
        from: String,
        to: String,
    },
    /// `ALTER TABLE … RENAME TO` (stage 6D).
    AlterRenameTable {
        table: String,
        to: String,
    },
    /// `CREATE VIEW name AS …` (deferred 6B). Carries the full statement text,
    /// re-parsed on replay to rebuild the view's parsed body (a derived catalog
    /// fact, like an index — no rows of its own).
    CreateView {
        name: String,
        sql: String,
    },
    /// `DROP VIEW name`.
    DropView {
        name: String,
    },
    Commit,
}

const OP_CREATE: u8 = 1;
const OP_DROP: u8 = 2;
const OP_INSERT: u8 = 3;
const OP_DELETE: u8 = 4;
const OP_COMMIT: u8 = 5;
const OP_CREATE_INDEX: u8 = 6;
const OP_DROP_INDEX: u8 = 7;
const OP_ALTER_ADD: u8 = 8;
const OP_ALTER_DROP: u8 = 9;
const OP_ALTER_RENAME_COL: u8 = 10;
const OP_ALTER_RENAME_TABLE: u8 = 11;
const OP_CREATE_VIEW: u8 = 12;
const OP_DROP_VIEW: u8 = 13;

impl WalOp {
    pub fn encode(&self) -> WalRecord {
        let mut b = Vec::new();
        match self {
            WalOp::CreateTable { schema } => {
                b.push(OP_CREATE);
                put_str(&mut b, &schema.name);
                put_u32(&mut b, schema.columns.len() as u32);
                for c in &schema.columns {
                    put_str(&mut b, &c.name);
                    put_coltype(&mut b, c.ty);
                    let mut flags = 0u8;
                    if c.primary_key {
                        flags |= 1;
                    }
                    if c.not_null {
                        flags |= 2;
                    }
                    if c.unique {
                        flags |= 4;
                    }
                    if c.autoincrement {
                        flags |= 8;
                    }
                    b.push(flags);
                }
                // Foreign keys follow the columns. Records written before FK
                // support carry no trailing bytes; decode treats their absence
                // as "no foreign keys" (see below), so this stays compatible.
                put_u32(&mut b, schema.foreign_keys.len() as u32);
                for fk in &schema.foreign_keys {
                    put_str(&mut b, &fk.name);
                    put_str(&mut b, &fk.foreign_table);
                    put_u32(&mut b, fk.columns.len() as u32);
                    for c in &fk.columns {
                        put_str(&mut b, c);
                    }
                    put_u32(&mut b, fk.foreign_columns.len() as u32);
                    for c in &fk.foreign_columns {
                        put_str(&mut b, c);
                    }
                }
                // Stage-6D extras follow the FK section: per-column DEFAULT text,
                // then table CHECK predicates and composite UNIQUE sets. A record
                // written before 6D ends after the FKs (decode treats an exhausted
                // cursor as "no extras"), so this stays backward-compatible.
                put_u32(&mut b, schema.columns.len() as u32);
                for c in &schema.columns {
                    match &c.default_sql {
                        Some(s) => {
                            b.push(1);
                            put_str(&mut b, s);
                        }
                        None => b.push(0),
                    }
                }
                put_u32(&mut b, schema.checks.len() as u32);
                for chk in &schema.checks {
                    put_str(&mut b, chk);
                }
                put_u32(&mut b, schema.uniques.len() as u32);
                for u in &schema.uniques {
                    put_u32(&mut b, u.len() as u32);
                    for col in u {
                        put_str(&mut b, col);
                    }
                }
            }
            WalOp::DropTable { name } => {
                b.push(OP_DROP);
                put_str(&mut b, name);
            }
            WalOp::CreateIndex { def } => {
                b.push(OP_CREATE_INDEX);
                put_str(&mut b, &def.name);
                put_str(&mut b, &def.table);
                put_str(&mut b, &def.column);
                put_u32(&mut b, def.params.m as u32);
                put_u32(&mut b, def.params.ef_construction as u32);
                put_u32(&mut b, def.params.ef_search as u32);
                b.push(def.params.metric.tag());
            }
            WalOp::DropIndex { name } => {
                b.push(OP_DROP_INDEX);
                put_str(&mut b, name);
            }
            WalOp::Insert { table, vid, values } => {
                b.push(OP_INSERT);
                put_str(&mut b, table);
                put_u64(&mut b, *vid);
                put_u32(&mut b, values.len() as u32);
                for v in values {
                    put_value(&mut b, v);
                }
            }
            WalOp::Delete { table, vid } => {
                b.push(OP_DELETE);
                put_str(&mut b, table);
                put_u64(&mut b, *vid);
            }
            WalOp::AlterAddColumn { table, column } => {
                b.push(OP_ALTER_ADD);
                put_str(&mut b, table);
                // Reuse the column-list + extras framing (a single column) so the
                // decoder shares decode_columns / decode_table_extras.
                put_u32(&mut b, 1);
                put_str(&mut b, &column.name);
                put_coltype(&mut b, column.ty);
                let mut flags = 0u8;
                if column.primary_key {
                    flags |= 1;
                }
                if column.not_null {
                    flags |= 2;
                }
                if column.unique {
                    flags |= 4;
                }
                if column.autoincrement {
                    flags |= 8;
                }
                b.push(flags);
                put_u32(&mut b, 1); // extras: one default entry
                match &column.default_sql {
                    Some(s) => {
                        b.push(1);
                        put_str(&mut b, s);
                    }
                    None => b.push(0),
                }
                put_u32(&mut b, 0); // no checks
                put_u32(&mut b, 0); // no uniques
            }
            WalOp::AlterDropColumn { table, column } => {
                b.push(OP_ALTER_DROP);
                put_str(&mut b, table);
                put_str(&mut b, column);
            }
            WalOp::AlterRenameColumn { table, from, to } => {
                b.push(OP_ALTER_RENAME_COL);
                put_str(&mut b, table);
                put_str(&mut b, from);
                put_str(&mut b, to);
            }
            WalOp::AlterRenameTable { table, to } => {
                b.push(OP_ALTER_RENAME_TABLE);
                put_str(&mut b, table);
                put_str(&mut b, to);
            }
            WalOp::CreateView { name, sql } => {
                b.push(OP_CREATE_VIEW);
                put_str(&mut b, name);
                put_str(&mut b, sql);
            }
            WalOp::DropView { name } => {
                b.push(OP_DROP_VIEW);
                put_str(&mut b, name);
            }
            WalOp::Commit => b.push(OP_COMMIT),
        }
        WalRecord::new(b)
    }

    pub fn decode(bytes: &[u8]) -> Result<WalOp> {
        let mut c = Cursor { b: bytes, p: 0 };
        let tag = c.u8()?;
        let op = match tag {
            OP_CREATE => decode_create(&mut c)?,
            OP_ALTER_ADD => decode_alter_add(&mut c)?,
            OP_ALTER_DROP => WalOp::AlterDropColumn {
                table: c.str()?,
                column: c.str()?,
            },
            OP_ALTER_RENAME_COL => WalOp::AlterRenameColumn {
                table: c.str()?,
                from: c.str()?,
                to: c.str()?,
            },
            OP_ALTER_RENAME_TABLE => WalOp::AlterRenameTable {
                table: c.str()?,
                to: c.str()?,
            },
            OP_CREATE_VIEW => WalOp::CreateView {
                name: c.str()?,
                sql: c.str()?,
            },
            OP_DROP_VIEW => WalOp::DropView { name: c.str()? },
            OP_DROP => WalOp::DropTable { name: c.str()? },
            OP_CREATE_INDEX => decode_create_index(&mut c)?,
            OP_DROP_INDEX => WalOp::DropIndex { name: c.str()? },
            OP_INSERT => decode_insert(&mut c)?,
            OP_DELETE => WalOp::Delete {
                table: c.str()?,
                vid: c.u64()?,
            },
            OP_COMMIT => WalOp::Commit,
            other => return Err(EngineError::internal(format!("bad WAL op tag {other}"))),
        };
        Ok(op)
    }
}

/// Decode a `CreateTable` op body (name + columns + FKs + checks/uniques).
fn decode_create(c: &mut Cursor) -> Result<WalOp> {
    let name = c.str()?;
    let mut columns = decode_columns(c)?;
    let foreign_keys = decode_foreign_keys(c)?;
    let (checks, uniques) = decode_table_extras(c, &mut columns)?;
    Ok(WalOp::CreateTable {
        schema: TableSchema {
            name,
            columns,
            foreign_keys,
            checks,
            uniques,
        },
    })
}

/// Decode an `AlterAddColumn` op body (one column, sharing the column codec).
fn decode_alter_add(c: &mut Cursor) -> Result<WalOp> {
    let table = c.str()?;
    let mut cols = decode_columns(c)?;
    let _ = decode_table_extras(c, &mut cols)?;
    Ok(WalOp::AlterAddColumn {
        table,
        column: cols
            .pop()
            .ok_or_else(|| EngineError::internal("ALTER ADD COLUMN record has no column"))?,
    })
}

/// Decode a `CreateIndex` op body (name/table/column + HNSW params).
fn decode_create_index(c: &mut Cursor) -> Result<WalOp> {
    let name = c.str()?;
    let table = c.str()?;
    let column = c.str()?;
    let m = c.u32()? as usize;
    let ef_construction = c.u32()? as usize;
    let ef_search = c.u32()? as usize;
    let metric = Metric::from_tag(c.u8()?);
    Ok(WalOp::CreateIndex {
        def: IndexDef {
            name,
            table,
            column,
            params: IndexParams {
                m,
                ef_construction,
                ef_search,
                metric,
            },
        },
    })
}

/// Decode an `Insert` op body (table + vid + count-prefixed values).
fn decode_insert(c: &mut Cursor) -> Result<WalOp> {
    let table = c.str()?;
    let vid = c.u64()?;
    let n = c.u32()? as usize;
    let mut values = Vec::with_capacity(n);
    for _ in 0..n {
        values.push(c.value()?);
    }
    Ok(WalOp::Insert { table, vid, values })
}

/// Decode a `CreateTable`'s column list (count-prefixed).
fn decode_columns(c: &mut Cursor) -> Result<Vec<Column>> {
    let n = c.u32()? as usize;
    let mut columns = Vec::with_capacity(n);
    for _ in 0..n {
        let name = c.str()?;
        let ty = c.coltype()?;
        let flags = c.u8()?;
        columns.push(Column {
            name,
            ty,
            primary_key: flags & 1 != 0,
            not_null: flags & 2 != 0,
            unique: flags & 4 != 0,
            autoincrement: flags & 8 != 0,
            default_sql: None,
        });
    }
    Ok(columns)
}

/// Decode the stage-6D extras section (per-column DEFAULT text, table CHECK
/// predicates, composite UNIQUE sets), filling `columns` defaults in place. A
/// record written before 6D is exhausted here, yielding empty extras.
fn decode_table_extras(
    c: &mut Cursor,
    columns: &mut [Column],
) -> Result<(Vec<String>, Vec<Vec<String>>)> {
    if c.at_end() {
        return Ok((Vec::new(), Vec::new()));
    }
    let ncols = c.u32()? as usize;
    for i in 0..ncols {
        if c.u8()? == 1 {
            let s = c.str()?;
            if let Some(col) = columns.get_mut(i) {
                col.default_sql = Some(s);
            }
        }
    }
    let checks = decode_str_list(c)?;
    let nuniq = c.u32()? as usize;
    let mut uniques = Vec::with_capacity(nuniq);
    for _ in 0..nuniq {
        uniques.push(decode_str_list(c)?);
    }
    Ok((checks, uniques))
}

/// Decode the foreign-key section, if present. A record written before FK support
/// ends right after the columns, so an exhausted cursor means "no foreign keys"
/// rather than a truncation error (backward-compatible decode).
fn decode_foreign_keys(c: &mut Cursor) -> Result<Vec<ForeignKey>> {
    let mut foreign_keys = Vec::new();
    if c.at_end() {
        return Ok(foreign_keys);
    }
    let nfk = c.u32()? as usize;
    for _ in 0..nfk {
        let name = c.str()?;
        let foreign_table = c.str()?;
        let columns = decode_str_list(c)?;
        let foreign_columns = decode_str_list(c)?;
        foreign_keys.push(ForeignKey {
            name,
            columns,
            foreign_table,
            foreign_columns,
        });
    }
    Ok(foreign_keys)
}

/// Decode a count-prefixed list of strings.
fn decode_str_list(c: &mut Cursor) -> Result<Vec<String>> {
    let n = c.u32()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(c.str()?);
    }
    Ok(v)
}

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_str(b: &mut Vec<u8>, s: &str) {
    put_u32(b, s.len() as u32);
    b.extend_from_slice(s.as_bytes());
}
/// Column type codec: the affinity tag, plus the dimension for `vector(n)`.
fn put_coltype(b: &mut Vec<u8>, ty: ColumnType) {
    b.push(ty.tag());
    if let ColumnType::Vector(d) = ty {
        put_u32(b, d);
    }
}
fn put_value(b: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => b.push(0),
        Value::Int(i) => {
            b.push(1);
            put_u64(b, *i as u64);
        }
        Value::Real(r) => {
            b.push(2);
            b.extend_from_slice(&r.to_le_bytes());
        }
        Value::Text(s) => {
            b.push(3);
            put_str(b, s);
        }
        Value::Blob(bytes) => {
            b.push(4);
            put_u32(b, bytes.len() as u32);
            b.extend_from_slice(bytes);
        }
        Value::Vector(vec) => {
            b.push(5);
            put_u32(b, vec.len() as u32);
            for x in vec {
                b.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    /// Whether the cursor has consumed every byte of the record.
    fn at_end(&self) -> bool {
        self.p >= self.b.len()
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.p + n > self.b.len() {
            return Err(EngineError::internal("WAL record truncated"));
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| EngineError::internal("bad utf8 in WAL"))
    }
    fn value(&mut self) -> Result<Value> {
        let tag = self.u8()?;
        Ok(match tag {
            0 => Value::Null,
            1 => Value::Int(self.u64()? as i64),
            2 => Value::Real(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            3 => Value::Text(self.str()?),
            4 => {
                let n = self.u32()? as usize;
                Value::Blob(self.take(n)?.to_vec())
            }
            5 => {
                let n = self.u32()? as usize;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(f32::from_le_bytes(self.take(4)?.try_into().unwrap()));
                }
                Value::Vector(v)
            }
            other => return Err(EngineError::internal(format!("bad value tag {other}"))),
        })
    }

    /// Decode a column type written by [`put_coltype`].
    fn coltype(&mut self) -> Result<ColumnType> {
        let tag = self.u8()?;
        if tag == 4 {
            Ok(ColumnType::Vector(self.u32()?))
        } else {
            Ok(ColumnType::from_tag(tag))
        }
    }
}
