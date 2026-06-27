//! Project generation: turn a parsed [`Request`] into a starter file tree.
//!
//! Templates are embedded at compile time (`include_str!`) so the binary is
//! self-contained — no network fetch, works offline. Substitution is a plain
//! string replace of `{{token}}` placeholders; the project deliberately avoids a
//! template-engine dependency, matching the hand-rolled style of the rest of the
//! workspace.

use std::fs;
use std::path::{Path, PathBuf};

use crate::exit;

// ---- Embedded templates -----------------------------------------------------
//
// One template set per client ecosystem, under templates/<client>/. Each set is
// an embedded starter over the same engine; only the host binding differs (Bun:
// bun:ffi, Node: koffi FFI, PHP: the FFI extension). See spec 20 — Client
// Runtimes.

// Bun (the reference client).
const BUN_PACKAGE: &str = include_str!("../templates/bun/package.json.tmpl");
const BUN_TSCONFIG: &str = include_str!("../templates/bun/tsconfig.json.tmpl");
const BUN_GITIGNORE: &str = include_str!("../templates/bun/gitignore.tmpl");
const BUN_APP: &str = include_str!("../templates/bun/app.ts.tmpl");
const BUN_README: &str = include_str!("../templates/bun/README.md.tmpl");
const BUN_VECTORS: &str = include_str!("../templates/bun/vectors.ts.tmpl");

// Node (and Node-based frameworks: Next.js, Astro, Nuxt, …).
const NODE_PACKAGE: &str = include_str!("../templates/node/package.json.tmpl");
const NODE_TSCONFIG: &str = include_str!("../templates/node/tsconfig.json.tmpl");
const NODE_GITIGNORE: &str = include_str!("../templates/node/gitignore.tmpl");
const NODE_APP: &str = include_str!("../templates/node/app.ts.tmpl");
const NODE_README: &str = include_str!("../templates/node/README.md.tmpl");
const NODE_VECTORS: &str = include_str!("../templates/node/vectors.ts.tmpl");

// PHP (plain PHP + frameworks: Laravel, Symfony, CodeIgniter).
const PHP_COMPOSER: &str = include_str!("../templates/php/composer.json.tmpl");
const PHP_GITIGNORE: &str = include_str!("../templates/php/gitignore.tmpl");
const PHP_INDEX: &str = include_str!("../templates/php/index.php.tmpl");
const PHP_README: &str = include_str!("../templates/php/README.md.tmpl");
const PHP_VECTORS: &str = include_str!("../templates/php/vectors.php.tmpl");

/// Per-client README section + manifest script entry appended when `--vector`
/// is set. `(readme, script)`.
const BUN_VECTOR_README: &str = "\n## Vector search\n\nThis project includes a \
vector starter (`vectors.ts`):\n\n```bash\nbun run vectors\n```\n\nIt creates a \
`vector(3)` column, an HNSW index, and runs a top-k nearest-neighbour query.\n";
const BUN_VECTOR_SCRIPT: &str = ",\n    \"vectors\": \"bun run vectors.ts\"";

const NODE_VECTOR_README: &str = "\n## Vector search\n\nThis project includes a \
vector starter (`vectors.ts`):\n\n```bash\nnpm run vectors\n```\n\nIt creates a \
`vector(3)` column, an HNSW index, and runs a top-k nearest-neighbour query.\n";
const NODE_VECTOR_SCRIPT: &str = ",\n    \"vectors\": \"node vectors.ts\"";

const PHP_VECTOR_README: &str = "\n## Vector search\n\nThis project includes a \
vector starter (`vectors.php`):\n\n```bash\ncomposer vectors\n```\n\nIt creates a \
`vector(3)` column, an HNSW index, and runs a top-k nearest-neighbour query.\n";
const PHP_VECTOR_SCRIPT: &str = ",\n        \"vectors\": \"php -d ffi.enable=1 vectors.php\"";

// ---- Request model ----------------------------------------------------------

/// The client ecosystem a starter targets. Bun, Node, and PHP each generate a
/// working embedded starter over the same engine C ABI (see spec 20 — Client
/// Runtimes); `rust` is recognised so the CLI gives a roadmap-aware message
/// rather than "unknown value".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    Bun,
    Node,
    Php,
    Rust,
}

impl Client {
    pub fn parse(s: &str) -> Result<Client, String> {
        match s {
            "bun" => Ok(Client::Bun),
            "node" => Ok(Client::Node),
            "php" => Ok(Client::Php),
            "rust" => Ok(Client::Rust),
            other => Err(format!(
                "unknown --client '{other}' (expected: bun, node, php, rust)"
            )),
        }
    }

    /// Whether a starter for this client can be generated yet.
    pub fn available(self) -> bool {
        matches!(self, Client::Bun | Client::Node | Client::Php)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Client::Bun => "bun",
            Client::Node => "node",
            Client::Php => "php",
            Client::Rust => "rust",
        }
    }
}

/// Storage backend, which only affects the connection string written into the
/// starter — the engine selects the backend purely by URL scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    File,
    S3,
}

impl Backend {
    pub fn parse(s: &str) -> Result<Backend, String> {
        match s {
            "file" => Ok(Backend::File),
            "s3" => Ok(Backend::S3),
            other => Err(format!("unknown --backend '{other}' (expected: file, s3)")),
        }
    }

    /// The connection string written into the starter, given the project name.
    fn db_url(self, name: &str) -> String {
        match self {
            Backend::File => format!("file://./{name}.db"),
            Backend::S3 => format!("s3://your-bucket/{name}"),
        }
    }
}

/// A fully-resolved scaffolding request.
#[derive(Clone, Debug)]
pub struct Request {
    /// Project name (also the npm package name and, for `new`, the directory).
    pub name: String,
    /// Directory the files are written into.
    pub dir: PathBuf,
    pub client: Client,
    pub backend: Backend,
    pub vector: bool,
}

// ---- Errors -----------------------------------------------------------------

#[derive(Debug)]
pub enum GenError {
    /// The requested client isn't generated yet (a usage problem).
    Unavailable(String),
    /// Would overwrite existing files (refused).
    Conflict(String),
    /// Filesystem failure.
    Io(String),
}

impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenError::Unavailable(m) | GenError::Conflict(m) | GenError::Io(m) => f.write_str(m),
        }
    }
}

impl GenError {
    /// Process exit code for this failure class.
    pub fn code(&self) -> i32 {
        match self {
            GenError::Unavailable(_) => exit::USAGE,
            GenError::Conflict(_) | GenError::Io(_) => exit::ERROR,
        }
    }
}

// ---- Generation -------------------------------------------------------------

/// Render the file list (relative path → contents) for a request. Pure: writes
/// nothing, so tests can assert the tree directly. Each client has its own
/// template set, but the substitution tokens are shared.
pub fn files(req: &Request) -> Vec<(String, String)> {
    let db_url = req.backend.db_url(&req.name);
    let (vector_script, vector_readme) = vector_tokens(req.client, req.vector);
    let render = |tmpl: &str| {
        tmpl.replace("{{name}}", &req.name)
            .replace("{{version}}", env!("CARGO_PKG_VERSION"))
            .replace("{{db_url}}", &db_url)
            .replace("{{vector_script}}", vector_script)
            .replace("{{vector_readme}}", vector_readme)
    };

    match req.client {
        Client::Bun => {
            let mut out = vec![
                ("package.json".to_string(), render(BUN_PACKAGE)),
                ("tsconfig.json".to_string(), render(BUN_TSCONFIG)),
                (".gitignore".to_string(), render(BUN_GITIGNORE)),
                ("app.ts".to_string(), render(BUN_APP)),
                ("README.md".to_string(), render(BUN_README)),
            ];
            if req.vector {
                out.push(("vectors.ts".to_string(), render(BUN_VECTORS)));
            }
            out
        }
        Client::Node => {
            let mut out = vec![
                ("package.json".to_string(), render(NODE_PACKAGE)),
                ("tsconfig.json".to_string(), render(NODE_TSCONFIG)),
                (".gitignore".to_string(), render(NODE_GITIGNORE)),
                ("app.ts".to_string(), render(NODE_APP)),
                ("README.md".to_string(), render(NODE_README)),
            ];
            if req.vector {
                out.push(("vectors.ts".to_string(), render(NODE_VECTORS)));
            }
            out
        }
        Client::Php => {
            let mut out = vec![
                ("composer.json".to_string(), render(PHP_COMPOSER)),
                (".gitignore".to_string(), render(PHP_GITIGNORE)),
                ("index.php".to_string(), render(PHP_INDEX)),
                ("README.md".to_string(), render(PHP_README)),
            ];
            if req.vector {
                out.push(("vectors.php".to_string(), render(PHP_VECTORS)));
            }
            out
        }
        // Unavailable: `generate()` guards before reaching here.
        Client::Rust => Vec::new(),
    }
}

/// `(vector_script, vector_readme)` substitution values for a client, or empty
/// strings when `--vector` is off.
fn vector_tokens(client: Client, vector: bool) -> (&'static str, &'static str) {
    if !vector {
        return ("", "");
    }
    match client {
        Client::Bun => (BUN_VECTOR_SCRIPT, BUN_VECTOR_README),
        Client::Node => (NODE_VECTOR_SCRIPT, NODE_VECTOR_README),
        Client::Php => (PHP_VECTOR_SCRIPT, PHP_VECTOR_README),
        Client::Rust => ("", ""),
    }
}

/// Generate the project. `is_new` is true for `twilldb new` (the target dir must
/// be absent or empty); for `init` we only refuse to clobber individual files.
/// Never overwrites an existing file.
pub fn generate(req: &Request, is_new: bool) -> Result<Vec<PathBuf>, GenError> {
    if !req.client.available() {
        return Err(GenError::Unavailable(unavailable_msg(req.client)));
    }

    let files = files(req);

    if is_new && req.dir.exists() && dir_non_empty(&req.dir)? {
        return Err(GenError::Conflict(format!(
            "{} already exists and is not empty",
            req.dir.display()
        )));
    }

    let conflicts: Vec<&str> = files
        .iter()
        .filter(|(rel, _)| req.dir.join(rel).exists())
        .map(|(rel, _)| rel.as_str())
        .collect();
    if !conflicts.is_empty() {
        return Err(GenError::Conflict(format!(
            "refusing to overwrite existing file(s): {}",
            conflicts.join(", ")
        )));
    }

    let mut written = Vec::with_capacity(files.len());
    for (rel, content) in files {
        let path = req.dir.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| GenError::Io(format!("{}: {e}", parent.display())))?;
        }
        fs::write(&path, content).map_err(|e| GenError::Io(format!("{}: {e}", path.display())))?;
        written.push(path);
    }
    Ok(written)
}

/// The message shown when a not-yet-shipped client is requested.
pub fn unavailable_msg(client: Client) -> String {
    format!(
        "the '{}' client is not available yet — bun, node and php ship today.\n\
         twilldb's C ABI makes each language a thin binding; a native rust \
         starter is on the roadmap (see pages/specs/20-client-runtimes.html).",
        client.as_str()
    )
}

/// Reject names that would escape the target directory or break the package
/// manifest. Permissive otherwise. Shared by the flag path and the wizard.
pub fn validate_name(name: &str) -> Result<(), String> {
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

fn dir_non_empty(dir: &Path) -> Result<bool, GenError> {
    let mut entries =
        fs::read_dir(dir).map_err(|e| GenError::Io(format!("{}: {e}", dir.display())))?;
    Ok(entries.next().is_some())
}
