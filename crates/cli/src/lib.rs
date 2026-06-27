//! `twilldb` — project scaffolding for the Twill DB engine.
//!
//! Generates a ready-to-run starter app for a chosen client (Bun today; node /
//! php / rust on the roadmap), selecting the storage backend purely by the
//! connection string it writes. The binary (`src/main.rs`) is a thin shim over
//! [`cli_main`]; [`run_cli`] is the dispatch core, factored out so tests can
//! assert exit codes without spawning a process — the same shape as
//! `twill-bench`.

pub mod scaffold;

use std::path::PathBuf;

use scaffold::{Backend, Client, Request};

/// Process exit codes.
pub mod exit {
    /// Success.
    pub const OK: i32 = 0;
    /// Generation failure (I/O, refused overwrite).
    pub const ERROR: i32 = 1;
    /// Bad flags / usage / unknown subcommand.
    pub const USAGE: i32 = 2;
}

/// CLI entry point (the `twilldb` binary is a thin shim over this). Computes the
/// exit code and terminates the process with it.
pub fn cli_main() {
    std::process::exit(run_cli(&std::env::args().collect::<Vec<_>>()));
}

/// The dispatch core, factored out of [`cli_main`] so tests can assert exit
/// codes without spawning a process.
pub fn run_cli(args: &[String]) -> i32 {
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let rest = &args[2.min(args.len())..];

    match cmd {
        "help" | "-h" | "--help" => {
            print_help();
            exit::OK
        }
        "version" | "-V" | "--version" => {
            println!("twilldb {}", env!("CARGO_PKG_VERSION"));
            exit::OK
        }
        "new" => run_new(rest),
        "init" => run_init(rest),
        other => {
            eprintln!("error: unknown subcommand '{other}'\n");
            print_help();
            exit::USAGE
        }
    }
}

/// Parsed `--flags`, shared by `new` and `init`.
struct Flags {
    client: Client,
    backend: Backend,
    vector: bool,
}

/// Split `args` into positionals and flags. Returns a usage error string on a
/// bad flag or a flag missing its value.
fn parse_flags(args: &[String]) -> Result<(Vec<String>, Flags), String> {
    let mut positionals = Vec::new();
    let mut client = Client::Bun;
    let mut backend = Backend::File;
    let mut vector = false;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--client" | "-c" => client = Client::parse(value(&mut it, "--client")?)?,
            "--backend" | "-b" => backend = Backend::parse(value(&mut it, "--backend")?)?,
            "--vector" => vector = true,
            s if s.starts_with('-') => return Err(format!("unknown flag '{s}'")),
            other => positionals.push(other.to_string()),
        }
    }
    Ok((
        positionals,
        Flags {
            client,
            backend,
            vector,
        },
    ))
}

/// Pull the next value for a flag, or report it as missing.
fn value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a str, String> {
    it.next()
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn run_new(args: &[String]) -> i32 {
    let (positionals, flags) = match parse_flags(args) {
        Ok(v) => v,
        Err(e) => return usage_err(&e),
    };
    let name = match positionals.as_slice() {
        [name] => name.clone(),
        [] => return usage_err("new requires a project name: twilldb new <name>"),
        _ => return usage_err("new takes a single project name"),
    };
    if let Err(e) = check_name(&name) {
        return usage_err(&e);
    }
    let req = Request {
        dir: PathBuf::from(&name),
        name,
        client: flags.client,
        backend: flags.backend,
        vector: flags.vector,
    };
    generate(&req, true)
}

fn run_init(args: &[String]) -> i32 {
    let (positionals, flags) = match parse_flags(args) {
        Ok(v) => v,
        Err(e) => return usage_err(&e),
    };
    if !positionals.is_empty() {
        return usage_err(
            "init takes no positional arguments (it scaffolds into the current directory)",
        );
    }
    let dir = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot read current directory: {e}");
            return exit::ERROR;
        }
    };
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("app")
        .to_string();
    if let Err(e) = check_name(&name) {
        return usage_err(&format!(
            "current directory name '{name}' is not a usable project name ({e}); \
             use `twilldb new <name>` instead"
        ));
    }
    let req = Request {
        name,
        dir,
        client: flags.client,
        backend: flags.backend,
        vector: flags.vector,
    };
    generate(&req, false)
}

/// Run generation and print the outcome.
fn generate(req: &Request, is_new: bool) -> i32 {
    match scaffold::generate(req, is_new) {
        Ok(written) => {
            print_success(req, is_new, written.len());
            exit::OK
        }
        Err(e) => {
            eprintln!("error: {e}");
            e.code()
        }
    }
}

/// Reject names that would escape the target directory or break the package
/// manifest. Permissive otherwise.
fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".into());
    }
    if name.starts_with('-') {
        return Err("name may not start with '-'".into());
    }
    if name == "." || name == ".." {
        return Err("name may not be '.' or '..'".into());
    }
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err("name may not contain path separators or '..'".into());
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("name may not contain whitespace or control characters".into());
    }
    Ok(())
}

fn usage_err(msg: &str) -> i32 {
    eprintln!("error: {msg}\n");
    print_help();
    exit::USAGE
}

fn print_success(req: &Request, is_new: bool, count: usize) {
    let vec = if req.vector { ", vector starter" } else { "" };
    if is_new {
        println!(
            "created {}/ ({} files — {} client, {} backend{})\n",
            req.dir.display(),
            count,
            req.client.as_str(),
            backend_label(req.backend),
            vec
        );
        println!("next steps:");
        println!("  cd {}", req.name);
        println!("  bun install");
        println!("  bun run start");
    } else {
        println!(
            "scaffolded into {} ({} files — {} client, {} backend{})\n",
            req.dir.display(),
            count,
            req.client.as_str(),
            backend_label(req.backend),
            vec
        );
        println!("next steps:");
        println!("  bun install");
        println!("  bun run start");
    }
}

fn backend_label(backend: Backend) -> &'static str {
    match backend {
        Backend::File => "file",
        Backend::S3 => "s3",
    }
}

fn print_help() {
    eprintln!(
        "twilldb — scaffold a Twill DB starter project\n\
         \n\
         usage:\n\
         \x20 twilldb new <name> [options]   create a new project in ./<name>\n\
         \x20 twilldb init [options]         scaffold into the current directory\n\
         \x20 twilldb version                print the version\n\
         \x20 twilldb help                   show this help\n\
         \n\
         options:\n\
         \x20 -c, --client <bun>             client ecosystem (default: bun)\n\
         \x20                                node, php, rust are on the roadmap\n\
         \x20 -b, --backend <file|s3>        storage backend / connection string (default: file)\n\
         \x20     --vector                   include a vector-search (HNSW) starter\n\
         \n\
         examples:\n\
         \x20 twilldb new notes\n\
         \x20 twilldb new search --vector\n\
         \x20 twilldb new app --backend s3\n\
         \x20 twilldb init"
    );
}
