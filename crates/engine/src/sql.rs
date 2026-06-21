//! A focused SQL frontend: lexer + recursive-descent parser producing the
//! engine's internal statement AST (spec 02 — parser stage). The supported
//! subset is deliberately small for Phase 1 (DDL + DML + queries + transaction
//! control); anything outside it is rejected with `ENGINE_ERR_SQL` rather than
//! silently mis-parsed.

use crate::error::{EngineError, Result};
use crate::value::ColumnType;
use crate::vector::{IndexParams, Metric};

// ---- AST ------------------------------------------------------------------

#[derive(Debug)]
pub enum Stmt {
    CreateTable {
        name: String,
        columns: Vec<ColumnSpec>,
        if_not_exists: bool,
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
        rows: Vec<Vec<Expr>>,
    },
    Select(SelectStmt),
    Update {
        table: String,
        sets: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug)]
pub struct ColumnSpec {
    pub name: String,
    pub ty: ColumnType,
    pub primary_key: bool,
    pub not_null: bool,
}

#[derive(Debug)]
pub struct SelectStmt {
    pub items: Vec<SelItem>,
    pub from: Option<String>,
    pub filter: Option<Expr>,
    pub order_by: Vec<(Expr, bool)>, // (expr, ascending)
    pub limit: Option<i64>,
}

#[derive(Debug)]
pub enum SelItem {
    Star,
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
    Aggregate {
        func: AggFunc,
        arg: AggArg,
        cast: Option<CastTarget>,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum AggFunc {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

#[derive(Debug)]
pub enum AggArg {
    Star,
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Null,
    Int(i64),
    Real(f64),
    Str(String),
    Vector(Vec<f32>),
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
        negated: bool,
    },
    /// Postgres `expr::type` cast — coerces the inner value to `target`.
    Cast {
        e: Box<Expr>,
        target: CastTarget,
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

// ---- lexer ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Int(i64),
    Real(f64),
    Str(String),
    Param,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semi,
    DColon, // `::` type cast
    Dot,
    Star,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Slash,
    Percent,
    VecL2,     // <->
    VecCosine, // <=>
    VecIp,     // <#>
    Eof,
}

fn lex(sql: &str) -> Result<Vec<Tok>> {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // line comment: `-- ... <eol>`
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if let Some(tok) = simple_token(c) {
            out.push(tok);
            i += 1;
            continue;
        }
        let (tok, next) = match c {
            b'<' | b'>' | b'!' => lex_operator(b, i)?,
            b':' => lex_colon(b, i)?,
            b'\'' => lex_string(b, i)?,
            b'"' => lex_quoted_ident(b, i)?,
            _ if c.is_ascii_digit() => lex_number(b, i)?,
            _ if c == b'_' || c.is_ascii_alphabetic() => lex_word(b, i),
            other => {
                return Err(EngineError::sql(format!(
                    "unexpected character '{}'",
                    other as char
                )))
            }
        };
        out.push(tok);
        i = next;
    }
    out.push(Tok::Eof);
    Ok(out)
}

/// Single-byte tokens with no multi-character form.
fn simple_token(c: u8) -> Option<Tok> {
    Some(match c {
        b'(' => Tok::LParen,
        b')' => Tok::RParen,
        b'[' => Tok::LBracket,
        b']' => Tok::RBracket,
        b',' => Tok::Comma,
        b';' => Tok::Semi,
        b'.' => Tok::Dot,
        b'*' => Tok::Star,
        b'+' => Tok::Plus,
        b'-' => Tok::Minus,
        b'/' => Tok::Slash,
        b'%' => Tok::Percent,
        b'?' => Tok::Param,
        b'=' => Tok::Eq,
        _ => return None,
    })
}

/// Comparison operators with optional second character (`<=`, `<>`, `>=`, `!=`)
/// plus the three-character vector distance operators (`<->`, `<=>`, `<#>`).
fn lex_operator(b: &[u8], i: usize) -> Result<(Tok, usize)> {
    let two = b.get(i + 1).copied();
    Ok(match b[i] {
        b'<' => lex_lt(b, i),
        b'>' => match two {
            Some(b'=') => (Tok::Ge, i + 2),
            _ => (Tok::Gt, i + 1),
        },
        _ => match two {
            // `!`
            Some(b'=') => (Tok::Ne, i + 2),
            _ => return Err(EngineError::sql("unexpected '!'")),
        },
    })
}

/// `::` is the only colon form the engine accepts (the Postgres type cast); a
/// lone `:` is not valid SQL here.
fn lex_colon(b: &[u8], i: usize) -> Result<(Tok, usize)> {
    if b.get(i + 1) == Some(&b':') {
        Ok((Tok::DColon, i + 2))
    } else {
        Err(EngineError::sql("unexpected ':'"))
    }
}

/// Disambiguate `<`: the three-char distance operators bind first, then `<=` /
/// `<>`, then bare `<`.
fn lex_lt(b: &[u8], i: usize) -> (Tok, usize) {
    let two = b.get(i + 1).copied();
    let three = b.get(i + 2).copied();
    match (two, three) {
        (Some(b'-'), Some(b'>')) => (Tok::VecL2, i + 3),
        (Some(b'='), Some(b'>')) => (Tok::VecCosine, i + 3),
        (Some(b'#'), Some(b'>')) => (Tok::VecIp, i + 3),
        (Some(b'='), _) => (Tok::Le, i + 2),
        (Some(b'>'), _) => (Tok::Ne, i + 2),
        _ => (Tok::Lt, i + 1),
    }
}

/// String literal; `''` escapes a single quote. `i` points at the opening quote.
fn lex_string(b: &[u8], mut i: usize) -> Result<(Tok, usize)> {
    i += 1;
    let mut s = String::new();
    loop {
        let Some(&ch) = b.get(i) else {
            return Err(EngineError::sql("unterminated string literal"));
        };
        if ch == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                s.push('\'');
                i += 2;
            } else {
                return Ok((Tok::Str(s), i + 1));
            }
        } else {
            s.push(ch as char);
            i += 1;
        }
    }
}

/// Double-quoted identifier. `i` points at the opening quote.
fn lex_quoted_ident(b: &[u8], mut i: usize) -> Result<(Tok, usize)> {
    i += 1;
    let start = i;
    while i < b.len() && b[i] != b'"' {
        i += 1;
    }
    if i >= b.len() {
        return Err(EngineError::sql("unterminated quoted identifier"));
    }
    let ident = std::str::from_utf8(&b[start..i]).unwrap().to_string();
    Ok((Tok::Word(ident), i + 1))
}

/// Integer or real literal, with optional fraction and exponent.
fn lex_number(b: &[u8], mut i: usize) -> Result<(Tok, usize)> {
    let start = i;
    let mut is_real = false;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
        if b[i] == b'.' {
            is_real = true;
        }
        i += 1;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        is_real = true;
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    let text = std::str::from_utf8(&b[start..i]).unwrap();
    let parse_real = || text.parse().map_err(|_| EngineError::sql("bad number"));
    let tok = if is_real {
        Tok::Real(parse_real()?)
    } else {
        match text.parse::<i64>() {
            Ok(n) => Tok::Int(n),
            Err(_) => Tok::Real(parse_real()?), // out of i64 range -> real
        }
    };
    Ok((tok, i))
}

/// Bare identifier / keyword (`[A-Za-z_][A-Za-z0-9_]*`).
fn lex_word(b: &[u8], mut i: usize) -> (Tok, usize) {
    let start = i;
    while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
        i += 1;
    }
    let word = std::str::from_utf8(&b[start..i]).unwrap().to_string();
    (Tok::Word(word), i)
}

// ---- parser ---------------------------------------------------------------

/// Parse a single SQL statement, returning it plus the number of `?` parameters.
pub fn parse(sql: &str) -> Result<(Stmt, usize)> {
    let toks = lex(sql)?;
    let mut p = Parser {
        toks,
        pos: 0,
        next_param: 1,
    };
    let stmt = p.statement()?;
    p.skip(Tok::Semi);
    if p.peek() != &Tok::Eof {
        return Err(EngineError::sql("trailing tokens after statement"));
    }
    Ok((stmt, p.next_param - 1))
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    next_param: usize,
}

impl Parser {
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

    fn statement(&mut self) -> Result<Stmt> {
        if self.is_kw("create") {
            if self.next_is_kw("index") {
                self.create_index()
            } else {
                self.create_table()
            }
        } else if self.is_kw("drop") {
            if self.next_is_kw("index") {
                self.drop_index()
            } else {
                self.drop_table()
            }
        } else if self.is_kw("insert") {
            self.insert()
        } else if self.is_kw("select") {
            Ok(Stmt::Select(self.select()?))
        } else if self.is_kw("update") {
            self.update()
        } else if self.is_kw("delete") {
            self.delete()
        } else if self.eat_kw("begin") {
            let _ = self.eat_kw("transaction") || self.eat_kw("work") || self.eat_kw("deferred");
            Ok(Stmt::Begin)
        } else if self.eat_kw("start") {
            let _ = self.eat_kw("transaction");
            Ok(Stmt::Begin)
        } else if self.eat_kw("commit") {
            let _ = self.eat_kw("transaction") || self.eat_kw("work");
            Ok(Stmt::Commit)
        } else if self.eat_kw("rollback") {
            let _ = self.eat_kw("transaction") || self.eat_kw("work");
            Ok(Stmt::Rollback)
        } else {
            Err(EngineError::sql(format!(
                "unsupported statement starting at {:?}",
                self.peek()
            )))
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
        loop {
            let col = self.column_spec()?;
            columns.push(col);
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
            if_not_exists,
        })
    }

    fn column_spec(&mut self) -> Result<ColumnSpec> {
        let name = self.ident()?;
        // Optional type name (one or more words, optional (n) / (n,m)).
        let mut ty = ColumnType::Text;
        if let Some(w) = self.peek_word() {
            if !is_constraint_kw(w) {
                let tyname = self.ident()?;
                ty = ColumnType::from_sql(&tyname);
                ty = self.parse_type_size(ty)?;
            }
        }
        let mut primary_key = false;
        let mut not_null = false;
        loop {
            if self.eat_kw("primary") {
                self.expect_kw("key")?;
                primary_key = true;
                not_null = true;
            } else if self.eat_kw("not") {
                self.expect_kw("null")?;
                not_null = true;
            } else if self.eat_kw("null") {
                // explicit nullable
            } else if self.eat_kw("unique") {
                // treated like a constraint marker; uniqueness enforced for PK only in P1
            } else {
                break;
            }
        }
        Ok(ColumnSpec {
            name,
            ty,
            primary_key,
            not_null,
        })
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

    fn insert(&mut self) -> Result<Stmt> {
        self.expect_kw("insert")?;
        // Accept (and ignore) an optional `OR REPLACE` / `OR IGNORE` clause.
        if self.eat_kw("or") {
            let _ = self.eat_kw("replace") || self.eat_kw("ignore");
        }
        self.expect_kw("into")?;
        let table = self.ident()?;
        let columns = if self.skip(Tok::LParen) {
            let mut cols = Vec::new();
            loop {
                cols.push(self.ident()?);
                if self.skip(Tok::Comma) {
                    continue;
                }
                break;
            }
            self.expect(Tok::RParen)?;
            Some(cols)
        } else {
            None
        };
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
        Ok(Stmt::Insert {
            table,
            columns,
            rows,
        })
    }

    fn select(&mut self) -> Result<SelectStmt> {
        self.expect_kw("select")?;
        let _ = self.eat_kw("all"); // ALL is the default; DISTINCT unsupported in P1
        if self.is_kw("distinct") {
            return Err(EngineError::sql(
                "SELECT DISTINCT is not supported in Phase 1",
            ));
        }
        let mut items = Vec::new();
        loop {
            items.push(self.select_item()?);
            if self.skip(Tok::Comma) {
                continue;
            }
            break;
        }
        let from = if self.eat_kw("from") {
            Some(self.ident()?)
        } else {
            None
        };
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        let mut order_by = Vec::new();
        if self.eat_kw("order") {
            self.expect_kw("by")?;
            loop {
                let e = self.expr()?;
                let asc = if self.eat_kw("desc") {
                    false
                } else {
                    let _ = self.eat_kw("asc");
                    true
                };
                order_by.push((e, asc));
                if self.skip(Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        let limit = if self.eat_kw("limit") {
            match self.bump() {
                Tok::Int(n) => Some(n),
                other => {
                    return Err(EngineError::sql(format!(
                        "LIMIT expects an integer, found {other:?}"
                    )))
                }
            }
        } else {
            None
        };
        Ok(SelectStmt {
            items,
            from,
            filter,
            order_by,
            limit,
        })
    }

    fn select_item(&mut self) -> Result<SelItem> {
        if self.peek() == &Tok::Star {
            self.bump();
            return Ok(SelItem::Star);
        }
        // Aggregate function?
        if let Some(w) = self.peek_word() {
            if let Some(func) = agg_func(w) {
                // lookahead: must be followed by '('
                if self.toks.get(self.pos + 1) == Some(&Tok::LParen) {
                    self.bump(); // func name
                    self.expect(Tok::LParen)?;
                    let arg = if self.peek() == &Tok::Star {
                        self.bump();
                        AggArg::Star
                    } else {
                        AggArg::Expr(self.expr()?)
                    };
                    self.expect(Tok::RParen)?;
                    // an aggregate may carry trailing `::type` casts, e.g.
                    // `count(*)::int` (common from Bun.sql and PostgREST)
                    let mut cast = None;
                    while self.skip(Tok::DColon) {
                        cast = Some(self.parse_cast_type()?);
                    }
                    let alias = self.opt_alias()?;
                    return Ok(SelItem::Aggregate {
                        func,
                        arg,
                        cast,
                        alias,
                    });
                }
            }
        }
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
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Update {
            table,
            sets,
            filter,
        })
    }

    fn delete(&mut self) -> Result<Stmt> {
        self.expect_kw("delete")?;
        self.expect_kw("from")?;
        let table = self.ident()?;
        let filter = if self.eat_kw("where") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Stmt::Delete { table, filter })
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
        // [NOT] LIKE
        let not_like = if self.is_kw("not")
            && self
                .toks
                .get(self.pos + 1)
                .map(|t| matches!(t, Tok::Word(w) if w.eq_ignore_ascii_case("like")))
                .unwrap_or(false)
        {
            self.bump(); // not
            true
        } else {
            false
        };
        if self.eat_kw("like") {
            let pattern = self.vecdist_expr()?;
            return Ok(Expr::Like {
                e: Box::new(left),
                pattern: Box::new(pattern),
                negated: not_like,
            });
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

    /// Vector distance operators bind tighter than comparison but looser than
    /// arithmetic, so `embedding <-> ? < 0.5` parses as `(embedding <-> ?) < 0.5`
    /// and `a + 1 <-> b` as `(a + 1) <-> b`.
    fn vecdist_expr(&mut self) -> Result<Expr> {
        let mut left = self.add_expr()?;
        loop {
            let op = match self.peek() {
                Tok::VecL2 => BinOp::VecL2,
                Tok::VecCosine => BinOp::VecCosine,
                Tok::VecIp => BinOp::VecIp,
                _ => break,
            };
            self.bump();
            let right = self.add_expr()?;
            left = Expr::Binary {
                op,
                l: Box::new(left),
                r: Box::new(right),
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
            "timestamp" | "time" => {
                if self.eat_kw("with") || self.eat_kw("without") {
                    self.eat_kw("time");
                    self.eat_kw("zone");
                }
            }
            _ => {}
        }
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
            Tok::Param => {
                self.bump();
                let idx = self.next_param;
                self.next_param += 1;
                Ok(Expr::Param(idx))
            }
            Tok::LParen => {
                self.bump();
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
                } else {
                    self.bump();
                    // table.column -> use column part (single-table queries in P1)
                    if self.skip(Tok::Dot) {
                        let col = self.ident()?;
                        Ok(Expr::Column(col))
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
        _ => None,
    }
}

fn is_constraint_kw(w: &str) -> bool {
    ["primary", "not", "null", "unique"]
        .iter()
        .any(|k| w.eq_ignore_ascii_case(k))
}

fn is_clause_kw(w: &str) -> bool {
    [
        "from", "where", "order", "limit", "group", "having", "as", "and", "or", "is", "like",
        "asc", "desc",
    ]
    .iter()
    .any(|k| w.eq_ignore_ascii_case(k))
}
