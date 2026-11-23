//! HTTP-level tests of the dashboard's security posture and the authenticated stream.
use crate::{
    core::{self, Core, hash},
    dbos::admission::{AdmissionStore, Envelope, Invocation},
    settings::Paths,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

fn core() -> (tempfile::TempDir, Arc<Core>) {
    let home = tempfile::tempdir().unwrap();
    core::init(home.path()).unwrap();
    let mut policy = core::load_policy(home.path()).unwrap();
    policy.actors.insert(
        "stranger".into(),
        core::Actor {
            token: "s".repeat(32),
            capabilities: vec!["read".into(), "execute".into()],
            types: vec![],
        },
    );
    let config = crate::settings::Settings::for_tests(&policy)
        .runtime_config(&policy)
        .unwrap();
    let store = Arc::new(
        AdmissionStore::initialize(&Paths::defaults(home.path()), &policy.home_id).unwrap(),
    );
    let core = Core::new(
        Paths::defaults(home.path()),
        policy,
        config,
        store,
        BTreeMap::new(),
    )
    .unwrap();
    (home, core)
}
fn admit(core: &Core, key: &str) -> Envelope {
    let definition = json!({"type":"echo","input":{"text":"x"}});
    core.register("operator", "w", definition.clone()).ok();
    let invocation = Invocation::from_json(&json!({"format_version":1,"home_id":core.policy.home_id,"scope":"operator","workflow_name":core::WORKFLOW,"request_id":key,"workflow_version":core::VERSION,"principal":"operator","workspace":"w","plugin":"echo","resource":"bounded-work-queue","policy_ref":"configured-actor","definition_sha256":hash(crate::canonical::to_vec(&definition).unwrap()),"action_sha256":core.action_hash("operator",&definition).unwrap(),"parameters":{},"evidence":[],"authorization_expires_at_ms":null}).to_string()).unwrap();
    core.store.admit(&invocation).unwrap().envelope
}
async fn serve(core: Arc<Core>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, crate::server::router(core))
            .await
            .unwrap();
    });
    format!("http://{address}")
}

#[tokio::test]
async fn every_core_response_carries_the_restrictive_security_headers() {
    let (_home, core) = core();
    let base = serve(core).await;
    let client = reqwest::Client::new();
    for (path, post) in [
        ("/", false),
        ("/assets/forge-ui.js", false),
        ("/assets/forge-core.js", false),
        ("/assets/forge-ui.css", false),
        ("/assets/forge-base.css", false),
        ("/assets/missing.js", false),
        ("/api", true),
    ] {
        let request = if post {
            client
                .post(format!("{base}{path}"))
                .json(&json!({"method":"snapshot"}))
        } else {
            client.get(format!("{base}{path}"))
        };
        let response = request.send().await.unwrap();
        let h = response.headers();
        assert_eq!(h["content-security-policy"], crate::server::CSP, "{path}");
        assert_eq!(h["x-content-type-options"], "nosniff", "{path}");
        assert_eq!(h["x-frame-options"], "DENY", "{path}");
        assert_eq!(h["referrer-policy"], "no-referrer", "{path}");
        assert!(
            h["cache-control"].to_str().unwrap().contains("no-store"),
            "{path}"
        );
    }
    let csp = crate::server::CSP;
    for needed in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
        "connect-src 'self'",
    ] {
        assert!(csp.contains(needed), "{needed}");
    }
    assert!(!csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"));
}

#[test]
fn the_dashboard_is_compatible_with_the_policy_and_renders_data_safely() {
    let page = include_str!("../assets/dashboard.html");
    // No inline script or style, and no inline event handler or style attribute.
    assert!(!page.contains("<style"));
    for script in page.split("<script").skip(1) {
        let tag = script.split('>').next().unwrap();
        assert!(tag.contains("src="), "inline script: {tag}");
    }
    for forbidden in [
        " onclick=",
        " onload=",
        " onsubmit=",
        " style=",
        "javascript:",
    ] {
        assert!(!page.contains(forbidden), "{forbidden}");
    }
    // No script builds markup from data: rendering uses textContent and DOM nodes only.
    for (name, source) in [
        ("forge-core.js", include_str!("../assets/forge-core.js")),
        ("forge-ui.js", include_str!("../assets/forge-ui.js")),
        ("forge-views.js", include_str!("../assets/forge-views.js")),
    ] {
        for forbidden in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
            "new Function",
            "localStorage",
            "sessionStorage",
        ] {
            assert!(!source.contains(forbidden), "{name} uses {forbidden}");
        }
        assert!(
            !source.contains("setAttribute(\"style\"") && !source.contains("setAttribute('style'"),
            "{name}"
        );
    }
}

#[tokio::test]
async fn the_event_stream_authenticates_like_the_api_and_redacts_what_it_sends() {
    let (_home, core) = core();
    let e = admit(&core, "stream-run");
    let operator = core.policy.actors["operator"].token.clone();
    // A harness step with a retained ACP update log that contains the actor's token.
    let slot = "root/step:agent";
    core.store
        .write(|c| {
            c.execute(
                "INSERT INTO forge_flow_effects(run_id,slot,invocation_digest,action_digest,state,action) VALUES(?1,?2,'i','a','uncertain',?3)",
                rusqlite::params![e.dbos_id, slot, json!({"type":"harness","input":{"profile":"p"}}).to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    let dir = core.harness_dir(&Core::harness_invocation(&e.dbos_id, slot, 1));
    std::fs::create_dir_all(&dir).unwrap();
    let lines = [
        json!({"ms":1,"kind":"agent_message_chunk","body":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("secret {operator} text")}}}),
        json!({"ms":2,"kind":"permission","body":{"kind":"execute","decision":"denied","reason":"kind execute is denied by policy","locations":[]}}),
        json!({"ms":3,"kind":"tool_call","body":{"sessionUpdate":"tool_call","toolCallId":"t1","kind":"edit","status":"completed"}}),
    ];
    std::fs::write(
        dir.join("updates.jsonl"),
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let base = serve(core.clone()).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/api/stream");
    let body = json!({"run_id":e.dbos_id});
    // No credential, a wrong one, a non-Bearer scheme, a token in the URL, another
    // actor, and an unknown run are all refused before any event is produced.
    for request in [
        client.post(&url).json(&body),
        client.post(&url).bearer_auth("x".repeat(32)).json(&body),
        client
            .post(&url)
            .header("authorization", format!("Basic {operator}"))
            .json(&body),
        client.post(format!("{url}?token={operator}")).json(&body),
        client.post(&url).bearer_auth("s".repeat(32)).json(&body),
        client
            .post(&url)
            .bearer_auth(&operator)
            .json(&json!({"run_id":"no-such-run"})),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403);
        assert!(
            !response.headers()["content-type"]
                .to_str()
                .unwrap()
                .contains("event-stream")
        );
    }
    // The owner receives SSE: the state, the retained updates with stable ids, redacted.
    let mut response = client
        .post(&url)
        .bearer_auth(&operator)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let mut seen = String::new();
    while !seen.contains("event: tool_call") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), response.chunk())
            .await
            .expect("stream stalled")
            .unwrap()
            .expect("stream ended early");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(!seen.contains(&operator), "token leaked into the stream");
    assert!(seen.contains("event: state") && !seen.contains("RECOVERY_REQUIRED"));
    assert!(seen.contains("id: root/step:agent#0") && seen.contains("event: permission"));
    assert!(seen.contains("\"decision\":\"denied\"") && seen.contains("[redacted]"));
    // Unknown fields are refused: the stream takes only a run id.
    let bad = client
        .post(&url)
        .bearer_auth(&operator)
        .json(&json!({"run_id":e.dbos_id,"actor":"x"}))
        .send()
        .await
        .unwrap();
    assert!(bad.status().is_client_error());
    // The paginated log read serves the same redacted evidence with a cursor.
    let page = core
        .run_logs(
            "operator",
            &e.dbos_id,
            Some("harness:root/step:agent"),
            Some(1),
            Some(1),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            page["entries"][0]["type"].as_str(),
            page["next_after"].as_u64(),
            page["complete"].as_bool()
        ),
        (Some("permission"), Some(2), Some(false))
    );
    let index = core
        .run_logs("operator", &e.dbos_id, None, None, None)
        .await
        .unwrap();
    assert_eq!(index["sources"][1]["entries"], 3);
    assert!(
        core.run_logs("stranger", &e.dbos_id, None, None, None)
            .await
            .is_err()
    );
    assert!(
        core.run_logs("operator", &e.dbos_id, Some("harness:other"), None, None)
            .await
            .is_err()
    );
    assert!(!index.to_string().contains(&operator));
    let _: Value = index;
}

#[tokio::test]
async fn configuration_refusals_reach_the_client_as_bounded_messages_and_new_operations_are_closed()
{
    let (_home, core) = core();
    let operator = core.policy.actors["operator"].token.clone();
    let base = serve(core).await;
    let client = reqwest::Client::new();
    let call = |method: &str, params: Value| {
        client
            .post(format!("{base}/api"))
            .bearer_auth(&operator)
            .json(&json!({"method":method,"params":params}))
            .send()
    };
    let bad = call(
        "provider_set",
        json!({"kind":"model","name":"m","definition":{"base_url":"ftp://x/v1","model":"m"}}),
    )
    .await
    .unwrap();
    assert_eq!(bad.status(), 400);
    let body: Value = bad.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("model m refused"),
        "{body}"
    );
    let bad = call(
        "settings_set",
        json!({"key":"database.max_connections","value":1}),
    )
    .await
    .unwrap();
    assert_eq!(bad.status(), 400);
    assert!(
        bad.json::<Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("max_connections")
    );
    // Authority-forging and unknown parameters are refused for every new operation.
    for (method, params) in [
        (
            "approve",
            json!({"approval_id":"x","note":"n","actor":"root"}),
        ),
        ("run_logs", json!({"run_id":"x","path":"/etc/passwd"})),
        (
            "settings_set",
            json!({"key":"server.port","value":1,"home":"/"}),
        ),
        (
            "provider_remove",
            json!({"kind":"model","name":"x","force":true}),
        ),
        ("backup_start", json!({"destination":"/tmp/x"})),
    ] {
        let r = call(method, params).await.unwrap();
        assert_eq!(r.status(), 400, "{method}");
    }
    // Not logged in: every new operation requires the bearer credential.
    for method in [
        "overview",
        "agents",
        "approvals",
        "settings",
        "providers",
        "backups",
        "skills",
        "doctor",
        "runner_test",
        "definitions_export",
        "definitions_import",
        "service",
        "schedule_update",
        "schedules",
    ] {
        let r = client
            .post(format!("{base}/api"))
            .json(&json!({"method":method,"params":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{method}");
    }
    // Skills need no executor and are a real, read-only catalog.
    let skills: Value = call("skills", json!({}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(skills["result"]["skills"][0]["name"], "forge-operations");
}

fn enable_mcp(core: &Core) {
    let mut settings = crate::settings::Settings::for_tests(&core.policy);
    settings.server.mcp_http = true;
    core.settings.set(settings).unwrap();
}

fn mcp_request(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    params: Value,
) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-06-18")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
}

async fn exercise_mcp(client: &reqwest::Client, base: &str, core: &Core) {
    let initialize = || {
        mcp_request(
            client,
            base,
            "initialize",
            json!({
                "protocolVersion":"2025-06-18","capabilities":{},
                "clientInfo":{"name":"forge-test","version":"1"}
            }),
        )
    };
    let operator = &core.policy.actors["operator"].token;
    let reader = &core.policy.actors["stranger"].token;
    let unauthorized = initialize().send().await.unwrap();
    assert_eq!(unauthorized.status(), 401);
    assert_eq!(unauthorized.headers()["www-authenticate"], "Bearer");
    assert_eq!(
        initialize()
            .bearer_auth("invalid")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        initialize()
            .bearer_auth(reader)
            .header("origin", "https://example.invalid")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert!(
        !initialize()
            .bearer_auth(reader)
            .header("host", "example.invalid")
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let response = initialize().bearer_auth(reader).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("mcp-session-id").is_none());
    assert!(response.json::<Value>().await.unwrap()["result"]["capabilities"]["tools"].is_object());

    let list = mcp_request(client, base, "tools/list", json!({}))
        .bearer_auth(reader)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(
        |tool| tool["name"] == "forge_register" && tool["annotations"]["readOnlyHint"] == false
    ));

    let register = || {
        mcp_request(
            client,
            base,
            "tools/call",
            json!({
                "name":"forge_register",
                "arguments":{"work_id":"via-mcp","definition":{"type":"echo","input":{"text":"hello"}}}
            }),
        )
    };
    let denied = register()
        .bearer_auth(reader)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert!(core.work("operator", "via-mcp").is_err());
    let accepted = register()
        .bearer_auth(operator)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        accepted["result"]["isError"].as_bool().unwrap_or(false),
        false,
        "{accepted}"
    );
    assert!(core.work("operator", "via-mcp").is_ok());
}

#[tokio::test]
async fn streamable_mcp_is_configurable_and_authorizes_each_request() {
    let (_home, core) = core();
    let client = reqwest::Client::new();
    let base = serve(core.clone()).await;
    assert_eq!(
        mcp_request(&client, &base, "tools/list", json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    enable_mcp(&core);
    let base = serve(core.clone()).await;
    exercise_mcp(&client, &base, &core).await;
}

#[tokio::test]
async fn streamable_mcp_over_https_uses_the_configured_certificate() {
    use std::os::unix::fs::PermissionsExt;

    let (_home, core) = core();
    enable_mcp(&core);
    let tls = tempfile::tempdir().unwrap();
    let certificate = tls.path().join("cert.pem");
    let private_key = tls.path().join("key.pem");
    let output = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-noenc",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-keyout",
        ])
        .arg(&private_key)
        .arg("-out")
        .arg(&certificate)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::set_permissions(&private_key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let server_tls = crate::settings::TlsConfig {
        certificate: certificate.clone(),
        private_key,
    };
    let acceptor = crate::tls::acceptor(&server_tls).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("https://{}", listener.local_addr().unwrap());
    let app = crate::server::router(core.clone());
    tokio::spawn(async move {
        axum::serve(crate::tls::Listener::new(listener, acceptor), app)
            .await
            .unwrap();
    });
    let untrusted = reqwest::Client::new().get(&base).send().await;
    assert!(untrusted.is_err());
    let cert = reqwest::Certificate::from_pem(&std::fs::read(&certificate).unwrap()).unwrap();
    let client = reqwest::Client::builder()
        .tls_certs_merge([cert])
        .build()
        .unwrap();
    exercise_mcp(&client, &base, &core).await;
}
