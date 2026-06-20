//! Engine-owned WAL encoding. Each [`WalOp`] serializes to one opaque
//! [`bydesigns_storage::WalRecord`]; the storage backend stores and orders them
//! but never interprets the bytes (spec 02 / 03).
//!
//! A committing transaction emits its data ops followed by a single `Commit`
//! marker. On recovery the engine groups records up to each marker and applies
//! the group, stamping every produced row version with the marker's commit LSN.
//! Records after the last marker (an incomplete transaction) are discarded.

use crate::catalog::{Column, TableSchema};
use crate::error::{EngineError, Result};
use crate::value::{ColumnType, Value};
use bydesigns_storage::WalRecord;

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
    Commit,
}

const OP_CREATE: u8 = 1;
const OP_DROP: u8 = 2;
const OP_INSERT: u8 = 3;
const OP_DELETE: u8 = 4;
const OP_COMMIT: u8 = 5;

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
                    b.push(c.ty.tag());
                    let mut flags = 0u8;
                    if c.primary_key {
                        flags |= 1;
                    }
                    if c.not_null {
                        flags |= 2;
                    }
                    b.push(flags);
                }
            }
            WalOp::DropTable { name } => {
                b.push(OP_DROP);
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
            WalOp::Commit => b.push(OP_COMMIT),
        }
        WalRecord::new(b)
    }

    pub fn decode(bytes: &[u8]) -> Result<WalOp> {
        let mut c = Cursor { b: bytes, p: 0 };
        let tag = c.u8()?;
        let op = match tag {
            OP_CREATE => {
                let name = c.str()?;
                let n = c.u32()? as usize;
                let mut columns = Vec::with_capacity(n);
                for _ in 0..n {
                    let cname = c.str()?;
                    let ty = ColumnType::from_tag(c.u8()?);
                    let flags = c.u8()?;
                    columns.push(Column {
                        name: cname,
                        ty,
                        primary_key: flags & 1 != 0,
                        not_null: flags & 2 != 0,
                    });
                }
                WalOp::CreateTable {
                    schema: TableSchema { name, columns },
                }
            }
            OP_DROP => WalOp::DropTable { name: c.str()? },
            OP_INSERT => {
                let table = c.str()?;
                let vid = c.u64()?;
                let n = c.u32()? as usize;
                let mut values = Vec::with_capacity(n);
                for _ in 0..n {
                    values.push(c.value()?);
                }
                WalOp::Insert { table, vid, values }
            }
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
    }
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
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
            other => return Err(EngineError::internal(format!("bad value tag {other}"))),
        })
    }
}
