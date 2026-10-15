//! The HTTP API: one `POST /commands/{Command}` per command of the
//! model, described by the contract the component builds
//! (`/openapi.json`) and checked against it before the component sees a
//! request – what is documented is what is enforced.
//!
//!   200 published   422 rejected   400 arguments do not fit
//!   404 no such command   409 conflict   500 evaluation   503 store
//!
//! Failures are RFC 9457 problem details. `/docs` is Swagger UI over the
//! contract, embedded, so it works without network access.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::component::{Event, Outcome, Pool};

pub struct Api {
    pool: Pool,
    contract: String,
    validators: HashMap<String, jsonschema::Validator>,
}

impl Api {
    /// Reads the contract off the component and compiles one validator
    /// per command from it. A request schema names custom types by
    /// `$ref` into the contract's components, so each is compiled with
    /// those components beside it.
    pub async fn new(pool: Pool) -> wasmtime::Result<Self> {
        let contract = pool.contract().await?;
        let document: Value = serde_json::from_str(&contract)?;
        let mut validators = HashMap::new();
        for (path, item) in document["paths"].as_object().into_iter().flatten() {
            let Some(command) = path.strip_prefix("/commands/") else { continue };
            let mut schema = item["post"]["requestBody"]["content"]["application/json"]["schema"].clone();
            schema["components"] = document["components"].clone();
            let validator = jsonschema::draft202012::new(&schema)
                .map_err(|e| wasmtime::format_err!("the contract's schema for {command}: {e}"))?;
            validators.insert(command.to_string(), validator);
        }
        Ok(Self { pool, contract, validators })
    }
}

fn problem(status: StatusCode, title: &str, detail: impl Into<String>, errors: Option<Vec<String>>) -> Response {
    let mut body = json!({ "title": title, "status": status.as_u16(), "detail": detail.into() });
    if let Some(errors) = errors {
        body["errors"] = json!(errors);
    }
    (status, [(header::CONTENT_TYPE, "application/problem+json")], body.to_string()).into_response()
}

fn events_json(events: Vec<Event>) -> Vec<Value> {
    events
        .into_iter()
        .map(|e| {
            json!({
                "type": e.event_type,
                "data": serde_json::from_str::<Value>(&e.data).unwrap_or(Value::String(e.data)),
                "tags": e.tags,
            })
        })
        .collect()
}

pub fn outcome_response(outcome: Outcome) -> Response {
    match outcome {
        Outcome::Published(p) => {
            (StatusCode::OK, Json(json!({ "events": events_json(p.events), "position": p.position }))).into_response()
        }
        Outcome::Rejected(r) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "rejection": r.rejection, "ruleIndex": r.rule_index })),
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
    let errors: Vec<String> = validator
        .iter_errors(&args)
        .map(|e| format!("{}: {}", if e.instance_path().as_str().is_empty() { "/" } else { e.instance_path().as_str() }, e))
        .collect();
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

async fn openapi(State(api): State<Arc<Api>>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], api.contract.clone()).into_response()
}

async fn health(State(api): State<Arc<Api>>) -> Response {
    match api.pool.store().health().await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(reason) => problem(StatusCode::SERVICE_UNAVAILABLE, "Event store unavailable", reason, None),
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

pub fn router(api: Arc<Api>, docs: bool) -> Router {
    let mut router = Router::new()
        .route("/commands/{command}", post(command))
        .route("/openapi.json", get(openapi))
        .route("/health", get(health));
    if docs {
        router = router.merge(docs_router());
    }
    router.with_state(api)
}
