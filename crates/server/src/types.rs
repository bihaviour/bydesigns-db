//! Mapping the engine's dynamic [`Value`] onto Postgres type OIDs and the wire
//! text/binary encodings (spec 07 — "SHOULD support binary parameter and result
//! formats").
//!
//! The engine is dynamically typed (SQLite-style storage classes), so a column's
//! OID is inferred from its values: a column whose values are all integers is
//! advertised as `int8`, all-real as `float8`, otherwise `text`. NULLs do not
//! constrain the inference.

use engine::{ColumnType, Value};

// Postgres built-in type OIDs (from pg_type).
pub const OID_BOOL: i32 = 16;
pub const OID_BYTEA: i32 = 17;
pub const OID_INT8: i32 = 20;
pub const OID_INT4: i32 = 23;
pub const OID_TEXT: i32 = 25;
pub const OID_FLOAT8: i32 = 701;

/// Map the engine's declared column type to a Postgres type OID. A vector has no
/// core Postgres OID, so it is advertised as text (its `[1,2,3]` literal form),
/// which is exactly how pg-wire clients without the pgvector extension see it.
pub fn column_type_oid(ct: ColumnType) -> i32 {
    match ct {
        ColumnType::Integer => OID_INT8,
        ColumnType::Real => OID_FLOAT8,
        ColumnType::Text => OID_TEXT,
        ColumnType::Blob => OID_BYTEA,
        ColumnType::Vector(_) => OID_TEXT,
    }
}

/// Fixed wire length for a type, or `-1` for variable-length.
pub fn type_len(oid: i32) -> i16 {
    match oid {
        OID_BOOL => 1,
        OID_INT4 => 4,
        OID_INT8 | OID_FLOAT8 => 8,
        _ => -1,
    }
}

/// Infer the column OID for a result column from the values it actually holds.
pub fn infer_column_oid(rows: &[Vec<Value>], col: usize) -> i32 {
    let mut seen = false;
    let mut all_int = true;
    let mut all_real = true;
    let mut all_blob = true;
    for row in rows {
        match row.get(col) {
            Some(Value::Null) | None => {}
            Some(Value::Int(_)) => {
                seen = true;
                all_real = false;
                all_blob = false;
            }
            Some(Value::Real(_)) => {
                seen = true;
                all_int = false;
                all_blob = false;
            }
            Some(Value::Blob(_)) => {
                seen = true;
                all_int = false;
                all_real = false;
            }
            // A vector reports as text (its `[1,2,3]` literal).
            Some(Value::Text(_)) | Some(Value::Vector(_)) => {
                seen = true;
                all_int = false;
                all_real = false;
                all_blob = false;
            }
        }
    }
    if !seen {
        OID_TEXT
    } else if all_int {
        OID_INT8
    } else if all_real {
        OID_FLOAT8
    } else if all_blob {
        OID_BYTEA
    } else {
        OID_TEXT
    }
}

/// Encode a value in the requested format (`0` text, `1` binary). `None` => NULL.
pub fn encode_value(v: &Value, format: i16) -> Option<Vec<u8>> {
    if matches!(v, Value::Null) {
        return None;
    }
    if format == 1 {
        Some(encode_binary(v))
    } else {
        Some(encode_text(v))
    }
}

fn encode_text(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => Vec::new(),
        Value::Int(i) => i.to_string().into_bytes(),
        Value::Real(r) => format_float(*r).into_bytes(),
        Value::Text(s) => s.clone().into_bytes(),
        Value::Blob(b) => hex_bytea(b).into_bytes(),
        Value::Vector(_) => v.render().unwrap_or_default().into_bytes(),
    }
}

fn encode_binary(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => Vec::new(),
        Value::Int(i) => i.to_be_bytes().to_vec(),
        Value::Real(r) => r.to_bits().to_be_bytes().to_vec(),
        Value::Text(s) => s.clone().into_bytes(),
        Value::Blob(b) => b.clone(),
        // No pg binary wire form for a vector; send its text literal.
        Value::Vector(_) => v.render().unwrap_or_default().into_bytes(),
    }
}

/// Decode a bound parameter into a [`Value`]. Text params are type-inferred
/// (int → real → text) so `WHERE id = $1` matches an integer column; binary
/// params are decoded by their declared length.
pub fn decode_param(bytes: &Option<Vec<u8>>, format: i16) -> Value {
    let Some(bytes) = bytes else {
        return Value::Null;
    };
    if format == 1 {
        match bytes.len() {
            4 => Value::Int(i32::from_be_bytes(bytes[..4].try_into().unwrap()) as i64),
            8 => Value::Int(i64::from_be_bytes(bytes[..8].try_into().unwrap())),
            _ => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        }
    } else {
        let s = String::from_utf8_lossy(bytes).into_owned();
        if let Ok(i) = s.parse::<i64>() {
            Value::Int(i)
        } else if let Ok(r) = s.parse::<f64>() {
            Value::Real(r)
        } else {
            Value::Text(s)
        }
    }
}

fn format_float(r: f64) -> String {
    if r.fract() == 0.0 && r.is_finite() && r.abs() < 1e15 {
        format!("{r:.1}")
    } else {
        format!("{r}")
    }
}

/// Postgres `bytea` hex output format: `\x` followed by lowercase hex.
fn hex_bytea(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("\\x");
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}
