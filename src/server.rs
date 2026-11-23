use crate::core::{Core, Denied, Invalid};
use crate::settings::Settings;
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::{any, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    method: String,
    #[serde(default)]
    params: serde_json::Map<String, Value>,
}
fn text<'a>(p: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str> {
    p.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Invalid("required string parameter missing").into())
}
fn tag_list(v: &Value) -> Result<Vec<String>> {
    v.as_array()
        .ok_or(Invalid("tags must be an array of strings"))?
        .iter()
        .map(|t| {
            t.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Invalid("tags must be an array of strings").into())
        })
        .collect()
}
fn int(p: &serde_json::Map<String, Value>, key: &str) -> Result<Option<i64>> {
    p.get(key)
        .map(|v| {
            v.as_i64()
                .ok_or_else(|| anyhow::Error::from(Invalid("numeric parameter must be an integer")))
        })
        .transpose()
}
async fn dispatch(core: &Arc<Core>, actor: &str, request: Request) -> Result<Value> {
    let p = &request.params;
    let allowed: &[&str] = match request.method.as_str() {
        "snapshot" | "runners" => &[],
        "work" => &["work_id"],
        "validate" => &["definition"],
        "runs" => &["work_id", "status", "limit", "before"],
        "register" => &["work_id", "definition"],
        "enable" => &["work_id", "enabled"],
        "run" => &["work_id", "key", "params"],
        "inspect" => &["run_id", "effect_id", "step_id"],
        "cancel" => &["run_id"],
        "retry" => &["run_id", "key"],
        "reconcile" => &[
            "run_id",
            "decision",
            "note",
            "output",
            "settled",
            "effect_id",
        ],
        "schedule" => &["name", "work_id", "cron", "tz", "params", "tags"],
        "schedule_update" => &["name", "cron", "tz", "enabled", "params", "tags"],
        "schedules" => &["tag"],
        "schedule_action" => &["name", "action"],
        "schedule_occurrences" => &["name", "limit"],
        "schedule_show" => &["name", "limit"],
        "schedule_preview" => &["cron", "tz", "count"],
        "queues" => &["queue", "limit"],
        "schedule_trigger" => &["name"],
        "overview" | "schedule_stats" | "skills" | "settings" | "providers" | "backups" => &[],
        "agents" => &["limit"],
        "run_logs" => &["run_id", "source", "after", "limit"],
        "approvals" => &["state", "limit"],
        "approve" | "reject" => &["approval_id", "note"],
        "skill_show" => &["name"],
        "settings_set" => &["key", "value"],
        "provider_set" => &["kind", "name", "definition"],
        "provider_remove" => &["kind", "name"],
        "backup_start" => &["name"],
        "subagent_spawn" => &[
            "profile",
            "prompt",
            "instructions",
            "key",
            "permissions",
            "output_schema",
        ],
        "subagent_poll" => &["run_id", "max_bytes"],
        "subagent_list" => &["limit"],
        "subagent_cancel" => &["run_id"],
        "doctor" => &[],
        "runner_test" => &["name"],
        "definitions_export" => &[],
        "definitions_import" => &["document", "replace", "dry_run"],
        "service" => &["action", "name", "env_file", "start"],
        "plugin_list" | "plugin_tools" => &[],
        "plugin_show" => &["id", "check"],
        "plugin_install" => &["source", "dry_run"],
        "plugin_enable" | "plugin_disable" | "plugin_uninstall" => &["id"],
        "plugin_run" => &["id", "command"],
        "plugin_mcp" => &["tool", "arguments"],
        _ => return Err(Invalid("unknown operation").into()),
    };
    if p.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(Invalid("unsupported or authority-forging parameters").into());
    }
    match request.method.as_str() {
        "snapshot" => core.snapshot(actor).await,
        "runners" => core.runners(actor).await,
        "work" => {
            core.permit(actor, "read", None)?;
            core.work(actor, text(p, "work_id")?)
        }
        "validate" => core.validate_only(
            actor,
            p.get("definition").ok_or(Invalid("definition missing"))?,
        ),
        "runs" => {
            let optional = |key| p.get(key).map(|_| text(p, key)).transpose();
            core.search_runs(
                actor,
                optional("work_id")?,
                optional("status")?,
                p.get("limit")
                    .map(|v| v.as_i64().ok_or(Invalid("limit must be an integer")))
                    .transpose()?,
                optional("before")?,
            )
            .await
        }
        "register" => core.register(
            actor,
            text(p, "work_id")?,
            p.get("definition")
                .cloned()
                .ok_or(Invalid("definition missing"))?,
        ),
        "enable" => core.enable(
            actor,
            text(p, "work_id")?,
            p.get("enabled")
                .and_then(Value::as_bool)
                .ok_or(Invalid("enabled must be boolean"))?,
        ),
        "run" => {
            core.run_with_params(
                actor,
                text(p, "work_id")?,
                text(p, "key")?,
                None,
                p.get("params").cloned(),
            )
            .await
        }
        "inspect" => {
            core.inspect_selected(
                actor,
                text(p, "run_id")?,
                p.get("effect_id")
                    .map(|_| text(p, "effect_id"))
                    .transpose()?,
                p.get("step_id")
                    .map(|v| {
                        v.as_i64()
                            .filter(|id| *id >= 0)
                            .ok_or(Invalid("step_id must be a nonnegative integer"))
                    })
                    .transpose()?,
            )
            .await
        }
        "cancel" => core.cancel(actor, text(p, "run_id")?).await,
        "retry" => core.retry(actor, text(p, "run_id")?, text(p, "key")?).await,
        "reconcile" => {
            core.reconcile_selected(
                actor,
                text(p, "run_id")?,
                text(p, "decision")?,
                text(p, "note")?,
                p.get("output").cloned().unwrap_or(Value::Null),
                p.get("settled").and_then(Value::as_bool).unwrap_or(false),
                p.get("effect_id")
                    .map(|_| text(p, "effect_id"))
                    .transpose()?,
            )
            .await
        }
        "schedule" => {
            core.schedule_tagged(
                actor,
                text(p, "name")?,
                text(p, "work_id")?,
                text(p, "cron")?,
                text(p, "tz")?,
                p.get("params").cloned(),
                p.get("tags").map(tag_list).transpose()?,
            )
            .await
        }
        "schedule_update" => {
            core.schedule_update(
                actor,
                text(p, "name")?,
                crate::schedule_edit::ScheduleEdit {
                    cron: p
                        .get("cron")
                        .map(|_| text(p, "cron").map(str::to_owned))
                        .transpose()?,
                    tz: p
                        .get("tz")
                        .map(|_| text(p, "tz").map(str::to_owned))
                        .transpose()?,
                    enabled: p
                        .get("enabled")
                        .map(|v| v.as_bool().ok_or(Invalid("enabled must be boolean")))
                        .transpose()?,
                    params: p.get("params").map(
                        |v| {
                            if v.is_null() { None } else { Some(v.clone()) }
                        },
                    ),
                    tags: p.get("tags").map(tag_list).transpose()?,
                },
            )
            .await
        }
        "schedules" => {
            core.schedules_tagged(actor, p.get("tag").map(|_| text(p, "tag")).transpose()?)
                .await
        }
        "schedule_action" => {
            core.schedule_action(actor, text(p, "name")?, text(p, "action")?)
                .await
        }
        "schedule_occurrences" => {
            core.schedule_occurrences(
                actor,
                text(p, "name")?,
                p.get("limit")
                    .map(|v| v.as_i64().ok_or(Invalid("limit must be an integer")))
                    .transpose()?,
            )
            .await
        }
        "schedule_show" => {
            core.schedule_show(
                actor,
                text(p, "name")?,
                p.get("limit")
                    .map(|v| v.as_i64().ok_or(Invalid("limit must be an integer")))
                    .transpose()?,
            )
            .await
        }
        "schedule_preview" => core.schedule_preview(
            actor,
            text(p, "cron")?,
            text(p, "tz")?,
            p.get("count")
                .map(|v| v.as_i64().ok_or(Invalid("count must be an integer")))
                .transpose()?,
        ),
        "schedule_trigger" => core.schedule_trigger(actor, text(p, "name")?).await,
        "queues" => {
            core.queues(
                actor,
                p.get("queue").map(|_| text(p, "queue")).transpose()?,
                p.get("limit")
                    .map(|v| v.as_i64().ok_or(Invalid("limit must be an integer")))
                    .transpose()?,
            )
            .await
        }
        "overview" => core.overview(actor).await,
        "agents" => core.agents(actor, int(p, "limit")?).await,
        "schedule_stats" => core.schedule_stats(actor).await,
        "run_logs" => {
            core.run_logs(
                actor,
                text(p, "run_id")?,
                p.get("source").map(|_| text(p, "source")).transpose()?,
                int(p, "after")?,
                int(p, "limit")?,
            )
            .await
        }
        "approvals" => {
            core.approvals(
                actor,
                p.get("state").map(|_| text(p, "state")).transpose()?,
                int(p, "limit")?,
            )
            .await
        }
        "approve" | "reject" => {
            core.approval_decide(
                actor,
                text(p, "approval_id")?,
                &request.method,
                text(p, "note")?,
            )
            .await
        }
        "skills" => core.skills(actor),
        "skill_show" => core.skill_show(actor, text(p, "name")?),
        "settings" => core.settings_view(actor),
        "settings_set" => core.settings_set(
            actor,
            text(p, "key")?,
            p.get("value")
                .ok_or(Invalid("value missing; use null to unset"))?,
        ),
        "providers" => core.providers(actor).await,
        "provider_set" => core.provider_set(
            actor,
            text(p, "kind")?,
            text(p, "name")?,
            p.get("definition").ok_or(Invalid("definition missing"))?,
        ),
        "provider_remove" => core.provider_remove(actor, text(p, "kind")?, text(p, "name")?),
        "backups" => core.backups(actor),
        "backup_start" => {
            core.backup_start(actor, p.get("name").map(|_| text(p, "name")).transpose()?)
                .await
        }
        "subagent_spawn" => {
            let optional = |key: &str| p.get(key).map(|_| text(p, key)).transpose();
            core.subagent_spawn(
                actor,
                crate::subagents::Spawn {
                    profile: text(p, "profile")?,
                    prompt: text(p, "prompt")?,
                    instructions: optional("instructions")?,
                    key: text(p, "key")?,
                    permissions: optional("permissions")?,
                    schema: p.get("output_schema"),
                },
            )
            .await
        }
        "subagent_poll" => {
            core.subagent_poll(actor, text(p, "run_id")?, int(p, "max_bytes")?)
                .await
        }
        "subagent_list" => core.subagent_list(actor, int(p, "limit")?).await,
        "subagent_cancel" => core.subagent_cancel(actor, text(p, "run_id")?).await,
        "doctor" => core.doctor(actor).await,
        "runner_test" => core.runner_test(actor, text(p, "name")?).await,
        "definitions_export" => core.definitions_export(actor).await,
        "definitions_import" => {
            core.definitions_import(
                actor,
                p.get("document").ok_or(Invalid("document missing"))?,
                p.get("replace").and_then(Value::as_bool).unwrap_or(false),
                p.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
            )
            .await
        }
        "service" => {
            core.service(
                actor,
                text(p, "action")?,
                p.get("name").map(|_| text(p, "name")).transpose()?,
                p.get("env_file").map(|_| text(p, "env_file")).transpose()?,
                p.get("start").and_then(Value::as_bool).unwrap_or(false),
            )
            .await
        }
        "plugin_list" => core.plugins_list(actor),
        "plugin_tools" => core.plugin_tools(actor),
        "plugin_show" => {
            core.plugin_show(
                actor,
                text(p, "id")?,
                p.get("check").and_then(Value::as_bool).unwrap_or(false),
            )
            .await
        }
        "plugin_install" => {
            core.plugin_install(
                actor,
                text(p, "source")?,
                p.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
            )
            .await
        }
        "plugin_enable" => core.plugin_enable(actor, text(p, "id")?).await,
        "plugin_disable" => core.plugin_disable(actor, text(p, "id")?).await,
        "plugin_uninstall" => core.plugin_uninstall(actor, text(p, "id")?).await,
        "plugin_run" => {
            let command: Vec<String> = p
                .get("command")
                .and_then(Value::as_array)
                .ok_or(Invalid("command must be an array of strings"))?
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<_>>()
                .ok_or(Invalid("command must be an array of strings"))?;
            core.plugin_run(actor, text(p, "id")?, &command).await
        }
        "plugin_mcp" => {
            core.plugin_mcp(
                actor,
                text(p, "tool")?,
                p.get("arguments").cloned().unwrap_or_else(|| json!({})),
            )
            .await
        }
        _ => unreachable!(),
    }
}
/// Status and message of a failed operation; never includes internal detail.
fn failure(error: &anyhow::Error) -> (StatusCode, String) {
    if let Some(e) = error.downcast_ref::<Denied>() {
        (StatusCode::FORBIDDEN, {
            let _ = e;
            error.to_string()
        })
    } else if let Some(e) = error.downcast_ref::<Invalid>() {
        (StatusCode::BAD_REQUEST, e.0.to_owned())
    } else if error.is::<crate::work_input::InvalidWorkInput>()
        || error.is::<crate::core::InvalidFlow>()
    {
        (StatusCode::BAD_REQUEST, error.to_string())
    } else if let Some(e) = error.downcast_ref::<crate::plugin_registry::PluginError>() {
        (StatusCode::BAD_REQUEST, e.0.clone())
    } else if let Some(e) = error.downcast_ref::<crate::settings::ConfigError>() {
        // Configuration refusals name a setting, never a value.
        (StatusCode::BAD_REQUEST, e.0.clone())
    } else if let Some(e) = error.downcast_ref::<crate::plugin_manifest::ManifestError>() {
        (StatusCode::BAD_REQUEST, e.to_string())
    } else if let Some(crate::dbos::admission::Error::Invalid(reason)) =
        error.downcast_ref::<crate::dbos::admission::Error>()
    {
        (StatusCode::BAD_REQUEST, (*reason).to_owned())
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "Service unavailable; inspect scoped state and operator configuration".to_owned(),
        )
    }
}
fn bearer(core: &Core, headers: &HeaderMap) -> Result<String> {
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(Denied("Bearer authentication required"))?;
    core.actor(token)
}
pub(crate) async fn invoke_mcp(
    core: &Arc<Core>,
    actor: &str,
    method: &str,
    params: Value,
) -> std::result::Result<Value, String> {
    let params = match params {
        Value::Object(params) => params,
        _ => return Err("Forge tool parameters must be an object".into()),
    };
    dispatch(
        core,
        actor,
        Request {
            method: method.into(),
            params,
        },
    )
    .await
    .map_err(|error| failure(&error).1)
}

async fn authorize_mcp(
    State(core): State<Arc<Core>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Ok(actor) = bearer(&core, request.headers()) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            "Forge bearer authentication required",
        )
            .into_response();
    };
    request
        .headers_mut()
        .remove(axum::http::header::AUTHORIZATION);
    request
        .extensions_mut()
        .insert(crate::mcp::Principal(actor));
    next.run(request).await
}

/// `/api/plugins/<id>/<route>`: the same bearer authentication as `/api`, then only
/// routes and methods the plugin's manifest declares, with its declared body limit.
async fn plugin_http(
    State(core): State<Arc<Core>>,
    Path((id, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let result = async {
        let actor = bearer(&core, &headers)?;
        core.plugin_api(
            &actor,
            &id,
            method.as_str(),
            &rest,
            uri.query().unwrap_or(""),
            &body,
        )
        .await
    }
    .await;
    match result {
        Ok(reply) => {
            let mut response = Response::new(axum::body::Body::from(reply.body));
            *response.status_mut() =
                StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
            for (name, value) in reply.headers {
                if let (Ok(name), Ok(value)) = (
                    axum::http::HeaderName::from_bytes(name.as_bytes()),
                    axum::http::HeaderValue::from_str(&value),
                ) {
                    response.headers_mut().insert(name, value);
                }
            }
            let extra = response.headers_mut();
            extra.insert("x-content-type-options", "nosniff".parse().expect("static"));
            extra
                .entry("cache-control")
                .or_insert("no-store".parse().expect("static"));
            response
        }
        Err(error) => {
            let (status, message) = failure(&error);
            (status, Json(json!({"error":message}))).into_response()
        }
    }
}
/// Plugin dashboard assets: only declared assets of an enabled plugin, served from
/// the digest-verified release bytes with a restrictive same-origin policy.
async fn plugin_asset(
    State(core): State<Arc<Core>>,
    Path((id, path)): Path<(String, String)>,
) -> Response {
    match core.plugin_asset(&id, &path).await {
        Ok((kind, bytes)) => (
            [
                (axum::http::header::CONTENT_TYPE, kind),
                (
                    axum::http::header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
                ),
                (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
/// `POST /api/stream` `{"run_id":...}`: the authenticated live event stream of one run
/// (server-sent events over a fetch response). The bearer token travels in the
/// `Authorization` header like every other call, never in a URL.
async fn stream(
    State(core): State<Arc<Core>>,
    headers: HeaderMap,
    Json(body): Json<StreamRequest>,
) -> Response {
    let opened = (|| -> Result<()> {
        let actor = bearer(&core, &headers)?;
        core.owned(&actor, &body.run_id)?;
        Ok(())
    })();
    if let Err(error) = opened {
        let (status, message) = failure(&error);
        return (status, Json(json!({"ok":false,"error":message}))).into_response();
    }
    let events = futures::stream::unfold(
        crate::run_logs::RunStream::new(core, body.run_id),
        |mut state| async move {
            let bytes: Vec<u8> = loop {
                let batch = state.next_batch().await?;
                if !batch.is_empty() {
                    break batch.iter().flat_map(|b| b.to_vec()).collect();
                }
            };
            Some((Ok::<_, std::convert::Infallible>(Bytes::from(bytes)), state))
        },
    );
    let mut response = Response::new(axum::body::Body::from_stream(events));
    let head = response.headers_mut();
    head.insert(
        axum::http::header::CONTENT_TYPE,
        "text/event-stream; charset=utf-8".parse().expect("static"),
    );
    head.insert(
        axum::http::header::CACHE_CONTROL,
        "no-store".parse().expect("static"),
    );
    head.insert("x-accel-buffering", "no".parse().expect("static"));
    response
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamRequest {
    run_id: String,
}
async fn api(
    State(core): State<Arc<Core>>,
    headers: HeaderMap,
    Json(body): Json<Request>,
) -> Response {
    let result = async {
        let actor = bearer(&core, &headers)?;
        dispatch(&core, &actor, body).await
    }
    .await;
    match result {
        Ok(result) => Json(json!({"ok":true,"result":result})).into_response(),
        Err(error) => {
            let (status, message) = failure(&error);
            (status, Json(json!({"ok":false,"error":message}))).into_response()
        }
    }
}
/// The dashboard's static assets, compiled in: no build step and no filesystem access.
const ASSETS: &[(&str, &str, &str)] = &[
    (
        "forge-core.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/forge-core.js"),
    ),
    (
        "forge-ui.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/forge-ui.js"),
    ),
    (
        "forge-views.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/forge-views.js"),
    ),
    (
        "forge-ui.css",
        "text/css; charset=utf-8",
        include_str!("../assets/forge-ui.css"),
    ),
    (
        "forge-base.css",
        "text/css; charset=utf-8",
        include_str!("../assets/forge-base.css"),
    ),
];
async fn static_asset(Path(name): Path<String>) -> Response {
    match ASSETS.iter().find(|(file, ..)| *file == name) {
        Some((_, kind, body)) => {
            ([(axum::http::header::CONTENT_TYPE, *kind)], *body).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
/// Restrictive same-origin policy for every dashboard response: scripts and styles
/// only from this origin (the page has no inline script or style), no framing, no
/// MIME sniffing, no referrer and no caching of API answers. A response that already
/// carries its own policy (plugin assets) keeps it.
pub(crate) const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
async fn security_headers<B>(mut response: axum::http::Response<B>) -> axum::http::Response<B> {
    use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, X_CONTENT_TYPE_OPTIONS};
    let headers = response.headers_mut();
    headers
        .entry(CONTENT_SECURITY_POLICY)
        .or_insert(axum::http::HeaderValue::from_static(CSP));
    headers.insert(X_CONTENT_TYPE_OPTIONS, "nosniff".parse().expect("static"));
    headers.insert("x-frame-options", "DENY".parse().expect("static"));
    headers.insert("referrer-policy", "no-referrer".parse().expect("static"));
    headers.insert(
        "cross-origin-opener-policy",
        "same-origin".parse().expect("static"),
    );
    headers
        .entry(CACHE_CONTROL)
        .or_insert(axum::http::HeaderValue::from_static("no-store"));
    response
}
/// Every HTTP route of the service (also used by tests against a real listener).
pub(crate) fn router(core: Arc<Core>) -> Router {
    let routes = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../assets/dashboard.html")) }),
        )
        .route("/assets/{name}", get(static_asset))
        .route("/api", post(api))
        .route("/api/stream", post(stream))
        .route("/api/plugins/{id}/{*rest}", any(plugin_http))
        .route("/plugins/{id}/assets/{*path}", get(plugin_asset))
        .with_state(core.clone());
    let routes = if let Some(server) = core.settings.get().map(|settings| &settings.server)
        && server.mcp_http
    {
        routes.merge(
            crate::mcp::http(core.clone(), server.host)
                .route_layer(axum::middleware::from_fn_with_state(core, authorize_mcp)),
        )
    } else {
        routes
    };
    routes
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(axum::middleware::map_response(security_headers))
}
pub async fn serve(settings: Settings, policy: crate::core::Policy) -> Result<()> {
    let (host, port) = (settings.server.host, settings.server.port);
    if !host.is_loopback() {
        return crate::settings::refuse(format!(
            "server host {host} is not a loopback address: Forge serves only 127.0.0.0/8 or ::1"
        ));
    }
    let tls = settings
        .server
        .tls
        .as_ref()
        .map(crate::tls::acceptor)
        .transpose()?;
    let home = settings.paths.home.clone();
    let config = settings.runtime_config(&policy)?;
    let resolved = settings.clone();
    settings.install();
    settings.paths.ensure_dirs()?;
    settings.paths.check_layout()?;
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(host, port))
        .await
        .context("loopback listener unavailable")?;
    let mut runtime = crate::dbos::Runtime::new(&settings.paths, config.clone())?;
    // Runtime retains the exclusive home owner lock across lifecycle recovery,
    // describe and later close, so these fixed lifecycle identities are never
    // used concurrently within one home.
    crate::plugin::reconcile_lifecycle(&home)?;
    let store = Arc::new(crate::dbos::admission::AdmissionStore::initialize(
        &settings.paths,
        &policy.home_id,
    )?);
    let plugins = crate::plugin::load(&home, &policy.plugins).await?;
    let core = Core::new(
        settings.paths.clone(),
        policy,
        config.clone(),
        store.clone(),
        plugins,
    )?;
    let _ = core.settings.set(resolved);
    // Before any DBOS recovery can run: terminate orphaned plugin and harness trees
    // from a previous process without retrying their unresolved effect intents.
    core.reconcile_plugin_effects()?;
    core.reconcile_harness_attempts()?;
    runtime.application = Some(core.clone());
    let c = core.clone();
    runtime.register_workflow(crate::core::WORKFLOW, move |input| {
        crate::workflows::execute(c.clone(), input)
    })?;
    let c = core.clone();
    runtime.register_workflow(crate::core::SCHEDULED, move |input| {
        crate::workflows::scheduled(c.clone(), input)
    })?;
    let bridge = runtime.install_submission_bridge(store)?;
    core.bridge
        .set(bridge)
        .map_err(|_| anyhow::anyhow!("bridge initialized twice"))?;
    // Native migrations/queues precede client connection and all recovery.
    runtime.prepare().await?;
    core.client
        .set(config.connect_client().await?)
        .map_err(|_| anyhow::anyhow!("client initialized twice"))?;
    // Prepare accepts only the exact authorized pending and previous native
    // definitions. Reconcile the chosen revision before launching the scheduler.
    for name in core.heal_schedule_edits().await? {
        eprintln!(
            "pending schedule edit {name:?} could not be reconciled; retained for operator inspection"
        );
    }
    // Admissions without a native workflow (a restored or damaged database) become
    // RECOVERY_REQUIRED; native workflows without an admission are reported. Neither
    // is replayed or run blindly.
    let consistency = core.check_consistency().await?;
    for (key, what) in [
        (
            "lost",
            "admitted runs without a native workflow (RECOVERY_REQUIRED; reconcile them)",
        ),
        (
            "orphans",
            "native workflows without a Forge admission (reported, not run by Forge)",
        ),
    ] {
        if let Some(n) = consistency[key].as_array().filter(|a| !a.is_empty()) {
            eprintln!("consistency: {} {what}", n.len());
        }
    }
    // Installed plugins are loaded (and unfinished installs removed) before DBOS
    // recovery can re-enter a run that uses one.
    core.plugins_startup().await?;
    runtime.launch().await?;
    // A decision recorded just before a crash may not have woken its waiting run.
    core.approvals_redeliver().await?;
    let app = router(core.clone());
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal registration");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
    };
    let result = if let Some(acceptor) = tls {
        axum::serve(crate::tls::Listener::new(listener, acceptor), app)
            .with_graceful_shutdown(shutdown)
            .await
    } else {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
    };
    runtime.shutdown().await;
    let close = crate::plugin::close(&core.home, &core.plugins).await;
    if let Some(client) = core.client.get() {
        client.close().await;
    }
    close?;
    result?;
    Ok(())
}
