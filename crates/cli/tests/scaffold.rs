//! Behaviour tests for the `twilldb` scaffolder. These assert the generated file
//! tree and the dispatch exit codes without spawning a process.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use twilldb_cli::scaffold::{self, Backend, Client, GenError, Request};
use twilldb_cli::{exit, run_cli};

/// A unique scratch directory under the OS temp dir for one test.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("twilldb-cli-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn req(name: &str, dir: PathBuf) -> Request {
    Request {
        name: name.to_string(),
        dir,
        client: Client::Bun,
        backend: Backend::File,
        vector: false,
    }
}

#[test]
fn bun_starter_writes_expected_tree() {
    let base = scratch("tree");
    let project = base.join("notes");
    let r = req("notes", project.clone());

    let written = scaffold::generate(&r, true).expect("generate");
    assert_eq!(written.len(), 5, "default bun starter writes 5 files");

    for f in [
        "package.json",
        "tsconfig.json",
        ".gitignore",
        "app.ts",
        "README.md",
    ] {
        assert!(project.join(f).exists(), "missing {f}");
    }
    // No vector file unless requested.
    assert!(!project.join("vectors.ts").exists());

    fs::remove_dir_all(&base).ok();
}

#[test]
fn package_json_references_client_scope_and_version() {
    let base = scratch("pkg");
    let r = req("myapp", base.join("myapp"));
    scaffold::generate(&r, true).unwrap();

    let pkg = fs::read_to_string(base.join("myapp/package.json")).unwrap();
    assert!(
        pkg.contains("\"name\": \"myapp\""),
        "project name substituted"
    );
    assert!(pkg.contains("@twilldb/bun"), "depends on the bun client");
    assert!(
        pkg.contains(env!("CARGO_PKG_VERSION")),
        "pins the workspace version"
    );
    // No placeholders survive.
    assert!(
        !pkg.contains("{{"),
        "unsubstituted placeholder in package.json"
    );

    fs::remove_dir_all(&base).ok();
}

#[test]
fn backend_selects_connection_string() {
    let base = scratch("backend");

    let mut file_req = req("f", base.join("f"));
    file_req.backend = Backend::File;
    scaffold::generate(&file_req, true).unwrap();
    let app = fs::read_to_string(base.join("f/app.ts")).unwrap();
    assert!(app.contains("file://./f.db"), "file backend url");

    let mut s3_req = req("s", base.join("s"));
    s3_req.backend = Backend::S3;
    scaffold::generate(&s3_req, true).unwrap();
    let app = fs::read_to_string(base.join("s/app.ts")).unwrap();
    assert!(app.contains("s3://your-bucket/s"), "s3 backend url");

    fs::remove_dir_all(&base).ok();
}

#[test]
fn vector_flag_adds_starter_and_script() {
    let base = scratch("vector");
    let mut r = req("vec", base.join("vec"));
    r.vector = true;
    let written = scaffold::generate(&r, true).unwrap();
    assert_eq!(written.len(), 6, "vector starter adds vectors.ts");

    assert!(base.join("vec/vectors.ts").exists());
    let pkg = fs::read_to_string(base.join("vec/package.json")).unwrap();
    assert!(
        pkg.contains("\"vectors\""),
        "vectors script wired into package.json"
    );
    let readme = fs::read_to_string(base.join("vec/README.md")).unwrap();
    assert!(readme.contains("Vector search"), "vector section in README");

    fs::remove_dir_all(&base).ok();
}

#[test]
fn refuses_to_overwrite_existing_files() {
    let base = scratch("clobber");
    let project = base.join("p");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("package.json"), "{}").unwrap();

    let r = req("p", project.clone());
    // is_new = false (like `init`) so we exercise the per-file conflict path.
    let err = scaffold::generate(&r, false).unwrap_err();
    assert!(matches!(err, GenError::Conflict(_)), "got {err:?}");
    assert_eq!(err.code(), exit::ERROR);
    // The pre-existing file is untouched.
    assert_eq!(
        fs::read_to_string(project.join("package.json")).unwrap(),
        "{}"
    );

    fs::remove_dir_all(&base).ok();
}

#[test]
fn new_refuses_non_empty_dir() {
    let base = scratch("nonempty");
    let project = base.join("p");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("unrelated.txt"), "hi").unwrap();

    let r = req("p", project);
    let err = scaffold::generate(&r, true).unwrap_err();
    assert!(matches!(err, GenError::Conflict(_)), "got {err:?}");

    fs::remove_dir_all(&base).ok();
}

#[test]
fn unavailable_client_is_a_usage_error() {
    let base = scratch("unavail");
    let mut r = req("p", base.join("p"));
    r.client = Client::Rust; // rust is still roadmap-only
    let err = scaffold::generate(&r, true).unwrap_err();
    assert!(matches!(err, GenError::Unavailable(_)), "got {err:?}");
    assert_eq!(err.code(), exit::USAGE);
    // Nothing should have been written.
    assert!(!base.join("p").exists());

    fs::remove_dir_all(&base).ok();
}

#[test]
fn node_starter_writes_expected_tree() {
    let base = scratch("node-tree");
    let project = base.join("web");
    let mut r = req("web", project.clone());
    r.client = Client::Node;

    let written = scaffold::generate(&r, true).expect("generate");
    assert_eq!(written.len(), 5, "default node starter writes 5 files");
    for f in [
        "package.json",
        "tsconfig.json",
        ".gitignore",
        "app.ts",
        "README.md",
    ] {
        assert!(project.join(f).exists(), "missing {f}");
    }
    let pkg = fs::read_to_string(project.join("package.json")).unwrap();
    assert!(pkg.contains("@twilldb/node"), "depends on the node client");
    assert!(
        pkg.contains("\"start\": \"node app.ts\""),
        "node start script"
    );
    let app = fs::read_to_string(project.join("app.ts")).unwrap();
    assert!(app.contains("@twilldb/node"), "app imports the node client");

    fs::remove_dir_all(&base).ok();
}

#[test]
fn php_starter_writes_expected_tree() {
    let base = scratch("php-tree");
    let project = base.join("api");
    let mut r = req("api", project.clone());
    r.client = Client::Php;

    let written = scaffold::generate(&r, true).expect("generate");
    assert_eq!(written.len(), 4, "default php starter writes 4 files");
    for f in ["composer.json", ".gitignore", "index.php", "README.md"] {
        assert!(project.join(f).exists(), "missing {f}");
    }
    // No TS files for the PHP starter.
    assert!(!project.join("app.ts").exists());
    let composer = fs::read_to_string(project.join("composer.json")).unwrap();
    assert!(
        composer.contains("twilldb/twilldb"),
        "requires the php package"
    );
    assert!(composer.contains("ext-ffi"), "declares the FFI requirement");
    let index = fs::read_to_string(project.join("index.php")).unwrap();
    assert!(
        index.contains("Twill\\Database"),
        "index uses the php client"
    );

    fs::remove_dir_all(&base).ok();
}

#[test]
fn node_and_php_vector_starters_use_native_extensions() {
    let base = scratch("vec-clients");

    let mut node = req("nv", base.join("nv"));
    node.client = Client::Node;
    node.vector = true;
    let n = scaffold::generate(&node, true).unwrap();
    assert_eq!(n.len(), 6, "node vector starter adds vectors.ts");
    assert!(base.join("nv/vectors.ts").exists());

    let mut php = req("pv", base.join("pv"));
    php.client = Client::Php;
    php.vector = true;
    let p = scaffold::generate(&php, true).unwrap();
    assert_eq!(p.len(), 5, "php vector starter adds vectors.php");
    assert!(base.join("pv/vectors.php").exists());

    fs::remove_dir_all(&base).ok();
}

#[test]
fn client_and_backend_parse_rejects_unknown() {
    assert!(Client::parse("bun").is_ok());
    assert!(Client::parse("node").is_ok()); // recognised, even if not generated
    assert!(Client::parse("cobol").is_err());
    assert!(Backend::parse("file").is_ok());
    assert!(Backend::parse("s3").is_ok());
    assert!(Backend::parse("ftp").is_err());
}

#[test]
fn files_render_is_pure_and_complete() {
    // The pure `files()` view should never leak a placeholder, for any client.
    for client in [Client::Bun, Client::Node, Client::Php] {
        let r = Request {
            name: "demo".into(),
            dir: PathBuf::from("demo"),
            client,
            backend: Backend::File,
            vector: true,
        };
        let rendered: HashMap<_, _> = scaffold::files(&r).into_iter().collect();
        for (path, content) in &rendered {
            assert!(
                !content.contains("{{"),
                "{client:?} {path} has an unsubstituted placeholder"
            );
        }
    }
}

// ---- dispatch (exit code) tests --------------------------------------------

fn argv(args: &[&str]) -> Vec<String> {
    std::iter::once("twilldb")
        .chain(args.iter().copied())
        .map(String::from)
        .collect()
}

#[test]
fn dispatch_help_and_version_are_ok() {
    assert_eq!(run_cli(&argv(&["help"])), exit::OK);
    assert_eq!(run_cli(&argv(&["--version"])), exit::OK);
    assert_eq!(run_cli(&argv(&[])), exit::OK); // no args ⇒ help
}

#[test]
fn dispatch_unknown_subcommand_is_usage() {
    assert_eq!(run_cli(&argv(&["frobnicate"])), exit::USAGE);
}

#[test]
fn dispatch_new_without_name_is_usage() {
    assert_eq!(run_cli(&argv(&["new"])), exit::USAGE);
}

#[test]
fn dispatch_bad_flag_is_usage() {
    assert_eq!(
        run_cli(&argv(&["new", "x", "--client", "cobol"])),
        exit::USAGE
    );
    assert_eq!(run_cli(&argv(&["new", "x", "--nope"])), exit::USAGE);
    assert_eq!(run_cli(&argv(&["new", "../escape"])), exit::USAGE);
}
