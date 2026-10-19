//! `crates/mcp/openapi.json` covers every route the crates register. Paths are read from the
//! router tables in the crates' sources (`.route("…"` calls) and compared with `{param}`
//! names normalised away.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn crate_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(name)
}

/// `/a/{x}/b` → `/a/{}/b`.
fn normalise(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut in_param = false;
    for c in path.chars() {
        match c {
            '{' => {
                in_param = true;
                out.push_str("{}");
            }
            '}' => in_param = false,
            _ if in_param => {}
            _ => out.push(c),
        }
    }
    out
}

/// Every `.route("<path>", <handlers>)` in `source` (skipping `#[cfg(test)]` modules) as
/// `(path, methods)`; `any(…)` (the WebSocket route) counts as `get`.
fn routes_in(source: &str) -> Vec<(String, Vec<String>)> {
    let source = source.split("#[cfg(test)]").next().unwrap_or(source);
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(i) = rest.find(".route(") {
        rest = &rest[i + ".route(".len()..];
        let Some(lit) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some(end) = lit.find('"') else {
            continue;
        };
        let path = lit[..end].to_owned();
        // The handler expression: up to the `)` closing `.route(`.
        let after = &lit[end + 1..];
        let mut depth = 1usize;
        let mut close = after.len();
        for (j, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = j;
                        break;
                    }
                }
                _ => {}
            }
        }
        let handlers = &after[..close];
        let mut methods = Vec::new();
        for m in ["get", "post", "put", "patch", "delete", "any"] {
            let needle = format!("{m}(");
            let found = handlers.match_indices(&needle).any(|(k, _)| {
                k == 0 || !handlers[..k].ends_with(|c: char| c.is_alphanumeric() || c == '_')
            });
            if found {
                methods.push(if m == "any" { "get" } else { m }.to_owned());
            }
        }
        out.push((path, methods));
    }
    out
}

/// Routes registered under `crates/<name>/src` (every `.rs` file).
fn crate_routes(name: &str) -> Vec<(String, Vec<String>)> {
    let mut files = Vec::new();
    let mut stack = vec![crate_dir(name).join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
        .iter()
        .flat_map(|f| routes_in(&std::fs::read_to_string(f).unwrap()))
        .collect()
}

/// Documented `(normalised path, method)` pairs.
fn documented() -> BTreeSet<(String, String)> {
    let doc: Value = serde_json::from_str(canvas_cloud_mcp::openapi::OPENAPI_JSON).unwrap();
    assert!(doc["openapi"].as_str().unwrap().starts_with("3.1."));
    let mut out = BTreeSet::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        for method in item.as_object().unwrap().keys() {
            if method != "parameters" {
                out.insert((normalise(path), method.clone()));
            }
        }
    }
    out
}

fn documented_paths() -> BTreeSet<String> {
    documented().into_iter().map(|(p, _)| p).collect()
}

/// Paths a crate mounts at the root rather than under `/v1`.
const ROOT_PATHS: &[&str] = &["/join/{token}", "/mcp", "/llms.txt"];

fn full_path(route: &str) -> String {
    if ROOT_PATHS.contains(&route) {
        route.to_owned()
    } else {
        format!("/v1{route}")
    }
}

/// `(routes found, "METHOD /path" missing from the document)`.
fn missing(crate_name: &str, skip: impl Fn(&str) -> bool) -> (usize, Vec<String>) {
    let doc = documented();
    let routes = crate_routes(crate_name);
    let mut missing = Vec::new();
    for (route, methods) in &routes {
        if skip(route) {
            continue;
        }
        let full = full_path(route);
        assert!(!methods.is_empty(), "{full}: no handler methods parsed");
        for m in methods {
            if !doc.contains(&(normalise(&full), m.clone())) {
                missing.push(format!("{} {full}", m.to_uppercase()));
            }
        }
    }
    (routes.len(), missing)
}

#[test]
fn every_engine_core_and_mcp_route_is_documented() {
    let (n, missing_engine) = missing("engine", |_| false);
    assert!(
        n >= 20,
        "found only {n} engine routes: is the router table still parseable?"
    );
    let (_, missing_mcp) = missing("mcp", |_| false);
    assert!(
        missing_engine.is_empty(),
        "engine routes missing from openapi.json: {missing_engine:?}"
    );
    assert!(
        missing_mcp.is_empty(),
        "mcp routes missing from openapi.json: {missing_mcp:?}"
    );
    let doc = documented();
    for core in ["/v1/info", "/healthz"] {
        assert!(doc.contains(&(core.to_owned(), "get".to_owned())), "{core}");
    }
}

/// Un-ignore once `feat/accounts` and `feat/apps` are merged (their routers live in
/// `crates/accounts` and `crates/apps`).
#[test]
#[ignore = "needs the accounts and apps crates merged"]
fn every_accounts_and_apps_route_is_documented() {
    // `/account…` is the account page at the root, not an API route.
    let (n, missing_accounts) = missing("accounts", |r| r.starts_with("/account"));
    assert!(n >= 10, "found only {n} accounts routes");
    let (n, missing_apps) = missing("apps", |_| false);
    assert!(n >= 10, "found only {n} apps routes");
    assert!(
        missing_accounts.is_empty(),
        "accounts routes missing from openapi.json: {missing_accounts:?}"
    );
    assert!(
        missing_apps.is_empty(),
        "apps routes missing from openapi.json: {missing_apps:?}"
    );
}

#[test]
fn documented_paths_are_not_invented() {
    // Every documented /v1 path is registered by some crate (or is one the contract defines
    // for a crate still being built: accounts, apps).
    let mut registered: BTreeSet<String> = ["engine", "mcp"]
        .iter()
        .flat_map(|c| crate_routes(c))
        .map(|(r, _)| normalise(&full_path(&r)))
        .collect();
    registered.extend(["/v1/info".to_owned(), "/healthz".to_owned()]);
    for p in documented_paths() {
        let pending = ["/v1/auth/", "/v1/tokens", "/v1/events", "/v1/apps/self/"]
            .iter()
            .any(|pre| p.starts_with(pre));
        assert!(
            registered.contains(&p) || pending,
            "{p} is documented but not routed"
        );
    }
}

#[test]
fn normalise_and_parse() {
    assert_eq!(
        normalise("/canvases/{id}/members/{user_id}"),
        "/canvases/{}/members/{}"
    );
    let src = r#"
        Router::new()
            .route("/a", get(x))
            .route(
                "/b/{id}",
                post(y),
            )
        #[cfg(test)]
        .route("/test-only", get(z))
    "#;
    let parsed = routes_in(src);
    assert_eq!(
        parsed,
        [
            ("/a".to_owned(), vec!["get".to_owned()]),
            ("/b/{id}".to_owned(), vec!["post".to_owned()]),
        ]
    );
    let src =
        r#".route("/c", get(h::list).post(h::create).delete(h::target)) .route("/w", any(ws))"#;
    assert_eq!(routes_in(src)[0].1, ["get", "post", "delete"]);
    assert_eq!(routes_in(src)[1].1, ["get"]);
}
