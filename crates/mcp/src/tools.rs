//! The MCP tools: names, descriptions, JSON Schemas and their implementations over the
//! engine's in-process canvas service (`canvas_cloud_engine::api`).
//!
//! Tool names are a contract (CONTRACT §5.4). Every tool answers with
//! `content: [{type: "text", text: <pretty JSON>}]` plus `structuredContent` (the same JSON
//! object, matching the tool's `outputSchema`). Failures are tool results with `isError: true`
//! whose text is the API error body (`{"error": <code>, "message": …}`) and no
//! `structuredContent` — never JSON-RPC errors (only an unknown tool name is one, `-32602`).
//!
//! A read-only personal access token sees only the [`READ_ONLY_TOOLS`] in `tools/list`;
//! calling any other tool with it is an `isError` result with code `read_only_token`.

use std::time::Instant;

use canvas_cloud_core::error::ApiError;
use canvas_cloud_core::identity::{AuthUser, Via};
use canvas_cloud_core::state::SharedState;
use canvas_cloud_engine::api::{self, MarkReadBody, OpsRequest, ReadOptions, PERSONAL};
use serde::Serialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::transport::{RpcError, INVALID_PARAMS};

/// `initialize` `instructions`: how to use the tools, for the model.
pub const INSTRUCTIONS: &str = "\
Canvas Cloud hosts collaborative whiteboards (canvases) that people edit live in a canvas app.
Start with list_canvases (or pass \"personal\" for the caller's own Personal canvas), then read_canvas to see its shapes, arrows, active agents and recent chat.
Draw and edit with apply_ops; pass as={\"name\":\"<your agent name>\"} so people see who is writing. Your edits appear instantly for everyone in the canvas.
Shape ids returned by apply_ops and read_canvas are how later ops refer to shapes. Never clear a canvas unless asked (clear needs confirm=true).
Share with invite (email) or create_share_link; list_mentions shows chat messages that @mention the caller.";

/// Tools a read-only personal access token may list and call.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "whoami",
    "list_canvases",
    "read_canvas",
    "list_members",
    "list_mentions",
];

/// Above this many bytes of (pretty) JSON, `read_canvas` answers with a summary instead.
pub const MAX_READ_BYTES: usize = 200 * 1024;
/// Shapes listed (id, type, position, short label) in an oversized-read summary.
const SUMMARY_SHAPES: usize = 400;
/// Frames listed in an oversized-read summary.
const SUMMARY_FRAMES: usize = 100;
/// Characters of text kept per shape in an oversized-read summary.
const SUMMARY_LABEL_CHARS: usize = 60;
/// Mention ids accepted by `mark_mentions_read` (as `POST /mentions/read`).
const MAX_MARK_IDS: usize = 500;
/// Rounds of "mark every unread mention" (100 per round) before giving up.
const MARK_ALL_ROUNDS: usize = 50;

/// One tool's static description.
pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Callable with a read-only token.
    pub read_only: bool,
    /// May destroy or replace data (annotation hint only).
    pub destructive: bool,
    /// Calling twice with the same arguments has no additional effect (annotation hint).
    pub idempotent: bool,
    input_schema: fn() -> Value,
    output_schema: fn() -> Value,
}

impl Tool {
    /// The tool as `tools/list` shows it.
    pub fn definition(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": (self.input_schema)(),
            "outputSchema": (self.output_schema)(),
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.idempotent,
                "openWorldHint": false,
            },
        })
    }
}

/// Every tool, in `tools/list` order.
pub fn all() -> &'static [Tool] {
    TOOLS
}

/// `tools/list` entries for a caller (`read_only`: a read-scoped token).
pub fn definitions(read_only: bool) -> Vec<Value> {
    TOOLS
        .iter()
        .filter(|t| !read_only || t.read_only)
        .map(Tool::definition)
        .collect()
}

fn find(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

// ---------------------------------------------------------------------------------------------
// Results

/// A failed call: the body shown to the model (`{"error", "message", …}`).
#[derive(Debug)]
struct ToolError(Value);

impl ToolError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self(json!({ "error": code, "message": message.into() }))
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }
}

impl From<ApiError> for ToolError {
    fn from(e: ApiError) -> Self {
        Self(api_error_body(e))
    }
}

/// The REST error body for `e` (same `error` codes and messages as the HTTP API), with a hint
/// for the model on the generic not-found and forbidden cases.
fn api_error_body(e: ApiError) -> Value {
    let code = e.code();
    match e {
        ApiError::BadRequest(message) => json!({ "error": code, "message": message }),
        ApiError::Unauthorized(_) => json!({ "error": code, "message": "unauthorized" }),
        ApiError::UnauthorizedWith { message, .. }
        | ApiError::Denied { message, .. }
        | ApiError::NotFoundCode { message, .. } => json!({ "error": code, "message": message }),
        ApiError::Forbidden => json!({
            "error": code,
            "message": "forbidden: your role on this canvas does not allow this (only the owner may)",
        }),
        ApiError::NotFound => json!({
            "error": code,
            "message": "not found: no such canvas, or you are not a member of it (canvas ids come from list_canvases; \"personal\" is your Personal canvas)",
        }),
        ApiError::Conflict(Value::Object(mut map)) => {
            map.insert("error".into(), Value::from(code));
            map.entry("message")
                .or_insert_with(|| Value::from("conflict"));
            Value::Object(map)
        }
        ApiError::Conflict(other) => {
            json!({ "error": code, "message": "conflict", "detail": other })
        }
        ApiError::PayloadTooLarge => json!({ "error": code, "message": "payload too large" }),
        ApiError::RateLimited => {
            json!({ "error": code, "message": "too many requests; wait a minute and retry" })
        }
        ApiError::Internal(err) => {
            tracing::error!(error = format!("{err:#}"), "internal error in an mcp tool");
            json!({ "error": code, "message": "internal error" })
        }
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn success(v: Value) -> Value {
    let mut result = json!({
        "content": [{ "type": "text", "text": pretty(&v) }],
        "isError": false,
    });
    result["structuredContent"] = v;
    result
}

fn failure(e: &ToolError) -> Value {
    json!({
        "content": [{ "type": "text", "text": pretty(&e.0) }],
        "isError": true,
    })
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, ToolError> {
    serde_json::to_value(v).map_err(|e| ToolError::from(ApiError::internal(e)))
}

// ---------------------------------------------------------------------------------------------
// Arguments (tolerant of the shapes models actually send: numbers or booleans as strings,
// arrays as comma-separated or JSON strings)

struct Args(Map<String, Value>);

impl Args {
    fn parse(v: Value) -> Result<Self, ToolError> {
        match v {
            Value::Null => Ok(Self(Map::new())),
            Value::Object(m) => Ok(Self(m)),
            _ => Err(ToolError::bad("arguments must be a JSON object")),
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    fn string(&self, key: &str) -> Result<Option<String>, ToolError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(ToolError::bad(format!("`{key}` must be a string"))),
        }
    }

    fn required(&self, key: &str) -> Result<String, ToolError> {
        self.string(key)?
            .ok_or_else(|| ToolError::bad(format!("`{key}` is required")))
    }

    /// `canvas`: a canvas id or `personal` (any case).
    fn canvas(&self) -> Result<String, ToolError> {
        let raw = self.string("canvas")?.ok_or_else(|| {
            ToolError::bad(
                "`canvas` is required: a canvas id from list_canvases, or \"personal\" for your Personal canvas",
            )
        })?;
        let raw = raw.trim();
        Ok(if raw.eq_ignore_ascii_case(PERSONAL) {
            PERSONAL.to_owned()
        } else {
            raw.to_owned()
        })
    }

    fn flag(&self, key: &str) -> Result<bool, ToolError> {
        match self.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(true),
                "false" | "0" | "no" | "" => Ok(false),
                _ => Err(ToolError::bad(format!("`{key}` must be true or false"))),
            },
            Some(_) => Err(ToolError::bad(format!("`{key}` must be true or false"))),
        }
    }

    fn int(&self, key: &str) -> Result<Option<i64>, ToolError> {
        let bad = || ToolError::bad(format!("`{key}` must be an integer"));
        match self.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => n.as_i64().map(Some).ok_or_else(bad),
            Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| bad()),
            Some(_) => Err(bad()),
        }
    }

    fn list(&self, key: &str) -> Result<Option<Vec<String>>, ToolError> {
        let bad = || ToolError::bad(format!("`{key}` must be an array of strings"));
        let items = |a: &[Value]| -> Result<Vec<String>, ToolError> {
            a.iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(bad))
                .collect()
        };
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(a)) => items(a).map(Some),
            Some(Value::String(s)) => {
                let s = s.trim();
                if s.starts_with('[') {
                    match serde_json::from_str::<Value>(s) {
                        Ok(Value::Array(a)) => items(&a).map(Some),
                        _ => Err(bad()),
                    }
                } else {
                    Ok(Some(
                        s.split(',')
                            .map(|p| p.trim().to_owned())
                            .filter(|p| !p.is_empty())
                            .collect(),
                    ))
                }
            }
            Some(_) => Err(bad()),
        }
    }

    /// A JSON value that some clients send stringified (`ops`, `near`).
    fn json(&self, key: &str) -> Option<Value> {
        match self.get(key)? {
            Value::String(s) if matches!(s.trim().chars().next(), Some('[' | '{')) => {
                Some(serde_json::from_str(s.trim()).unwrap_or_else(|_| Value::String(s.clone())))
            }
            v => Some(v.clone()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Dispatch

/// `tools/call`. `Err` only for an unknown tool (`-32602`); everything else — including bad
/// arguments, API errors and a read-only token on a write tool — is a tool result with
/// `isError: true`.
pub(crate) async fn call(
    state: &SharedState,
    user: &AuthUser,
    name: &str,
    arguments: Value,
) -> Result<Value, RpcError> {
    let Some(tool) = find(name) else {
        return Err(RpcError::new(
            INVALID_PARAMS,
            format!(
                "unknown tool: {}",
                name.chars().take(64).collect::<String>()
            ),
        ));
    };
    let started = Instant::now();
    let outcome = if user.is_read_only() && !tool.read_only {
        Err(ToolError::new(
            "read_only_token",
            format!(
                "read-only token: `{}` needs a write-scoped personal access token (this one may use {})",
                tool.name,
                READ_ONLY_TOOLS.join(", ")
            ),
        ))
    } else {
        match Args::parse(arguments) {
            Ok(args) => run(tool.name, state, user, &args).await,
            Err(e) => Err(e),
        }
    };
    let (result, outcome_label) = match &outcome {
        Ok(v) => (success(v.clone()), "ok"),
        Err(e) => (failure(e), "error"),
    };
    metrics::counter!("mcp_tool_calls_total", "tool" => tool.name, "outcome" => outcome_label)
        .increment(1);
    tracing::info!(
        tool = tool.name,
        user_id = %user.user_id,
        outcome = outcome_label,
        error = outcome.as_ref().err().and_then(|e| e.0["error"].as_str()).unwrap_or(""),
        ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "mcp tool call"
    );
    Ok(result)
}

async fn run(
    name: &str,
    state: &SharedState,
    user: &AuthUser,
    args: &Args,
) -> Result<Value, ToolError> {
    match name {
        "whoami" => Ok(whoami(state, user)),
        "list_canvases" => {
            let canvases = api::list_canvases(state, user).await?;
            Ok(json!({ "canvases": to_value(&canvases)? }))
        }
        "create_canvas" => {
            let name = args.required("name")?;
            to_value(&api::create_canvas(state, user, &name).await?)
        }
        "rename_canvas" => {
            let canvas = args.canvas()?;
            let name = args.required("name")?;
            to_value(&api::rename_canvas(state, user, &canvas, &name).await?)
        }
        "read_canvas" => read_canvas(state, user, args).await,
        "apply_ops" => apply_ops(state, user, args).await,
        "list_members" => {
            let canvas = args.canvas()?;
            let members = api::list_members(state, user, &canvas).await?;
            Ok(json!({ "members": to_value(&members)? }))
        }
        "invite" => {
            let canvas = args.canvas()?;
            let email = args.required("email")?;
            let sent = api::create_invite(state, user, &canvas, &email).await?;
            let mut v = to_value(&sent)?;
            v["created"] = Value::Bool(sent.created);
            Ok(v)
        }
        "create_share_link" => {
            let canvas = args.canvas()?;
            let link = api::create_share_link(state, user, &canvas, None).await?;
            let mut v = to_value(&link)?;
            v["url"] = Value::String(format!(
                "{}/join/{}",
                state.cfg.public_url.trim_end_matches('/'),
                link.token
            ));
            Ok(v)
        }
        "list_mentions" => {
            let unread_only = args.flag("unread_only")?;
            let limit = args.int("limit")?;
            to_value(&api::list_mentions(state, user, unread_only, limit).await?)
        }
        "mark_mentions_read" => mark_mentions_read(state, user, args).await,
        other => Err(ToolError::new(
            "internal",
            format!("tool {other} is not implemented"),
        )),
    }
}

/// The same `via` as `GET /v1/auth/me`: `{"kind":"session"}`, `{"kind":"token","scope"}`
/// or `{"kind":"app","app_id","subject"}`.
fn via_json(via: &Via) -> Value {
    match via {
        Via::Session { .. } => json!({ "kind": "session" }),
        Via::Token { scope, .. } => json!({ "kind": "token", "scope": scope }),
        Via::App { app_id, subject } => {
            json!({ "kind": "app", "app_id": app_id, "subject": subject })
        }
    }
}

fn whoami(state: &SharedState, user: &AuthUser) -> Value {
    json!({
        "user": {
            "id": user.user_id,
            "email": user.email,
            "display_name": user.display_name,
        },
        "via": via_json(&user.via),
        "read_only": user.is_read_only(),
        "server": {
            "name": "canvas-cloud",
            "version": env!("CARGO_PKG_VERSION"),
            "public_url": state.cfg.public_url,
        },
    })
}

async fn read_canvas(
    state: &SharedState,
    user: &AuthUser,
    args: &Args,
) -> Result<Value, ToolError> {
    let canvas = args.canvas()?;
    let options = ReadOptions {
        full: args.flag("full")?,
        ids: args.list("ids")?,
        types: args.list("types")?,
    };
    let read = api::read_canvas(state, user, &canvas, &options).await?;
    let value = to_value(&read)?;
    let size = pretty(&value).len();
    if size <= MAX_READ_BYTES {
        return Ok(value);
    }
    tracing::debug!(user_id = %user.user_id, bytes = size, "mcp read_canvas over the size limit; summarizing");
    Ok(read_summary(&value, size, options.full))
}

fn shape_label(shape: &Value) -> Option<String> {
    ["title", "text", "label", "url"]
        .iter()
        .find_map(|k| shape.get(*k).and_then(Value::as_str))
        .map(|s| {
            let mut out: String = s.chars().take(SUMMARY_LABEL_CHARS).collect();
            if s.chars().count() > SUMMARY_LABEL_CHARS {
                out.push('…');
            }
            out
        })
}

/// What `read_canvas` returns when the full read is over [`MAX_READ_BYTES`]: counts per
/// type, the frames, and the first shapes (id, type, position, a short label) — enough to
/// pick `ids` / `types` for a narrower read.
fn read_summary(read: &Value, size: usize, full: bool) -> Value {
    let empty = Vec::new();
    let shapes = read["shapes"].as_array().unwrap_or(&empty);
    let mut by_type = Map::new();
    for s in shapes {
        let t = s["type"].as_str().unwrap_or("unknown").to_owned();
        let n = by_type.get(&t).and_then(Value::as_u64).unwrap_or(0);
        by_type.insert(t, Value::from(n + 1));
    }
    let brief = |s: &Value| {
        let mut b = Map::new();
        for k in ["id", "type", "x", "y", "w", "h", "frame"] {
            if let Some(v) = s.get(k) {
                b.insert(k.to_owned(), v.clone());
            }
        }
        if let Some(label) = shape_label(s) {
            b.insert("label".into(), Value::String(label));
        }
        Value::Object(b)
    };
    let frames: Vec<Value> = shapes
        .iter()
        .filter(|s| s["type"] == "frame")
        .take(SUMMARY_FRAMES)
        .map(brief)
        .collect();
    let listed: Vec<Value> = shapes.iter().take(SUMMARY_SHAPES).map(brief).collect();
    let hint = format!(
        "The full read is {size} bytes, over the {MAX_READ_BYTES}-byte limit for one tool result, so this is a summary. \
         Call read_canvas again with `ids` (shape ids from `shapes` below) or `types` (e.g. [\"frame\", \"sticky\"]) to read part of the canvas{}.",
        if full {
            ", or without `full` (text is then cut to 500 characters)"
        } else {
            ""
        }
    );
    json!({
        "canvas": read["canvas"],
        "truncated": true,
        "bytes": size,
        "limit_bytes": MAX_READ_BYTES,
        "count": read["count"],
        "matched": shapes.len(),
        "by_type": by_type,
        "frames": frames,
        "shapes": listed,
        "shapes_listed": listed.len(),
        "agents": read["agents"],
        "chat_messages": read["chat"].as_array().map_or(0, Vec::len),
        "hint": hint,
    })
}

async fn apply_ops(state: &SharedState, user: &AuthUser, args: &Args) -> Result<Value, ToolError> {
    let canvas = args.canvas()?;
    let Some(ops) = args.json("ops") else {
        return Err(ToolError::bad(
            "`ops` is required: an array of ops such as [{\"op\":\"add\",\"shape\":{\"type\":\"sticky\",\"text\":\"Hello\"}}]",
        ));
    };
    let mut envelope = Map::new();
    envelope.insert("ops".into(), ops);
    if let Some(actor) = args.json("as") {
        envelope.insert("as".into(), actor);
    }
    if let Some(near) = args.json("near") {
        envelope.insert("near".into(), near);
    }
    if args.flag("confirm")? {
        envelope.insert("confirm".into(), Value::Bool(true));
    }
    let request = match OpsRequest::from_json(Value::Object(envelope)) {
        Ok(request) => request,
        Err(e) => {
            // Membership first, like REST: a stranger learns nothing from a malformed call.
            api::get_canvas(state, user, &canvas).await?;
            return Err(ToolError::bad(e));
        }
    };
    let result = api::apply_ops(state, user, &canvas, request).await?;
    let value = to_value(&result)?;
    if result.applied == 0 && !result.errors.is_empty() {
        // Nothing applied: report it as a failed call (the per-op errors say why).
        return Err(ToolError(value));
    }
    Ok(value)
}

async fn mark_mentions_read(
    state: &SharedState,
    user: &AuthUser,
    args: &Args,
) -> Result<Value, ToolError> {
    let ids = args
        .list("ids")?
        .map(|ids| {
            if ids.len() > MAX_MARK_IDS {
                return Err(ToolError::bad(format!(
                    "at most {MAX_MARK_IDS} ids per call"
                )));
            }
            ids.iter()
                .enumerate()
                .map(|(i, raw)| {
                    Uuid::parse_str(raw.trim()).map_err(|_| {
                        ToolError::bad(format!(
                            "ids[{i}] is not a mention id (a UUID from list_mentions)"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    let canvas_id = match args.string("canvas")? {
        Some(_) => Some(api::get_canvas(state, user, &args.canvas()?).await?.id),
        None => None,
    };
    if ids.is_some() || canvas_id.is_some() {
        let read = api::mark_read(state, user, MarkReadBody { ids, canvas_id }).await?;
        return to_value(&read);
    }
    // Neither: every unread mention the caller can see, a page at a time.
    let mut updated = 0;
    for _ in 0..MARK_ALL_ROUNDS {
        let page = api::list_mentions(state, user, true, Some(100)).await?;
        if page.mentions.is_empty() {
            break;
        }
        let ids = page.mentions.iter().map(|m| m.id).collect();
        let read = api::mark_read(
            state,
            user,
            MarkReadBody {
                ids: Some(ids),
                canvas_id: None,
            },
        )
        .await?;
        updated += read.updated;
        if read.updated == 0 {
            break;
        }
    }
    Ok(json!({ "ok": true, "updated": updated }))
}

// ---------------------------------------------------------------------------------------------
// Schemas

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn canvas_arg() -> Value {
    string("Canvas id (a UUID from list_canvases), or \"personal\" for your own Personal canvas.")
}

fn input(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({ "type": "object" });
    schema["properties"] = properties;
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

fn out(properties: Value, required: &[&str]) -> Value {
    input(properties, required)
}

fn ty(t: &str) -> Value {
    json!({ "type": t })
}

fn nullable(t: &str) -> Value {
    json!({ "type": [t, "null"] })
}

fn array_of(items: Value) -> Value {
    let mut schema = json!({ "type": "array" });
    schema["items"] = items;
    schema
}

fn user_ref_schema() -> Value {
    out(
        json!({ "id": ty("string"), "email": ty("string"), "display_name": ty("string") }),
        &["id", "email", "display_name"],
    )
}

fn canvas_schema() -> Value {
    out(
        json!({
            "id": ty("string"),
            "name": ty("string"),
            "kind": { "type": "string", "enum": ["personal", "shared"] },
            "role": { "type": "string", "enum": ["owner", "editor"] },
            "owner": user_ref_schema(),
            "member_count": ty("integer"),
            "created_at": ty("string"),
            "updated_at": ty("string"),
        }),
        &["id", "name", "kind", "role", "owner", "member_count"],
    )
}

fn no_args() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn whoami_out() -> Value {
    out(
        json!({
            "user": out(
                json!({ "id": ty("string"), "email": ty("string"), "display_name": nullable("string") }),
                &["id", "email"],
            ),
            "via": out(json!({ "kind": { "type": "string", "enum": ["session", "token", "app"] } }), &["kind"]),
            "read_only": ty("boolean"),
            "server": out(
                json!({ "name": ty("string"), "version": ty("string"), "public_url": ty("string") }),
                &["name", "version"],
            ),
        }),
        &["user", "via", "read_only"],
    )
}

fn list_canvases_out() -> Value {
    out(
        json!({ "canvases": array_of(canvas_schema()) }),
        &["canvases"],
    )
}

fn create_canvas_in() -> Value {
    input(
        json!({ "name": string("Name of the new shared canvas (1-200 characters).") }),
        &["name"],
    )
}

fn rename_canvas_in() -> Value {
    input(
        json!({
            "canvas": canvas_arg(),
            "name": string("New name (1-200 characters)."),
        }),
        &["canvas", "name"],
    )
}

fn read_canvas_in() -> Value {
    input(
        json!({
            "canvas": canvas_arg(),
            "ids": {
                "type": "array",
                "items": ty("string"),
                "description": "Only these shape ids (as returned by read_canvas or apply_ops).",
            },
            "types": {
                "type": "array",
                "items": ty("string"),
                "description": "Only these shape types: sticky, text, frame, arrow, image, link, checklist.",
            },
            "full": {
                "type": "boolean",
                "description": "Return whole texts (default false: text, titles and labels are cut to 500 characters).",
            },
        }),
        &["canvas"],
    )
}

fn read_canvas_out() -> Value {
    out(
        json!({
            "canvas": out(
                json!({ "id": ty("string"), "name": ty("string"), "kind": ty("string") }),
                &["id", "name", "kind"],
            ),
            "shapes": array_of(out(
                json!({ "id": ty("string"), "type": ty("string") }),
                &["id", "type"],
            )),
            "agents": array_of(ty("object")),
            "selection": array_of(ty("string")),
            "count": ty("integer"),
            "chat": array_of(ty("object")),
            "truncated": ty("boolean"),
            "hint": ty("string"),
        }),
        &["canvas", "shapes", "count"],
    )
}

fn apply_ops_in() -> Value {
    input(
        json!({
            "canvas": canvas_arg(),
            "ops": {
                "type": "array",
                "minItems": 1,
                "maxItems": 500,
                "items": { "type": "object" },
                "description": "Ops applied in order in one transaction; see the tool description for every op.",
            },
            "as": {
                "type": "object",
                "description": "Who is drawing: created shapes get `by` = name and your agent cursor shows on the canvas while you write. Default: the signed-in user.",
                "properties": {
                    "name": string("Agent display name, e.g. \"Planner\"."),
                    "id": string("Stable agent id (default \"agent:<lowercased name>\")."),
                    "color": string("Cursor colour: a palette name or #rrggbb."),
                },
            },
            "near": {
                "type": "object",
                "description": "Where auto-placement looks for free space for shapes without x/y (default 0,0).",
                "properties": { "x": ty("number"), "y": ty("number") },
                "required": ["x", "y"],
            },
            "confirm": {
                "type": "boolean",
                "description": "Must be true for a clear op. Only clear when the user explicitly asked.",
            },
        }),
        &["canvas", "ops"],
    )
}

fn apply_ops_out() -> Value {
    out(
        json!({
            "applied": ty("integer"),
            "ids": array_of(nullable("string")),
            "errors": array_of(out(
                json!({ "index": ty("integer"), "op": ty("string"), "error": ty("string") }),
                &["index", "op", "error"],
            )),
        }),
        &["applied", "ids", "errors"],
    )
}

fn canvas_only_in() -> Value {
    input(json!({ "canvas": canvas_arg() }), &["canvas"])
}

fn list_members_out() -> Value {
    out(
        json!({
            "members": array_of(out(
                json!({
                    "user_id": ty("string"),
                    "email": ty("string"),
                    "display_name": ty("string"),
                    "role": { "type": "string", "enum": ["owner", "editor"] },
                    "added_at": ty("string"),
                }),
                &["user_id", "email", "display_name", "role"],
            )),
        }),
        &["members"],
    )
}

fn invite_in() -> Value {
    input(
        json!({
            "canvas": canvas_arg(),
            "email": string("Email address to invite. They join only after accepting."),
        }),
        &["canvas", "email"],
    )
}

fn invite_out() -> Value {
    let user_or_null = json!({ "anyOf": [user_ref_schema(), ty("null")] });
    out(
        json!({
            "id": ty("string"),
            "canvas_id": ty("string"),
            "canvas_name": ty("string"),
            "email": ty("string"),
            "status": ty("string"),
            "invited_by": user_or_null,
            "invitee": user_or_null,
            "created_at": ty("string"),
            "nudged_at": nullable("string"),
            "nudged": ty("boolean"),
            "created": ty("boolean"),
        }),
        &["id", "canvas_id", "email", "status", "nudged", "created"],
    )
}

fn share_link_out() -> Value {
    out(
        json!({
            "id": ty("string"),
            "token": ty("string"),
            "url": ty("string"),
            "canvas_id": ty("string"),
            "role": ty("string"),
            "created_at": ty("string"),
        }),
        &["id", "token", "url", "canvas_id", "role"],
    )
}

fn list_mentions_in() -> Value {
    input(
        json!({
            "unread_only": {
                "type": "boolean",
                "description": "Only mentions not yet marked read (default false).",
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "description": "At most this many, newest first (default 50, max 100).",
            },
        }),
        &[],
    )
}

fn list_mentions_out() -> Value {
    out(
        json!({
            "mentions": array_of(out(
                json!({
                    "id": ty("string"),
                    "canvas": out(json!({ "id": ty("string"), "name": ty("string") }), &["id", "name"]),
                    "from": user_ref_schema(),
                    "message_id": ty("string"),
                    "excerpt": ty("string"),
                    "created_at": ty("string"),
                    "read_at": nullable("string"),
                }),
                &["id", "canvas", "from", "message_id", "excerpt"],
            )),
            "unread": ty("integer"),
        }),
        &["mentions", "unread"],
    )
}

fn mark_read_in() -> Value {
    input(
        json!({
            "ids": {
                "type": "array",
                "items": ty("string"),
                "maxItems": 500,
                "description": "Mention ids from list_mentions to mark read.",
            },
            "canvas": string("Mark every mention on this canvas read (a canvas id, or \"personal\")."),
        }),
        &[],
    )
}

fn mark_read_out() -> Value {
    out(
        json!({ "ok": ty("boolean"), "updated": ty("integer") }),
        &["ok", "updated"],
    )
}

const APPLY_OPS_DESCRIPTION: &str = r#"Edit a canvas: apply ops in order, in one transaction. Everyone in the canvas sees the change live. A bad op is skipped and reported in `errors` (by index); the rest still apply. At most 500 ops per call.

Shape types (shape.type) and their props:
- sticky: text (markdown, ≤ 20000 chars), fontSize?; 200x200, default yellow
- text: text, fontSize? (6-240), align? (left|center|right); 240x36
- frame: title, image? ({src, naturalW, naturalH}); 480x320. Moving a frame carries every shape wholly inside it.
- arrow: from, to (a shape id, {ref: id, side?: top|right|bottom|left}, or {x,y}), label? — usually made with connect
- image: src (https: URL or data:image/… ≤ 2 MiB), naturalW, naturalH
- link: url (http, https, mailto; bare hosts get https://), title? (default: the host)
- checklist: title, columns (1-4 names, default ["Yes","No"]), rows (labels like "Ann" or {label, id?}, ≤ 60), picks ({"<row label or id>": "<column>" | true (first column) | null (clear)})
Every shape: x, y (top-left in canvas pixels; omit both to auto-place in free space near `near`), w, h, color (yellow, pink, blue, green, purple, gray, white, or #rgb/#rrggbb), z (stacking).

Ops:
- {"op":"add","shape":{"type":"sticky","text":"Ship v1","id":"goal"}} — `id` is optional (1-64 of A-Z a-z 0-9 _ - : .); without it one is generated. ids[i] in the result is the shape id.
- {"op":"update","id":"goal","patch":{"text":"Ship v1.1","color":"green"}} — merges props (frame: "image": null removes its image).
- {"op":"move","id":"goal","dx":40,"dy":0}
- {"op":"resize","id":"goal","w":300,"h":200} — not for arrows
- {"op":"delete","id":"goal"} — arrows attached to it are deleted too
- {"op":"connect","from":"goal","to":"risk","label":"blocked by"} — adds an arrow between two shapes (optional color, fromSide, toSide, id)
- {"op":"clear"} — deletes every shape; refused unless confirm is true. Never clear unless the user explicitly asked.
Ids may be written "shape:<id>". Later ops in the same call can use ids given to shapes added earlier in it. Aliases: text (or label) on a frame, link or checklist is its title; text/title on an arrow is its label; title on a sticky/text is its text. Unknown props for a type are an error.

Envelope: `as` ({"name":"Planner","color":"purple"}) attributes the edit to your agent: created shapes get by = name and your cursor shows as writing on the canvas (default: the signed-in user). `near` ({x,y}) is where auto-placement looks for free space (default 0,0). `confirm`: true unlocks clear.
Result: {applied, ids: [shape id or null for each op], errors: [{index, op, error}]}. If nothing applied, the call is reported as an error."#;

static TOOLS: &[Tool] = &[
    Tool {
        name: "whoami",
        title: "Who am I",
        description: "Who the server thinks you are: the signed-in user (id, email, display name), how you authenticated (session, personal access token with its scope, or a linked app acting for a user), whether your token is read-only, and this server's version and public URL.",
        read_only: true,
        destructive: false,
        idempotent: true,
        input_schema: no_args,
        output_schema: whoami_out,
    },
    Tool {
        name: "list_canvases",
        title: "List canvases",
        description: "List the canvases you can open: your Personal canvas first, then shared canvases you own or were added to, most recently updated first. Each has id, name, kind (personal|shared), your role (owner|editor), owner, member_count, created_at and updated_at. Use the id (or \"personal\") as `canvas` in the other tools.",
        read_only: true,
        destructive: false,
        idempotent: true,
        input_schema: no_args,
        output_schema: list_canvases_out,
    },
    Tool {
        name: "create_canvas",
        title: "Create a canvas",
        description: "Create a new shared canvas owned by you and return it (id, name, kind \"shared\", role \"owner\", …). It starts empty; add shapes with apply_ops and bring people in with invite or create_share_link.",
        read_only: false,
        destructive: false,
        idempotent: false,
        input_schema: create_canvas_in,
        output_schema: canvas_schema,
    },
    Tool {
        name: "rename_canvas",
        title: "Rename a canvas",
        description: "Rename a shared canvas (owner only; the Personal canvas cannot be renamed). Connected people see the new name immediately. Returns the updated canvas.",
        read_only: false,
        destructive: false,
        idempotent: true,
        input_schema: rename_canvas_in,
        output_schema: canvas_schema,
    },
    Tool {
        name: "read_canvas",
        title: "Read a canvas",
        description: "Read a canvas: `canvas` {id, name, kind}; `shapes` sorted top-to-bottom then left-to-right, each with id, type, x, y, w, h, color, z, by (who made it) and per type: text (sticky/text), title (frame/link/checklist), label/from/to (arrow), url (link), src/naturalW/naturalH (image), columns/rows/tally (checklist), and `frame` (the frame it sits in); `agents` currently drawing; `count` (all shapes); `chat` (the last 50 messages, oldest first). Text is cut to 500 characters unless full=true. Narrow big canvases with `ids` or `types`; a read over 200 KB returns a summary (counts, frames, shape ids) with `truncated: true` instead.",
        read_only: true,
        destructive: false,
        idempotent: true,
        input_schema: read_canvas_in,
        output_schema: read_canvas_out,
    },
    Tool {
        name: "apply_ops",
        title: "Apply canvas ops",
        description: APPLY_OPS_DESCRIPTION,
        read_only: false,
        destructive: true,
        idempotent: false,
        input_schema: apply_ops_in,
        output_schema: apply_ops_out,
    },
    Tool {
        name: "list_members",
        title: "List canvas members",
        description: "List a canvas's members (owner first): user_id, email, display_name, role (owner|editor) and when they were added. Any member may list.",
        read_only: true,
        destructive: false,
        idempotent: true,
        input_schema: canvas_only_in,
        output_schema: list_members_out,
    },
    Tool {
        name: "invite",
        title: "Invite someone by email",
        description: "Invite an email address to a shared canvas as an editor (any member may invite; not the Personal canvas). They are added only once they accept. Inviting an address that already has a pending invite re-sends it (created=false, nudged=true when they were reminded). Returns the invite: id, status, invitee (null while nobody has an account with that email), created, nudged.",
        read_only: false,
        destructive: false,
        idempotent: false,
        input_schema: invite_in,
        output_schema: invite_out,
    },
    Tool {
        name: "create_share_link",
        title: "Create a share link",
        description: "Create a share link for a shared canvas (any member; not the Personal canvas). Anyone signed in who opens it joins as an editor. Returns the one-time-visible token and `url` (this server's /join/<token> page) — give the url to the people who should join; it is not shown again.",
        read_only: false,
        destructive: false,
        idempotent: false,
        input_schema: canvas_only_in,
        output_schema: share_link_out,
    },
    Tool {
        name: "list_mentions",
        title: "List chat mentions",
        description: "Chat messages that @mention you, on canvases you are still a member of, newest first: id, canvas {id, name}, from {id, email, display_name}, message_id, excerpt (≤ 200 chars), created_at, read_at (null = unread), plus `unread` (your total unread count). Read the full conversation with read_canvas (its `chat`).",
        read_only: true,
        destructive: false,
        idempotent: true,
        input_schema: list_mentions_in,
        output_schema: list_mentions_out,
    },
    Tool {
        name: "mark_mentions_read",
        title: "Mark mentions read",
        description: "Mark your mentions read: the given mention `ids`, every mention on `canvas`, or — with neither — all your unread mentions. Your other devices clear their badges. Returns {ok, updated} (how many changed from unread to read).",
        read_only: false,
        destructive: false,
        idempotent: true,
        input_schema: mark_read_in,
        output_schema: mark_read_out,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_table() {
        let names: Vec<&str> = all().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            [
                "whoami",
                "list_canvases",
                "create_canvas",
                "rename_canvas",
                "read_canvas",
                "apply_ops",
                "list_members",
                "invite",
                "create_share_link",
                "list_mentions",
                "mark_mentions_read",
            ]
        );
        let read: Vec<&str> = all()
            .iter()
            .filter(|t| t.read_only)
            .map(|t| t.name)
            .collect();
        assert_eq!(read, READ_ONLY_TOOLS);
        for t in all() {
            let d = t.definition();
            assert_eq!(d["inputSchema"]["type"], "object", "{}", t.name);
            assert_eq!(d["outputSchema"]["type"], "object", "{}", t.name);
            assert!(t.description.len() > 60, "{}", t.name);
            // Every required input is a declared property.
            if let Some(req) = d["inputSchema"]["required"].as_array() {
                for r in req {
                    assert!(
                        d["inputSchema"]["properties"]
                            .get(r.as_str().unwrap())
                            .is_some(),
                        "{} {r}",
                        t.name
                    );
                }
            }
        }
        assert_eq!(definitions(true).len(), READ_ONLY_TOOLS.len());
        assert_eq!(definitions(false).len(), all().len());
        let lines = INSTRUCTIONS.lines().count();
        assert!((3..=6).contains(&lines), "{lines}");
    }

    #[test]
    fn tolerant_arguments() {
        let a = Args::parse(json!({
            "canvas": " Personal ",
            "full": "true",
            "ids": "a, b,,c",
            "types": "[\"sticky\"]",
            "limit": "5",
            "ops": "[{\"op\":\"clear\"}]",
        }))
        .unwrap();
        assert_eq!(a.canvas().unwrap(), "personal");
        assert!(a.flag("full").unwrap());
        assert!(!a.flag("missing").unwrap());
        assert_eq!(a.list("ids").unwrap().unwrap(), ["a", "b", "c"]);
        assert_eq!(a.list("types").unwrap().unwrap(), ["sticky"]);
        assert_eq!(a.int("limit").unwrap(), Some(5));
        assert_eq!(a.json("ops").unwrap(), json!([{ "op": "clear" }]));
        assert!(Args::parse(json!([1])).is_err());
        let bad = Args::parse(json!({ "full": 3, "ids": [1], "canvas": 7 })).unwrap();
        assert!(bad.flag("full").is_err());
        assert!(bad.list("ids").is_err());
        assert_eq!(bad.canvas().unwrap_err().0["error"], "bad_request");
    }

    #[test]
    fn api_errors_keep_their_codes() {
        let b = api_error_body(ApiError::NotFound);
        assert_eq!(b["error"], "not_found");
        assert!(b["message"].as_str().unwrap().contains("list_canvases"));
        let b = api_error_body(ApiError::Conflict(json!({ "message": "dup", "x": 1 })));
        assert_eq!(b, json!({ "error": "conflict", "message": "dup", "x": 1 }));
        let b = api_error_body(ApiError::Denied {
            code: "read_only_token",
            message: "ro",
        });
        assert_eq!(b, json!({ "error": "read_only_token", "message": "ro" }));
    }

    #[test]
    fn summary_is_small_and_points_at_ids() {
        let shapes: Vec<Value> = (0..2000)
            .map(|i| {
                json!({
                    "id": format!("s{i}"),
                    "type": if i % 10 == 0 { "frame" } else { "sticky" },
                    "x": i, "y": 0, "w": 200, "h": 200,
                    "text": "x".repeat(400),
                })
            })
            .collect();
        let read = json!({
            "canvas": { "id": "c", "name": "Big", "kind": "shared" },
            "shapes": shapes, "agents": [], "selection": [], "count": 2000, "chat": [],
        });
        let size = pretty(&read).len();
        let s = read_summary(&read, size, true);
        assert!(pretty(&s).len() < MAX_READ_BYTES / 2);
        assert_eq!(s["truncated"], true);
        assert_eq!(s["by_type"]["frame"], 200);
        assert_eq!(s["shapes_listed"], SUMMARY_SHAPES);
        assert_eq!(s["frames"].as_array().unwrap().len(), SUMMARY_FRAMES);
        assert!(s["hint"].as_str().unwrap().contains("ids"));
    }
}
