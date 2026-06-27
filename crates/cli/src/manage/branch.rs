//! Copy-on-write branch management (spec 19, Milestone 2) — `branch create`,
//! `branch list`, `branch delete`. Branching is the differentiator the CLI leans
//! into: a branch is a zero-copy fork at the database's committed LSN, so these
//! commands are thin drivers over the engine's branch consumer API
//! ([`engine::Connection::create_branch`] / [`list_branches`](engine::Connection::list_branches)
//! / [`delete_branch`](engine::Connection::delete_branch)).
//!
//! A branch is addressed by id; the CLI surfaces a ready-to-use connection
//! string (`<base-url>#branch=<id>`) that every other command accepts (see
//! [`super::open`]). Branch pointers are durable on the base, so a branch created
//! by one invocation is visible to `list` / `delete` in the next.

use super::{open_embedded, positional, CmdError::Usage, CmdResult};

/// `twilldb branch <create|list|delete> …`.
pub fn cmd_branch(args: &[String]) -> CmdResult {
    let sub = positional(args, 0, "a branch subcommand (create|list|delete)")?;
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match sub {
        "create" => create(&rest),
        "list" | "ls" => list(&rest),
        "delete" | "rm" => delete(&rest),
        other => Err(Usage(format!(
            "unknown branch subcommand '{other}' (create|list|delete)"
        ))),
    }
}

/// `branch create <url> [name]` — fork a branch at the base's committed LSN.
fn create(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    // The name is an optional human label; the engine identifies branches by id.
    let name = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .nth(1)
        .map(String::as_str)
        .unwrap_or("branch");
    let conn = open_embedded(url)?;
    let id = conn.create_branch(name).map_err(|e| e.to_string())?;
    Ok(format!(
        "created branch {} (\"{name}\")\n\
         address it with: {}#branch={}",
        id.0, url, id.0
    ))
}

/// `branch list <url>` — every branch forked off the base, in id order.
fn list(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let conn = open_embedded(url)?;
    let branches = conn.list_branches().map_err(|e| e.to_string())?;
    if branches.is_empty() {
        return Ok("(no branches)".to_string());
    }
    // A compact aligned listing — the id is what every other command addresses.
    let mut out = String::from("id    parent  base_lsn  head_lsn  address\n");
    for b in &branches {
        let parent = if b.parent.0 == 0 {
            "root".to_string()
        } else {
            b.parent.0.to_string()
        };
        out.push_str(&format!(
            "{:<5} {:<7} {:<9} {:<9} {}#branch={}\n",
            b.id.0, parent, b.base_lsn.0, b.head_lsn.0, url, b.id.0
        ));
    }
    Ok(out.trim_end().to_string())
}

/// `branch delete <url> <id>` — drop a branch pointer and its private overlay.
fn delete(args: &[String]) -> CmdResult {
    let url = positional(args, 0, "<url>")?;
    let id_str = positional(args, 1, "<branch-id>")?;
    let id: u64 = id_str
        .parse()
        .map_err(|_| Usage(format!("branch id must be a number, got '{id_str}'")))?;
    let conn = open_embedded(url)?;
    conn.delete_branch(engine::BranchId(id))
        .map_err(|e| e.to_string())?;
    Ok(format!("deleted branch {id}"))
}
