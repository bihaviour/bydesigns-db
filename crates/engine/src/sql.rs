//! A focused SQL frontend: lexer + recursive-descent parser producing the
//! engine's internal statement AST (spec 02 — parser stage). The supported
//! subset is deliberately small for Phase 1 (DDL + DML + queries + transaction
//! control); anything outside it is rejected with `ENGINE_ERR_SQL` rather than
//! silently mis-parsed.

use crate::error::{EngineError, Result};
use crate::value::ColumnType;

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
    Comma,
    Semi,
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
    Eof,
}

fn lex(sql: &str) -> Result<Vec<Tok>> {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            _ if c.is_ascii_whitespace() => i += 1,
            b'-' if i + 1 < b.len() && b[i + 1] == b'-' => {
                // line comment
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            b';' => {
                out.push(Tok::Semi);
                i += 1;
            }
            b'.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'?' => {
                out.push(Tok::Param);
                i += 1;
            }
            b'=' => {
                out.push(Tok::Eq);
                i += 1;
            }
            b'<' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tok::Le);
                    i += 2;
                } else if i + 1 < b.len() && b[i + 1] == b'>' {
                    out.push(Tok::Ne);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tok::Ge);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            b'!' => {
                if i + 1 < b.len() && b[i + 1] == b'=' {
                    out.push(Tok::Ne);
                    i += 2;
                } else {
                    return Err(EngineError::sql("unexpected '!'"));
                }
            }
            b'\'' => {
                // string literal, '' escapes a quote
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= b.len() {
                        return Err(EngineError::sql("unterminated string literal"));
                    }
                    if b[i] == b'\'' {
                        if i + 1 < b.len() && b[i + 1] == b'\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(b[i] as char);
                        i += 1;
                    }
                }
                out.push(Tok::Str(s));
            }
            b'"' => {
                // double-quoted identifier
                i += 1;
                let start = i;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                if i >= b.len() {
                    return Err(EngineError::sql("unterminated quoted identifier"));
                }
                let ident = std::str::from_utf8(&b[start..i]).unwrap().to_string();
                i += 1;
                out.push(Tok::Word(ident));
            }
            _ if c.is_ascii_digit() => {
                let start = i;
                let mut is_real = false;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    if b[i] == b'.' {
                        is_real = true;
                    }
                    i += 1;
                }
                // exponent
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
                if is_real {
                    out.push(Tok::Real(
                        text.parse().map_err(|_| EngineError::sql("bad number"))?,
                    ));
                } else {
                    match text.parse::<i64>() {
                        Ok(n) => out.push(Tok::Int(n)),
                        Err(_) => out.push(Tok::Real(
                            text.parse().map_err(|_| EngineError::sql("bad number"))?,
                        )),
                    }
                }
            }
            _ if c == b'_' || c.is_ascii_alphabetic() => {
                let start = i;
                while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                let word = std::str::from_utf8(&b[start..i]).unwrap().to_string();
                out.push(Tok::Word(word));
            }
            other => {
                return Err(EngineError::sql(format!(
                    "unexpected character '{}'",
                    other as char
                )))
            }
        }
    }
    out.push(Tok::Eof);
    Ok(out)
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

    fn statement(&mut self) -> Result<Stmt> {
        if self.is_kw("create") {
            self.create_table()
        } else if self.is_kw("drop") {
            self.drop_table()
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
                // optional (size) or (p,s)
                if self.skip(Tok::LParen) {
                    while self.peek() != &Tok::RParen && self.peek() != &Tok::Eof {
                        self.bump();
                    }
                    self.expect(Tok::RParen)?;
                }
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
                    let alias = self.opt_alias()?;
                    return Ok(SelItem::Aggregate { func, arg, alias });
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
        let left = self.add_expr()?;
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
            let pattern = self.add_expr()?;
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
            let right = self.add_expr()?;
            Ok(Expr::Binary {
                op,
                l: Box::new(left),
                r: Box::new(right),
            })
        } else {
            Ok(left)
        }
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
            self.primary()
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
