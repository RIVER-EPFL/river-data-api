//! Every route's authorization, pinned.
//!
//! Q143 splits router registration by prefix: a component's `views.rs` will own the routes under
//! its own prefix and carry their layers itself, while `service/mod.rs` keeps the generated entity
//! nests and the cross-cutting routes. Five moves are queued behind that (C271 to C275), each
//! re-declaring by hand the layers the central file shares today, and a move that drops a layer or
//! narrows it leaves every other test passing: the route answers, the handler runs, and only a
//! request from an account that should have been refused would show it.
//!
//! So the pairing is pinned here rather than left to review. This reads what the source declares,
//! not what the assembled router does, because axum exposes no way to ask a built `Router` which
//! layers a route carries. It therefore proves that a route's declared layers did not change; it
//! does not prove the layer does what its name says, which is what `tests/rbac/` is for.
//!
//! A route that moves between files keeps its entry: the table is keyed on the path as declared,
//! and Q143 has the component carry the same layers the central block applied, so a correct move
//! changes no row. A move that forgets a layer turns that route's row into one with fewer layers,
//! or `-`, and fails here.
//!
//! `-` means the declaring block applies no layer of its own. It does NOT mean unauthenticated,
//! and two things can put a guard somewhere this scan does not look: the whole `/api` router is
//! mounted behind `service_auth_middleware` at the mount point, and a handler may refuse a caller
//! itself (`/me` refuses an API token, which has no user sub). Every component that owns routes
//! now carries their layers in the block that declares them, so a guard applied at a nest site is
//! no longer one of them. The genuinely open surface is `/healthz`, `/readyz`, `/enroll`, which
//! authenticates on body credentials, and the public API under `/{project_code}`.
//!
//! A path can hold more than one row: the table is keyed on the path as declared, so `/services`
//! under a read group and `/services/{id}` under a write group are separate rows, and so are the
//! two `/events` (the SSE feed, and the sync service's own).

use std::path::{Path, PathBuf};

/// Every route and the authorization layers its block applies, `path => layers`, sorted. A route
/// under no layer at all is `-`, which is a claim worth making explicitly: it is either public or
/// it is guarded somewhere the scan cannot see.
const EXPECTED: &str = include_str!("route_guards.txt");

/// The source files that declare routers. A component that starts owning its routes is added here,
/// which is a deliberate edit at the moment the ownership moves.
fn router_sources() -> Vec<PathBuf> {
    let root = crate::test_crate_root().join("src/routes");
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    files
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src/routes") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// One `Router::new()` block: what it routes, what it nests or merges, and what guards it.
#[derive(Debug, Default, Clone)]
struct Block {
    routes: Vec<String>,
    layers: Vec<String>,
}

/// The blocks one source file declares, keyed by the name a caller reaches them through: the
/// function name for `pub fn read_routes() -> Router`, the binding name for `let x = Router::new()`.
///
/// A block runs from `Router::new()` to the next one, which is how these are written: a block's
/// `.route(...)` calls and the `.layer(...)` calls guarding them are contiguous.
fn blocks(source: &str) -> Vec<(String, Block)> {
    let live = source.split("#[cfg(test)]").next().unwrap_or(source);
    let mut out = Vec::new();
    let pieces: Vec<&str> = live.split("Router::new()").collect();
    for (index, body) in pieces.iter().enumerate().skip(1) {
        let name = binding_before(pieces[index - 1]);
        let mut block = Block {
            routes: captures(body, ".route("),
            ..Block::default()
        };
        for marker in ["from_fn(", "from_fn_with_state("] {
            for found in idents_after(body, marker) {
                if !block.layers.contains(&found) {
                    block.layers.push(found);
                }
            }
        }
        block.layers.sort();
        if block.routes.is_empty() {
            continue;
        }
        out.push((name, block));
    }
    out
}

/// The name a block is reached by, read backwards from the text before its `Router::new()`.
fn binding_before(before: &str) -> String {
    let tail: String = before.chars().rev().take(400).collect();
    let tail: String = tail.chars().rev().collect();
    for marker in ["let ", "pub fn ", "fn "] {
        if let Some(at) = tail.rfind(marker) {
            let rest = &tail[at + marker.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return name;
            }
        }
    }
    String::new()
}

/// The first string literal after each occurrence of `marker`.
fn captures(block: &str, marker: &str) -> Vec<String> {
    let mut found = Vec::new();
    for piece in block.split(marker).skip(1) {
        let Some(open) = piece.find('"') else {
            continue;
        };
        // Only a literal that starts the argument list is a path; anything else is a later
        // argument of some other call that happens to contain a string.
        if piece[..open].contains(')') || piece[..open].contains(';') {
            continue;
        }
        let rest = &piece[open + 1..];
        if let Some(close) = rest.find('"') {
            found.push(rest[..close].to_string());
        }
    }
    found
}

/// The identifiers named by each occurrence of `marker`, taking the last argument, which is the
/// layer function in both `from_fn(f)` and `from_fn_with_state(state, f)`.
fn idents_after(block: &str, marker: &str) -> Vec<String> {
    let mut found = Vec::new();
    for piece in block.split(marker).skip(1) {
        // The argument list ends at the paren that closes the call, not at the first one: the
        // state argument of `from_fn_with_state` carries parens of its own.
        let mut depth = 0usize;
        let mut end = None;
        for (at, c) in piece.char_indices() {
            match c {
                '(' => depth += 1,
                ')' if depth == 0 => {
                    end = Some(at);
                    break;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        let Some(close) = end else { continue };
        let args = &piece[..close];
        // The last non-empty argument: a multi-line call leaves a trailing comma, so the text
        // after the final comma is whitespace and the layer is the argument before it.
        let last = args
            .rsplit(',')
            .map(str::trim)
            .find(|part| !part.is_empty())
            .unwrap_or(args.trim());
        let name = last.rsplit("::").next().unwrap_or(last).trim();
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            found.push(name.to_string());
        }
    }
    found
}

/// The whole surface as the table spells it: every route, against the layers the block declaring
/// it applies.
///
/// Paths are as written, not prefixed by the nesting that mounts them, and layers are the
/// declaring block's own. Resolving either across files means resolving a block by name, and a
/// name the scan reads wrongly drops routes out of the table silently, which is worse than not
/// resolving at all: this test exists to notice a missing guard, so it must not be able to lose a
/// route.
fn table() -> String {
    let mut rows = Vec::new();
    for file in router_sources() {
        let source = std::fs::read_to_string(&file).expect("read a router source");
        for (_, block) in blocks(&source) {
            let layers = if block.layers.is_empty() {
                "-".to_string()
            } else {
                block.layers.join("+")
            };
            for route in block.routes {
                rows.push(format!("{route} => {layers}"));
            }
        }
    }
    rows.sort();
    rows.dedup();
    rows.join("\n")
}

#[test]
fn every_route_carries_the_authorization_it_always_carried() {
    let actual = table();
    if std::env::var_os("ROUTE_GUARDS_OVERWRITE").is_some() {
        std::fs::write(
            crate::test_crate_root().join("src/routes/service/tests/route_guards.txt"),
            format!("{actual}\n"),
        )
        .expect("write the table");
        return;
    }
    let expected = EXPECTED.trim_end();
    if expected == actual {
        return;
    }
    let expected_rows: Vec<&str> = expected.lines().collect();
    let actual_rows: Vec<&str> = actual.lines().collect();
    let gone: Vec<&&str> = expected_rows
        .iter()
        .filter(|row| !actual_rows.contains(row))
        .collect();
    let new: Vec<&&str> = actual_rows
        .iter()
        .filter(|row| !expected_rows.contains(row))
        .collect();
    panic!(
        "the route authorization table changed.\n\nno longer declared:\n  {}\n\nnow declared:\n  {}\n\n\
         A route that only moved file keeps its row. A row whose layers changed is an \
         authorization change: argue it into the table deliberately, then \
         ROUTE_GUARDS_OVERWRITE=1 cargo test --lib route_guards",
        gone.iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n  "),
        new.iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n  "),
    );
}

/// The scan is only worth as much as its reading of a block, so the shapes it must get right are
/// pinned directly: one block's routes take that block's layers, and the next block's do not.
#[test]
fn a_blocks_routes_take_that_blocks_layers_and_not_the_next_ones() {
    let source = r#"
        let read = Router::new()
            .route("/streams/{id}/stats", get(stats))
            .layer(middleware::from_fn(require_read_metadata))
            .with_state(state.clone());
        let write = Router::new()
            .route("/streams/register", post(register))
            .layer(middleware::from_fn(deny_scoped_token))
            .layer(middleware::from_fn(require_admin_or_token_write_metadata))
            .with_state(state.clone());
    "#;
    let found = blocks(source);
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].1.routes, vec!["/streams/{id}/stats".to_string()]);
    assert_eq!(found[0].1.layers, vec!["require_read_metadata".to_string()]);
    assert_eq!(found[1].1.routes, vec!["/streams/register".to_string()]);
    assert_eq!(
        found[1].1.layers,
        vec![
            "deny_scoped_token".to_string(),
            "require_admin_or_token_write_metadata".to_string()
        ]
    );
}

/// A route under no layer is recorded as such rather than skipped: an unguarded route is the one
/// most worth noticing when it appears.
#[test]
fn an_unguarded_route_is_recorded_not_skipped() {
    let source = r#"
        let open = Router::new()
            .route("/version", get(version))
            .with_state(state.clone());
    "#;
    let found = blocks(source);
    assert_eq!(found[0].1.routes, vec!["/version".to_string()]);
    assert!(found[0].1.layers.is_empty(), "{found:?}");
}

/// The layer is read from `from_fn_with_state` too, which is the other spelling in this tree.
#[test]
fn a_stateful_layer_names_its_function_not_its_state() {
    let source = r#"
        let guarded = Router::new()
            .route("/tokens", get(list))
            .layer(middleware::from_fn_with_state(state.clone(), require_admin))
            .with_state(state.clone());
    "#;
    let found = blocks(source);
    assert_eq!(found[0].1.layers, vec!["require_admin".to_string()]);
}
