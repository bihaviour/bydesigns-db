//! Tests for the interactive wizard. It is pure over its (read, write) streams,
//! so we drive it with a byte cursor and assert the resolved answers — no TTY,
//! no process spawn.

use std::io::Cursor;

use twilldb_cli::prompt::{wizard, Answers};
use twilldb_cli::scaffold::{Backend, Client};

/// Run the wizard over canned stdin lines; returns the answers (or None if the
/// user declined at the confirmation step).
fn run(
    keystrokes: &str,
    name: Option<&str>,
    client: Option<Client>,
    backend: Option<Backend>,
    vector: Option<bool>,
    need_name: bool,
) -> Option<Answers> {
    let mut input = Cursor::new(keystrokes.as_bytes().to_vec());
    let mut out: Vec<u8> = Vec::new();
    wizard(
        &mut input,
        &mut out,
        name.map(String::from),
        client,
        backend,
        vector,
        need_name,
    )
    .expect("wizard io")
}

#[test]
fn prompts_for_every_missing_field() {
    // name, client(default), backend(default), vector=y, confirm(default yes)
    let a = run("notes\n\n\ny\n\n", None, None, None, None, true).expect("not aborted");
    assert_eq!(a.name, "notes");
    assert_eq!(a.client, Client::Bun);
    assert_eq!(a.backend, Backend::File);
    assert!(a.vector);
}

#[test]
fn empty_answers_take_the_defaults() {
    // name given as arg; everything else blank ⇒ bun / file / no-vector.
    let a = run("\n\n\n\n", Some("app"), None, None, None, true).expect("not aborted");
    assert_eq!(a.name, "app");
    assert_eq!(a.client, Client::Bun);
    assert_eq!(a.backend, Backend::File);
    assert!(!a.vector);
}

#[test]
fn flags_are_not_reprompted() {
    // All fields pre-supplied: the only line consumed is the final confirmation.
    let a = run(
        "\n",
        Some("app"),
        Some(Client::Bun),
        Some(Backend::S3),
        Some(true),
        true,
    )
    .expect("not aborted");
    assert_eq!(a.backend, Backend::S3);
    assert!(a.vector);
}

#[test]
fn backend_choice_is_honored() {
    let a = run("\ns3\nn\n\n", Some("x"), None, None, None, true).expect("not aborted");
    assert_eq!(a.backend, Backend::S3);
    assert!(!a.vector);
}

#[test]
fn unavailable_client_is_reprompted_until_valid() {
    // "node" (coming soon) is rejected, then "bun" accepted; then defaults.
    let a = run("x\nnode\nbun\n\n\n\n", None, None, None, None, true).expect("not aborted");
    assert_eq!(a.name, "x");
    assert_eq!(a.client, Client::Bun);
}

#[test]
fn invalid_name_is_reprompted() {
    // "../escape" fails validation, then "ok" is accepted.
    let a = run("../escape\nok\n\n\n\n\n", None, None, None, None, true).expect("not aborted");
    assert_eq!(a.name, "ok");
}

#[test]
fn declining_confirmation_aborts() {
    // Everything supplied; answer "n" at the proceed prompt.
    let aborted = run(
        "n\n",
        Some("app"),
        Some(Client::Bun),
        Some(Backend::File),
        Some(false),
        true,
    );
    assert!(aborted.is_none());
}

#[test]
fn init_does_not_prompt_for_name() {
    // need_name = false and name pre-set (as `init` does): no name line consumed.
    let a = run("\n\n\n\n", Some("my-dir"), None, None, None, false).expect("not aborted");
    assert_eq!(a.name, "my-dir");
}
