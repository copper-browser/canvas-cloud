//! `GET /v1/openapi.json` — the `OpenAPI` 3.1 description of every `/v1` route (engine, core,
//! accounts, apps, this crate) plus the root routes (`/mcp`, `/llms.txt`, `/join/{token}`,
//! `/healthz`). The document is hand-maintained in `crates/mcp/openapi.json` and embedded at
//! build time; at serve time `servers` becomes `[{"url": public_url}]` and `info.version` the
//! running version.

use std::sync::OnceLock;

use axum::extract::State;
use axum::http::header::CACHE_CONTROL;
use axum::response::{IntoResponse, Response};
use axum::Json;
use canvas_cloud_core::config::Config;
use canvas_cloud_core::state::SharedState;
use serde_json::{json, Value};

/// The embedded document, as written (`servers` is a placeholder).
pub const OPENAPI_JSON: &str = include_str!("../openapi.json");

fn parsed() -> &'static Value {
    static DOC: OnceLock<Value> = OnceLock::new();
    DOC.get_or_init(|| {
        serde_json::from_str(OPENAPI_JSON).expect("crates/mcp/openapi.json is valid JSON (tested)")
    })
}

/// The document for this server: `servers` = `public_url`, `info.version` = this build.
pub fn document(cfg: &Config) -> Value {
    let mut doc = parsed().clone();
    doc["servers"] =
        json!([{ "url": cfg.public_url.trim_end_matches('/'), "description": "this server" }]);
    doc["info"]["version"] = Value::from(env!("CARGO_PKG_VERSION"));
    doc
}

/// `GET /v1/openapi.json` — public.
pub(crate) async fn serve(State(state): State<SharedState>) -> Response {
    (
        [(CACHE_CONTROL, "public, max-age=300")],
        Json(document(&state.cfg)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    if k == "$ref" {
                        if let Some(s) = x.as_str() {
                            out.push(s);
                        }
                    } else {
                        refs(x, out);
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
            _ => {}
        }
    }

    #[test]
    fn document_is_openapi_31_and_every_ref_resolves() {
        let doc = parsed();
        assert!(doc["openapi"].as_str().unwrap().starts_with("3.1."));
        let mut all = Vec::new();
        refs(doc, &mut all);
        assert!(all.len() > 50, "{}", all.len());
        for r in all {
            let path = r
                .strip_prefix("#/")
                .unwrap_or_else(|| panic!("external ref {r}"));
            let mut node = doc;
            for part in path.split('/') {
                node = node
                    .get(part)
                    .unwrap_or_else(|| panic!("dangling $ref {r}"));
            }
        }
        // Every operation has an operationId (unique), a summary and responses.
        let mut ids = std::collections::HashSet::new();
        for (path, item) in doc["paths"].as_object().unwrap() {
            for (method, op) in item.as_object().unwrap() {
                if method == "parameters" {
                    continue;
                }
                let id = op["operationId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{method} {path}: operationId"));
                assert!(ids.insert(id.to_owned()), "duplicate operationId {id}");
                assert!(op["summary"].is_string(), "{method} {path}: summary");
                assert!(op["responses"].is_object(), "{method} {path}: responses");
            }
        }
        // No product other than Canvas Cloud is named (spelled split so a repo-wide grep stays clean).
        assert!(!OPENAPI_JSON.to_lowercase().contains(concat!("cop", "per")));
    }

    #[test]
    fn servers_and_version_are_patched() {
        let mut cfg = Config::for_tests("postgres://localhost/unused");
        cfg.public_url = "https://canvas.example.com".into();
        let doc = document(&cfg);
        assert_eq!(doc["servers"][0]["url"], "https://canvas.example.com");
        assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    }
}
