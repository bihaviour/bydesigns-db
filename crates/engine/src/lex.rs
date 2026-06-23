//! The SQL lexer: turns source text into a flat token stream the
//! recursive-descent parser in [`crate::sql`] consumes. Kept in its own module
//! so the parser file stays focused on grammar.

use crate::error::{EngineError, Result};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
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
    Concat,    // `||` string concatenation
    VecL2,     // <->
    VecCosine, // <=>
    VecIp,     // <#>
    Eof,
}

pub(crate) fn lex(sql: &str) -> Result<Vec<Tok>> {
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
            b'|' => lex_pipe(b, i)?,
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

/// `||` is the string-concatenation operator; a lone `|` (bitwise OR) is out of
/// scope and rejected rather than mis-parsed.
fn lex_pipe(b: &[u8], i: usize) -> Result<(Tok, usize)> {
    if b.get(i + 1) == Some(&b'|') {
        Ok((Tok::Concat, i + 2))
    } else {
        Err(EngineError::sql(
            "unexpected '|' (bitwise OR is unsupported)",
        ))
    }
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
