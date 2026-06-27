//! Interactive wizard: fill the scaffolding fields that weren't given on the
//! command line by prompting for them.
//!
//! The flow is pure over its `(read, write)` streams, so it is unit-testable
//! with canned input — the binary wires it to stdin/stdout, but only when stdin
//! is a TTY (a non-terminal stdin, e.g. CI or a pipe, never enters the wizard,
//! so automation can't hang waiting for input). Any field already supplied as a
//! flag is passed in as `Some(..)` and is not prompted.

use std::io::{self, BufRead, Write};

use crate::scaffold::{validate_name, Backend, Client};

/// The fully-resolved answers the wizard produces.
pub struct Answers {
    pub name: String,
    pub client: Client,
    pub backend: Backend,
    pub vector: bool,
}

/// Run the wizard. Returns `Ok(None)` if the user declines the final
/// confirmation. `need_name` is false for `init` (the name comes from the
/// directory and is passed in as `name`).
pub fn wizard<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    name: Option<String>,
    client: Option<Client>,
    backend: Option<Backend>,
    vector: Option<bool>,
    need_name: bool,
) -> io::Result<Option<Answers>> {
    writeln!(out, "twilldb — new project\n")?;

    let name = match name {
        Some(n) => n,
        None if need_name => prompt_name(input, out)?,
        // `init` always passes Some(name); this arm is unreachable in practice.
        None => "app".to_string(),
    };
    let client = match client {
        Some(c) => c,
        None => prompt_client(input, out)?,
    };
    let backend = match backend {
        Some(b) => b,
        None => prompt_backend(input, out)?,
    };
    let vector = match vector {
        Some(v) => v,
        None => prompt_yes_no(input, out, "Include a vector-search (HNSW) starter?", false)?,
    };

    // Summary + confirmation before writing anything.
    writeln!(out, "\nabout to create:")?;
    writeln!(out, "  name     {name}")?;
    writeln!(out, "  client   {}", client.as_str())?;
    writeln!(out, "  backend  {}", backend_label(backend))?;
    writeln!(out, "  vector   {}", if vector { "yes" } else { "no" })?;
    if !prompt_yes_no(input, out, "\nproceed?", true)? {
        return Ok(None);
    }

    Ok(Some(Answers {
        name,
        client,
        backend,
        vector,
    }))
}

fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::File => "file",
        Backend::S3 => "s3",
    }
}

/// Read one trimmed line. `Ok(None)` signals EOF (no more input).
fn read_line<R: BufRead>(input: &mut R) -> io::Result<Option<String>> {
    let mut s = String::new();
    if input.read_line(&mut s)? == 0 {
        return Ok(None);
    }
    Ok(Some(s.trim().to_string()))
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "end of input while prompting")
}

fn prompt_name<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> io::Result<String> {
    loop {
        write!(out, "Project name: ")?;
        out.flush()?;
        match read_line(input)? {
            None => return Err(eof()),
            Some(line) if line.is_empty() => writeln!(out, "  a name is required.")?,
            Some(line) => match validate_name(&line) {
                Ok(()) => return Ok(line),
                Err(e) => writeln!(out, "  {e}")?,
            },
        }
    }
}

fn prompt_client<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> io::Result<Client> {
    loop {
        write!(out, "Client [bun] (node/php/rust coming soon): ")?;
        out.flush()?;
        match read_line(input)? {
            // Empty line or EOF accepts the default.
            None => return Ok(Client::Bun),
            Some(s) if s.is_empty() => return Ok(Client::Bun),
            Some(s) => match Client::parse(&s) {
                Ok(c) if c.available() => return Ok(c),
                Ok(c) => writeln!(
                    out,
                    "  the '{}' client is coming soon — choose 'bun' for now.",
                    c.as_str()
                )?,
                Err(e) => writeln!(out, "  {e}")?,
            },
        }
    }
}

fn prompt_backend<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> io::Result<Backend> {
    loop {
        write!(out, "Backend [file] (file/s3): ")?;
        out.flush()?;
        match read_line(input)? {
            None => return Ok(Backend::File),
            Some(s) if s.is_empty() => return Ok(Backend::File),
            Some(s) => match Backend::parse(&s) {
                Ok(b) => return Ok(b),
                Err(e) => writeln!(out, "  {e}")?,
            },
        }
    }
}

fn prompt_yes_no<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    question: &str,
    default: bool,
) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        write!(out, "{question} {hint}: ")?;
        out.flush()?;
        match read_line(input)? {
            None => return Ok(default),
            Some(s) if s.is_empty() => return Ok(default),
            Some(s) => match s.to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => writeln!(out, "  please answer y or n.")?,
            },
        }
    }
}
