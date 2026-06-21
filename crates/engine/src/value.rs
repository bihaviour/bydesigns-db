//! SQL values and column types — the engine's row-cell representation.

use std::cmp::Ordering;

/// A single SQL value (NULL or one of the storage classes). `Vector` is the
/// Phase-5 fixed-length `f32` array (spec 12 — vector search in-core).
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    Vector(Vec<f32>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Int(_) => "INTEGER",
            Value::Real(_) => "REAL",
            Value::Text(_) => "TEXT",
            Value::Blob(_) => "BLOB",
            Value::Vector(_) => "VECTOR",
        }
    }

    /// Render to the text form returned across the string-only C ABI. NULL has
    /// no text form here (the FFI layer returns a null pointer for it). A vector
    /// renders as `[1,2,3]` — the same literal form clients send back.
    pub fn render(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Int(i) => Some(i.to_string()),
            Value::Real(r) => Some(format_real(*r)),
            Value::Text(s) => Some(s.clone()),
            Value::Blob(b) => Some(base64_encode(b)),
            Value::Vector(v) => Some(format_vector(v)),
        }
    }

    /// Truthiness under SQL three-valued logic: `None` == unknown (NULL).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Null => None,
            Value::Int(i) => Some(*i != 0),
            Value::Real(r) => Some(*r != 0.0),
            Value::Text(s) => Some(!s.is_empty() && s != "0"),
            Value::Blob(b) => Some(!b.is_empty()),
            Value::Vector(v) => Some(!v.is_empty()),
        }
    }

    /// Borrow the vector payload, if this value is a vector.
    pub fn as_vector(&self) -> Option<&[f32]> {
        match self {
            Value::Vector(v) => Some(v),
            _ => None,
        }
    }

    fn numeric(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Real(r) => Some(*r),
            _ => None,
        }
    }

    /// SQL comparison. `None` when the comparison is unknown (a NULL operand or
    /// incomparable mixed types).
    pub fn sql_cmp(&self, other: &Value) -> Option<Ordering> {
        if self.is_null() || other.is_null() {
            return None;
        }
        if let (Some(a), Some(b)) = (self.numeric(), other.numeric()) {
            return a.partial_cmp(&b);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
            (Value::Blob(a), Value::Blob(b)) => Some(a.cmp(b)),
            // Mixed/incomparable types: unknown.
            _ => None,
        }
    }

    /// SQL equality (three-valued). Mixed incomparable types are not equal.
    pub fn sql_eq(&self, other: &Value) -> Option<bool> {
        if self.is_null() || other.is_null() {
            return None;
        }
        match self.sql_cmp(other) {
            Some(o) => Some(o == Ordering::Equal),
            None => Some(false),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Real(a), Value::Real(b)) => a == b,
            (Value::Int(a), Value::Real(b)) | (Value::Real(b), Value::Int(a)) => (*a as f64) == *b,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::Blob(a), Value::Blob(b)) => a == b,
            (Value::Vector(a), Value::Vector(b)) => a == b,
            _ => false,
        }
    }
}

/// Declared column storage class. `Vector(dim)` is the Phase-5 fixed-length
/// `f32` array; the dimension is declared at column-definition time and enforced
/// on insert (spec 12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Real,
    Text,
    Blob,
    Vector(u32),
}

impl ColumnType {
    pub fn from_sql(name: &str) -> ColumnType {
        let n = name.to_ascii_uppercase();
        if n.contains("INT") {
            ColumnType::Integer
        } else if n.contains("CHAR")
            || n.contains("TEXT")
            || n.contains("CLOB")
            || n.contains("STRING")
        {
            ColumnType::Text
        } else if n.contains("VEC") {
            // Dimension is captured by the parser from the `(n)` suffix; a bare
            // `vector` with no size defaults to 0 (rejected at column build).
            ColumnType::Vector(0)
        } else if n.contains("REAL")
            || n.contains("FLOA")
            || n.contains("DOUB")
            || n.contains("NUMERIC")
            || n.contains("DEC")
        {
            ColumnType::Real
        } else if n.contains("BLOB") || n.contains("BYTEA") {
            ColumnType::Blob
        } else {
            // SQLite-ish default affinity.
            ColumnType::Text
        }
    }

    /// True for a `vector(n)` declaration.
    pub fn is_vector(self) -> bool {
        matches!(self, ColumnType::Vector(_))
    }

    pub fn tag(self) -> u8 {
        match self {
            ColumnType::Integer => 0,
            ColumnType::Real => 1,
            ColumnType::Text => 2,
            ColumnType::Blob => 3,
            ColumnType::Vector(_) => 4,
        }
    }

    pub fn from_tag(t: u8) -> ColumnType {
        match t {
            0 => ColumnType::Integer,
            1 => ColumnType::Real,
            3 => ColumnType::Blob,
            // Vector dimension is encoded alongside the tag by the WAL codec;
            // this fallback is only hit for a malformed tag.
            4 => ColumnType::Vector(0),
            _ => ColumnType::Text,
        }
    }

    /// Light type affinity: coerce a value toward the column's class where it is
    /// lossless / natural; otherwise leave it unchanged. A `'[1,2,3]'` text
    /// literal bound to a vector column parses into a vector (pgvector-style).
    pub fn coerce(self, v: Value) -> Value {
        match (self, v) {
            (ColumnType::Real, Value::Int(i)) => Value::Real(i as f64),
            (ColumnType::Integer, Value::Real(r)) if r.fract() == 0.0 => Value::Int(r as i64),
            (ColumnType::Vector(_), Value::Text(s)) => match parse_vector(&s) {
                Some(v) => Value::Vector(v),
                None => Value::Text(s),
            },
            (_, other) => other,
        }
    }
}

/// Render an `f32` vector as the `[a,b,c]` literal both `SELECT` and the wire
/// path use. Each component is formatted compactly (no trailing zeros).
pub fn format_vector(v: &[f32]) -> String {
    let mut out = String::with_capacity(2 + v.len() * 4);
    out.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format_f32(*x));
    }
    out.push(']');
    out
}

fn format_f32(x: f32) -> String {
    if x.fract() == 0.0 && x.is_finite() && x.abs() < 1e15 {
        format!("{x:.0}")
    } else {
        format!("{x}")
    }
}

/// Parse a vector literal: `[1, 2, 3]`, `1,2,3`, or whitespace-separated. Used
/// for text→vector coercion and the `'v…'` FFI parameter encoding. Returns
/// `None` if any component is not a finite number.
pub fn parse_vector(s: &str) -> Option<Vec<f32>> {
    let inner = s.trim();
    let inner = inner
        .strip_prefix('[')
        .map(|r| r.strip_suffix(']').unwrap_or(r))
        .unwrap_or(inner)
        .trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split([',', ' ', '\t'])
        .filter(|t| !t.is_empty())
        .map(|t| t.trim().parse::<f32>().ok().filter(|x| x.is_finite()))
        .collect()
}

fn format_real(r: f64) -> String {
    if r.fract() == 0.0 && r.is_finite() && r.abs() < 1e15 {
        // Render integral reals with a trailing .0 so they read as REAL.
        format!("{:.1}", r)
    } else {
        let s = format!("{}", r);
        s
    }
}

// ---- minimal base64 (standard alphabet, padded) for BLOB rendering --------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in s.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut n = 0u32;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                pad += 1;
                n <<= 6;
            } else {
                n = (n << 6) | val(c)?;
            }
            let _ = i;
        }
        if chunk.len() == 4 {
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        } else {
            return None;
        }
    }
    Some(out)
}
