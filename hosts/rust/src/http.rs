//! The HTTP API: one `POST /commands/{Command}` per command of the
//! model, described by the contract the component builds
//! (`/openapi.json`) and checked against it before the component sees a
//! request – what is documented is what is enforced.
//!
//!   200 published   422 rejected   400 arguments do not fit
//!   404 no such command   409 conflict   500 evaluation   503 store
//!
//! `POST /read` (`--with-read`) reads the events matching a DCB query, a
//! page at a time: `after` a position, at most `limit` of them, oldest
//! or – `backwards` – newest first. A full page names the position to go
//! on `after` as `next`. 501 when the store cannot read that way.
//!
//! `GET /projections/{Projection}` (`--with-projections`) folds a
//! projection, read by its tags and arguments as query parameters – a
//! scalar as it is, a composite as JSON – each checked against the
//! contract: `{value, position}`, never cached.
//!
//! `GET /health` answers whether the store is serving, which store it is,
//! its last position and how long it took to tell; 503 when it fails.
//!
//! Failures are RFC 9457 problem details. `/docs` (`--with-docs`) is
//! Swagger UI over the contract, embedded, so it works without network
//! access.
//!
//! Every request is logged at `info` – its status and duration, in a
//! span naming method and URI; at `debug` with the JSON it carried and
//! was answered with.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::Instrument;

use crate::component::{Event, Outcome, Pool, ProjectionOutcome};
use crate::component::dcb::runtime::types::QueryItem;
use crate::store::{EventStore, ReadOptions, ms};

/// A page of `/read`, when the request names no `limit`.
const DEFAULT_LIMIT: u64 = 100;
const MAX_LIMIT: u64 = 1000;

pub struct Api {
    pool: Pool,
    contract: String,
    validators: HashMap<String, jsonschema::Validator>,
    /// The validator of `/read`'s query; none when reads are not served.
    read: Option<jsonschema::Validator>,
    /// Each projection's parameters; none when projections are not served.
    projections: Option<HashMap<String, Vec<Parameter>>>,
}

/// One query parameter of a projection – a tag or an argument.
struct Parameter {
    name: String,
    /// A composite, given as JSON; a scalar is given as it is.
    json: bool,
    validator: jsonschema::Validator,
}

impl Parameter {
    /// The value a parameter's text stands for. A scalar's text is the
    /// string itself when the parameter takes that, its JSON otherwise:
    /// `c1`, `12`, `true`.
    fn value(&self, text: &str) -> Result<Value, Vec<String>> {
        let errors = |value: &Value| -> Vec<String> {
            self.validator.iter_errors(value).map(|e| format!("{}: {e}", self.name)).collect()
        };
        let parsed = serde_json::from_str::<Value>(text);
        if self.json {
            let value = parsed.map_err(|e| vec![format!("{}: no JSON: {e}", self.name)])?;
            let invalid = errors(&value);
            return if invalid.is_empty() { Ok(value) } else { Err(invalid) };
        }
        let string = Value::String(text.to_string());
        if self.validator.is_valid(&string) {
            return Ok(string);
        }
        match parsed {
            Ok(value) if !value.is_string() => {
                let invalid = errors(&value);
                if invalid.is_empty() { Ok(value) } else { Err(invalid) }
            }
            _ => Err(errors(&string)),
        }
    }
}

impl Api {
    /// Reads the contract off the component and compiles one validator
    /// per command from it. A request schema names custom types by
    /// `$ref` into the contract's components, so each is compiled with
    /// those components beside it.
    ///
    /// With `read`, the contract describes `/read` as well; with
    /// `projections`, each projection, one validator per parameter.
    pub async fn new(pool: Pool, read: bool, projections: bool) -> wasmtime::Result<Self> {
        let mut contract = pool.contract(projections).await?;
        let mut document: Value = serde_json::from_str(&contract)?;
        document["paths"]["/health"] = health_operation();
        contract = document.to_string();
        let read = if read {
            document["paths"]["/read"] = read_operation();
            contract = document.to_string();
            Some(jsonschema::draft202012::new(&read_query_schema()).map_err(|e| wasmtime::format_err!("the read query's schema: {e}"))?)
        } else {
            None
        };
        let compile = |mut schema: Value, of: &str| {
            schema["components"] = document["components"].clone();
            jsonschema::draft202012::new(&schema).map_err(|e| wasmtime::format_err!("the contract's schema for {of}: {e}"))
        };
        let mut validators = HashMap::new();
        let mut parameters = HashMap::new();
        for (path, item) in document["paths"].as_object().into_iter().flatten() {
            if let Some(command) = path.strip_prefix("/commands/") {
                let schema = item["post"]["requestBody"]["content"]["application/json"]["schema"].clone();
                validators.insert(command.to_string(), compile(schema, command)?);
            } else if let Some(projection) = path.strip_prefix("/projections/") {
                let mut of = Vec::new();
                for parameter in item["get"]["parameters"].as_array().into_iter().flatten() {
                    let name = parameter["name"].as_str().unwrap_or_default().to_string();
                    let json = parameter.get("content").is_some();
                    let schema = if json { &parameter["content"]["application/json"]["schema"] } else { &parameter["schema"] };
                    let validator = compile(schema.clone(), &format!("{projection}'s {name}"))?;
                    of.push(Parameter { name, json, validator });
                }
                parameters.insert(projection.to_string(), of);
            }
        }
        let projections = projections.then_some(parameters);
        Ok(Self { pool, contract, validators, read, projections })
    }
}

fn problem(status: StatusCode, title: &str, detail: impl Into<String>, errors: Option<Vec<String>>) -> Response {
    let mut body = json!({ "title": title, "status": status.as_u16(), "detail": detail.into() });
    if let Some(errors) = errors {
        body["errors"] = json!(errors);
    }
    (status, [(header::CONTENT_TYPE, "application/problem+json")], body.to_string()).into_response()
}

fn event_json(e: Event) -> Value {
    json!({
        "type": e.event_type,
        "data": serde_json::from_str::<Value>(&e.data).unwrap_or(Value::String(e.data)),
        "tags": e.tags,
    })
}

fn events_json(events: Vec<Event>) -> Vec<Value> {
    events.into_iter().map(event_json).collect()
}

fn validation_errors(validator: &jsonschema::Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|e| format!("{}: {}", if e.instance_path().as_str().is_empty() { "/" } else { e.instance_path().as_str() }, e))
        .collect()
}

pub fn outcome_response(outcome: Outcome) -> Response {
    match outcome {
        Outcome::Published(p) => {
            (StatusCode::OK, Json(json!({ "events": events_json(p.events), "position": p.position }))).into_response()
        }
        Outcome::Rejected(r) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "rejection": r.rejection })),
        )
            .into_response(),
        Outcome::Conflict => problem(
            StatusCode::CONFLICT,
            "Conflict",
            "Every attempt lost the race to a concurrent append; the request may be retried.",
            None,
        ),
        Outcome::Invalid(message) => problem(StatusCode::BAD_REQUEST, "Invalid request", message, None),
        Outcome::Evaluation(message) => problem(StatusCode::INTERNAL_SERVER_ERROR, "Evaluation failed", message, None),
        Outcome::Store(message) => problem(StatusCode::SERVICE_UNAVAILABLE, "Event store failed", message, None),
    }
}

async fn command(State(api): State<Arc<Api>>, Path(command): Path<String>, body: Bytes) -> Response {
    let Some(validator) = api.validators.get(&command) else {
        return problem(StatusCode::NOT_FOUND, "No such command", format!("The model has no command \"{command}\"."), None);
    };
    let args: Value = match serde_json::from_slice(&body) {
        Ok(args) => args,
        Err(e) => return problem(StatusCode::BAD_REQUEST, "Invalid request", format!("The body is no JSON: {e}"), None),
    };
    let errors = validation_errors(validator, &args);
    if !errors.is_empty() {
        return problem(StatusCode::BAD_REQUEST, "Invalid arguments", format!("The arguments do not fit {command}."), Some(errors));
    }
    match api.pool.handle(&command, &args.to_string()).await {
        Ok(outcome) => outcome_response(outcome),
        // A trap – a script that ran away, a bug in the runtime. The
        // instance is gone; the next request gets a fresh one.
        Err(e) => problem(StatusCode::INTERNAL_SERVER_ERROR, "Evaluation failed", e.to_string(), None),
    }
}

/// A projection's parameters as one object, by name; every one given
/// once, and none it does not take.
fn projection_params(parameters: &[Parameter], given: Vec<(String, String)>) -> Result<Value, Vec<String>> {
    let mut params = serde_json::Map::new();
    let mut seen = std::collections::HashSet::new();
    let mut errors = Vec::new();
    for (name, text) in given {
        let Some(parameter) = parameters.iter().find(|p| p.name == name) else {
            errors.push(format!("{name}: there is no such parameter"));
            continue;
        };
        if !seen.insert(name.clone()) {
            errors.push(format!("{name}: given more than once"));
            continue;
        }
        match parameter.value(&text) {
            Ok(value) => {
                params.insert(name, value);
            }
            Err(invalid) => errors.extend(invalid),
        }
    }
    errors.extend(parameters.iter().filter(|p| !seen.contains(&p.name)).map(|p| format!("{}: missing", p.name)));
    if errors.is_empty() { Ok(Value::Object(params)) } else { Err(errors) }
}

fn projection_response(outcome: ProjectionOutcome) -> Response {
    match outcome {
        ProjectionOutcome::Projected(p) => {
            let value = serde_json::from_str::<Value>(&p.value).unwrap_or(Value::String(p.value));
            (StatusCode::OK, Json(json!({ "value": value, "position": p.position }))).into_response()
        }
        ProjectionOutcome::Invalid(message) => problem(StatusCode::BAD_REQUEST, "Invalid request", message, None),
        ProjectionOutcome::Evaluation(message) => problem(StatusCode::INTERNAL_SERVER_ERROR, "Evaluation failed", message, None),
        ProjectionOutcome::Store(message) => problem(StatusCode::SERVICE_UNAVAILABLE, "Event store failed", message, None),
    }
}

/// A projection's value as of now – so never kept by a cache.
async fn projection(
    State(api): State<Arc<Api>>,
    Path(projection): Path<String>,
    Query(given): Query<Vec<(String, String)>>,
) -> Response {
    let mut response = fold(&api, &projection, given).await;
    response.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    response
}

async fn fold(api: &Api, projection: &str, given: Vec<(String, String)>) -> Response {
    let Some(parameters) = api.projections.as_ref().and_then(|p| p.get(projection)) else {
        return problem(StatusCode::NOT_FOUND, "No such projection", format!("The model has no projection \"{projection}\"."), None);
    };
    let params = match projection_params(parameters, given) {
        Ok(params) => params,
        Err(errors) => {
            return problem(StatusCode::BAD_REQUEST, "Invalid parameters", format!("The parameters do not fit {projection}."), Some(errors));
        }
    };
    match api.pool.project(projection, &params.to_string()).await {
        Ok(outcome) => projection_response(outcome),
        // A trap, as for a command: the next request gets a fresh instance.
        Err(e) => problem(StatusCode::INTERNAL_SERVER_ERROR, "Evaluation failed", e.to_string(), None),
    }
}

/// The query `/read` takes, in the DCB specification's shape. An item
/// leaves out what it does not constrain, so none is empty.
fn read_query_schema() -> Value {
    let strings = json!({ "type": "array", "minItems": 1, "items": { "type": "string" } });
    json!({
        "type": "object",
        "description": "A DCB query. Without items – or without a body – it matches every event.",
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "description": "An event matches the query when it matches any item.",
                "items": {
                    "type": "object",
                    "description": "Matches an event of one of `types` (any, when left out) carrying every one of `tags`.",
                    "properties": { "types": strings, "tags": strings },
                    "minProperties": 1,
                    "additionalProperties": false,
                },
            },
        },
        "additionalProperties": false,
    })
}

/// `/health` in the contract.
fn health_operation() -> Value {
    let kind = json!({ "type": "string", "enum": EventStore::kinds(), "description": "Which event store this is." });
    json!({
        "get": {
            "operationId": "health",
            "tags": ["health"],
            "summary": "Event store health",
            "description": "Whether the event store is serving, which one it is, its last position and how long it took to tell.",
            "responses": {
                "200": {
                    "description": "The event store is serving.",
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {
                                    "status": { "const": "ok" },
                                    "store": {
                                        "type": "object",
                                        "properties": {
                                            "kind": kind,
                                            "head": {
                                                "anyOf": [{ "type": "integer", "minimum": 1 }, { "type": "null" }],
                                                "description": "The last position in the store – not a count of events: positions may have gaps. Null when the store is empty.",
                                            },
                                            "durationMs": { "type": "integer", "minimum": 0, "description": "How long asking the store took, in milliseconds." },
                                        },
                                        "required": ["kind", "head", "durationMs"],
                                    },
                                },
                                "required": ["status", "store"],
                            },
                        },
                    },
                },
                "503": {
                    "description": "The event store failed.",
                    "content": {
                        "application/problem+json": {
                            "schema": {
                                "allOf": [
                                    { "$ref": "#/components/schemas/ProblemDetails" },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "store": { "type": "object", "properties": { "kind": kind }, "required": ["kind"] },
                                        },
                                        "required": ["store"],
                                    },
                                ],
                            },
                        },
                    },
                },
            },
        },
    })
}

/// `/read` in the contract. Events are not typed by the model here: a
/// page holds whichever the store has.
fn read_operation() -> Value {
    let problem = |description: &str| {
        json!({
            "description": description,
            "content": { "application/problem+json": { "schema": { "$ref": "#/components/schemas/ProblemDetails" } } },
        })
    };
    json!({
        "post": {
            "operationId": "read",
            "tags": ["events"],
            "summary": "Read events",
            "description": "The events matching a DCB query, a page at a time: pass a page's `next` as `after` for the following one.",
            "parameters": [
                {
                    "name": "after",
                    "in": "query",
                    "description": "Exclusive: forwards, the events after this position; backwards, those before it.",
                    "schema": { "type": "integer", "minimum": 0 },
                },
                {
                    "name": "limit",
                    "in": "query",
                    "description": "The most events a page holds.",
                    "schema": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": DEFAULT_LIMIT },
                },
                {
                    "name": "backwards",
                    "in": "query",
                    "description": "Newest first.",
                    "schema": { "type": "boolean", "default": false },
                },
            ],
            "requestBody": { "required": false, "content": { "application/json": { "schema": read_query_schema() } } },
            "responses": {
                "200": {
                    "description": "A page of events; `next` is the position to read on `after`, null on the last page.",
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "properties": {
                                    "events": {
                                        "type": "array",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "position": { "type": "integer", "minimum": 1 },
                                                "type": { "type": "string" },
                                                "data": {},
                                                "tags": { "type": "array", "items": { "type": "string" } },
                                            },
                                            "required": ["position", "type", "data", "tags"],
                                        },
                                    },
                                    "next": { "anyOf": [{ "type": "integer", "minimum": 1 }, { "type": "null" }] },
                                },
                                "required": ["events", "next"],
                            },
                        },
                    },
                },
                "400": problem("The query or a parameter is invalid."),
                "501": problem("The event store cannot read this way."),
                "503": problem("The event store failed."),
            },
        },
    })
}

fn read_options(params: &HashMap<String, String>) -> Result<ReadOptions, String> {
    if let Some(name) = params.keys().find(|name| !["after", "limit", "backwards"].contains(&name.as_str())) {
        return Err(format!("There is no parameter \"{name}\"; use after, limit or backwards."));
    }
    let after = match params.get("after") {
        None => None,
        Some(value) => Some(value.parse().map_err(|_| format!("after is a position, not \"{value}\"."))?),
    };
    let limit = match params.get("limit") {
        None => DEFAULT_LIMIT,
        Some(value) => match value.parse() {
            Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
            _ => return Err(format!("limit is from 1 to {MAX_LIMIT}, not \"{value}\".")),
        },
    };
    let backwards = match params.get("backwards").map(String::as_str) {
        None | Some("false") => false,
        Some("true") => true,
        Some(value) => return Err(format!("backwards is true or false, not \"{value}\".")),
    };
    Ok(ReadOptions { after, limit: Some(limit), backwards })
}

/// A 400's title, detail and errors.
type Invalid = (&'static str, String, Option<Vec<String>>);

/// The query's items; none for every event.
fn read_query(validator: &jsonschema::Validator, body: &[u8]) -> Result<Option<Vec<QueryItem>>, Invalid> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let query: Value = serde_json::from_slice(body)
        .map_err(|e| ("Invalid request", format!("The body is no JSON: {e}"), None))?;
    let errors = validation_errors(validator, &query);
    if !errors.is_empty() {
        return Err(("Invalid query", "The body is no DCB query.".into(), Some(errors)));
    }
    let strings = |value: &Value| -> Vec<String> {
        value.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
    };
    Ok(query["items"].as_array().map(|items| {
        items.iter().map(|item| QueryItem { types: strings(&item["types"]), tags: strings(&item["tags"]) }).collect()
    }))
}

async fn read(State(api): State<Arc<Api>>, Query(params): Query<HashMap<String, String>>, body: Bytes) -> Response {
    let Some(validator) = &api.read else {
        return problem(StatusCode::NOT_FOUND, "Not found", "Reads are not served.", None);
    };
    let options = match read_options(&params) {
        Ok(options) => options,
        Err(message) => return problem(StatusCode::BAD_REQUEST, "Invalid parameters", message, None),
    };
    let query = match read_query(validator, &body) {
        Ok(query) => query,
        Err((title, detail, errors)) => return problem(StatusCode::BAD_REQUEST, title, detail, errors),
    };
    let store = api.pool.store();
    if let Some(reason) = store.unsupported(&options) {
        return problem(StatusCode::NOT_IMPLEMENTED, "Not supported by the event store", reason, None);
    }
    match store.read(query, options).await {
        Ok(events) => {
            // A full page may be the last; the next then comes back empty.
            let next = (Some(events.len() as u64) == options.limit).then(|| events.last().map(|e| e.position)).flatten();
            let events: Vec<Value> = events
                .into_iter()
                .map(|sequenced| {
                    let mut event = event_json(sequenced.event);
                    event["position"] = json!(sequenced.position);
                    event
                })
                .collect();
            (StatusCode::OK, Json(json!({ "events": events, "next": next }))).into_response()
        }
        Err(message) => problem(StatusCode::SERVICE_UNAVAILABLE, "Event store failed", message, None),
    }
}

async fn openapi(State(api): State<Arc<Api>>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], api.contract.clone()).into_response()
}

/// The store's health, then its head: a store that cannot name its last
/// position cannot decide a command either.
async fn health(State(api): State<Arc<Api>>) -> Response {
    let store = api.pool.store();
    let started = Instant::now();
    let head = match store.health().await {
        Ok(()) => store.head().await,
        Err(reason) => Err(reason),
    };
    match head {
        Ok(head) => {
            let duration = started.elapsed().as_millis() as u64;
            let store = json!({ "kind": store.kind(), "head": head, "durationMs": duration });
            (StatusCode::OK, Json(json!({ "status": "ok", "store": store }))).into_response()
        }
        Err(reason) => {
            let body = json!({
                "title": "Event store unavailable",
                "status": StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                "detail": reason,
                "store": { "kind": store.kind() },
            });
            (StatusCode::SERVICE_UNAVAILABLE, [(header::CONTENT_TYPE, "application/problem+json")], body.to_string()).into_response()
        }
    }
}

const DOCS_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>DCB Model API</title>
  <link rel="stylesheet" href="/docs/swagger-ui.css">
</head>
<body>
  <div id="swagger-ui"></div>
  <script src="/docs/swagger-ui-bundle.js"></script>
  <script>
    window.ui = SwaggerUIBundle({ url: '/openapi.json', dom_id: '#swagger-ui', deepLinking: true });
  </script>
</body>
</html>
"#;

fn docs_router() -> Router<Arc<Api>> {
    let asset = |content_type: &'static str, bytes: &'static [u8]| {
        get(move || async move { ([(header::CONTENT_TYPE, content_type)], bytes) })
    };
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/docs") }))
        .route("/docs", get(|| async { ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], DOCS_HTML) }))
        .route(
            "/docs/swagger-ui.css",
            asset("text/css", include_bytes!("../assets/swagger-ui/swagger-ui.css")),
        )
        .route(
            "/docs/swagger-ui-bundle.js",
            asset("text/javascript", include_bytes!("../assets/swagger-ui/swagger-ui-bundle.js")),
        )
}

/// What a request body may hold – axum's own default, which reading it
/// for the log would otherwise lift.
const REQUEST_BODY_MAX: usize = 2 * 1024 * 1024;

/// How much of a body the log shows.
const LOGGED_BODY_MAX: usize = 4096;

/// A body for the log, cut to `LOGGED_BODY_MAX`; none when empty.
fn logged_body(bytes: &[u8]) -> Option<String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    let text = String::from_utf8_lossy(bytes);
    Some(match text.char_indices().nth(LOGGED_BODY_MAX) {
        Some((end, _)) => format!("{}… ({} bytes)", &text[..end], bytes.len()),
        None => text.into_owned(),
    })
}

/// Every line logged while serving a request – the store's calls
/// among them – is in its span, which names the method and the URI, and
/// numbers the request, to tell concurrent ones apart.
async fn log_request(request: Request, next: Next) -> Response {
    static REQUESTS: AtomicU64 = AtomicU64::new(1);
    let id = REQUESTS.fetch_add(1, Ordering::Relaxed);
    let span = tracing::info_span!(target: "http", "request", id, method = %request.method(), uri = %request.uri());
    serve_logged(request, next).instrument(span).await
}

async fn serve_logged(request: Request, next: Next) -> Response {
    let started = Instant::now();
    if !tracing::enabled!(target: "http", tracing::Level::DEBUG) {
        let response = next.run(request).await;
        tracing::info!(target: "http", status = response.status().as_u16(), duration_ms = ms(started), "response");
        return response;
    }
    // The bodies are read whole to be logged, then handed on as they were.
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, REQUEST_BODY_MAX).await else {
        return problem(StatusCode::PAYLOAD_TOO_LARGE, "Payload too large", format!("The body is over {REQUEST_BODY_MAX} bytes, or could not be read."), None);
    };
    if let Some(text) = logged_body(&bytes) {
        tracing::debug!(target: "http", body = %text, "request body");
    }
    let contract = parts.uri.path() == "/openapi.json";
    let response = next.run(Request::from_parts(parts, Body::from(bytes))).await;
    let (parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return problem(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", "The response could not be read.", None);
    };
    let status = parts.status.as_u16();
    tracing::info!(target: "http", status, duration_ms = ms(started), "response");
    // JSON answers only – not the contract, not the docs' assets.
    let json = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
    if json && !contract
        && let Some(text) = logged_body(&bytes)
    {
        tracing::debug!(target: "http", status, body = %text, "response body");
    }
    Response::from_parts(parts, Body::from(bytes))
}

/// `/read` when `api` serves reads, `/projections/…` when it serves
/// projections; `/docs` with `docs`.
pub fn router(api: Arc<Api>, docs: bool) -> Router {
    let mut router = Router::new()
        .route("/commands/{command}", post(command))
        .route("/openapi.json", get(openapi))
        .route("/health", get(health));
    if api.read.is_some() {
        router = router.route("/read", post(read));
    }
    if api.projections.is_some() {
        router = router.route("/projections/{projection}", get(projection));
    }
    if docs {
        router = router.merge(docs_router());
    }
    router.layer(middleware::from_fn(log_request)).with_state(api)
}
