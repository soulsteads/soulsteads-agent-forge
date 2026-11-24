//! Bounded authenticated HTTP client shared by CLI and MCP. Never opens state.
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use std::time::Duration;

pub const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 4 * MAX_REQUEST;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ClientError(pub String);
fn error(message: &str) -> ClientError {
    ClientError(message.to_owned())
}

/// A plugin CLI command's own exit status, propagated by the process without a message.
#[derive(Debug, thiserror::Error)]
#[error("plugin command exited with status {0}")]
pub struct ExitStatus(pub u8);

pub struct ApiClient {
    client: Client,
    endpoint: Url,
    token: String,
}
impl ApiClient {
    pub fn from_env(url: &str, token_env: &str) -> Result<Self, ClientError> {
        let token = std::env::var(token_env)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| error("Set the configured Forge token environment variable"))?;
        Self::new(url, token)
    }
    fn new(url: &str, token: String) -> Result<Self, ClientError> {
        let mut endpoint = Url::parse(url).map_err(|_| error("Invalid Forge service URL"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(error(
                "Forge URL must be HTTP(S) without credentials, query or fragment",
            ));
        }
        endpoint.set_path(&format!("{}/api", endpoint.path().trim_end_matches('/')));
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| error("Invalid Forge bearer token encoding"))?;
        authorization.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, authorization);
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| error("Cannot initialize Forge HTTP client"))?;
        Ok(Self {
            client,
            endpoint,
            token,
        })
    }
    pub async fn invoke(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let request = serde_json::to_vec(&json!({"method":method,"params":params}))
            .map_err(|_| error("Cannot encode Forge request"))?;
        if request.len() > MAX_REQUEST {
            return Err(error("Forge request exceeds 1 MiB limit"));
        }
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(request)
            .send()
            .await
            .map_err(|_| error(&uncertain(method, "Forge request could not be confirmed.")))?;
        if response
            .content_length()
            .is_some_and(|v| v > MAX_RESPONSE as u64)
        {
            return Err(error(
                "Forge response exceeds 4 MiB limit; inspect a specific run",
            ));
        }
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| error(&uncertain(method, "Forge response was interrupted.")))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(error(
                    "Forge response exceeds 4 MiB limit; inspect a specific run",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|_| {
            if method == "schedule_trigger" {
                error(&uncertain(method, "Forge returned an invalid response."))
            } else {
                error("Forge returned an invalid response. Inspect service health before retrying.")
            }
        })?;
        if !status.is_success() || body.get("ok") != Some(&Value::Bool(true)) {
            let message = body
                .get("error")
                .and_then(Value::as_str)
                .filter(|v| v.len() <= 4096 && !v.chars().any(char::is_control))
                .unwrap_or(
                    "Forge rejected the operation; inspect service health and your authority.",
                );
            return Err(ClientError(message.replace(&self.token, "[redacted]")));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| error("Forge response has no canonical result"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn exchange(
        status: &str,
        body: &str,
        extra: &str,
    ) -> (Result<Value, ClientError>, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
            body.len()
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 8192];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            // A bounded client deliberately closes an oversized response early.
            let _ = stream.write_all(reply.as_bytes()).await;
            String::from_utf8_lossy(&bytes).to_string()
        });
        let result = ApiClient::new(&format!("http://{address}"), "test-secret".into())
            .unwrap()
            .invoke("snapshot", json!({}))
            .await;
        (result, server.await.unwrap())
    }
    #[tokio::test]
    async fn authenticated_canonical_request_and_errors() {
        let (result, request) = exchange("200 OK", r#"{"ok":true,"result":{"work":[]}}"#, "").await;
        assert_eq!(result.unwrap(), json!({"work":[]}));
        assert!(request.starts_with("POST /api HTTP/1.1"));
        assert!(request.contains("authorization: Bearer test-secret"));
        assert!(request.contains(r#""method":"snapshot""#));
        let (result, _) = exchange(
            "403 Forbidden",
            r#"{"ok":false,"error":"denied test-secret"}"#,
            "",
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), "denied [redacted]");
        let (result, _) = exchange("200 OK", "private raw body", "").await;
        assert!(!result.unwrap_err().to_string().contains("private raw body"));
    }
    #[tokio::test]
    async fn redirects_are_not_followed_and_responses_bounded() {
        let (result, _) = exchange(
            "302 Found",
            r#"{"ok":false,"error":"redirect rejected"}"#,
            "Location: http://127.0.0.1:1/private\r\n",
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), "redirect rejected");
        let large = "x".repeat(MAX_RESPONSE + 1);
        let (result, _) = exchange("200 OK", &large, "").await;
        assert!(result.unwrap_err().to_string().contains("4 MiB"));
    }
    #[tokio::test]
    async fn oversized_request_is_refused_before_connecting() {
        let client = ApiClient::new("http://127.0.0.1:1", "secret".into()).unwrap();
        let result = client
            .invoke("register", json!({"input":"x".repeat(MAX_REQUEST)}))
            .await;
        assert!(result.unwrap_err().to_string().contains("1 MiB"));
    }
    #[test]
    fn invalid_configuration_is_refused() {
        for url in [
            "file:///private",
            &format!("{}://{}:{}@localhost", "http", "user", "password"),
            "http://localhost/?secret=x",
            "http://localhost/#token",
        ] {
            assert!(ApiClient::new(url, "secret".into()).is_err());
        }
        assert!(ApiClient::new("http://localhost", "a\nb".into()).is_err());
    }
}

/// Guidance after a transport failure, when the request's outcome is unknown.
/// Keyed operations are safe to repeat with the same key; a schedule trigger has
/// no request key, and every call starts another run.
fn uncertain(method: &str, what: &str) -> String {
    if method == "schedule_trigger" {
        format!(
            "{what} The manual schedule run may or may not have started; its outcome is unknown. It was not retried, and a schedule run has no request key: running it again starts another run. Check the schedule's history for a manual occurrence (soulsteads-forge schedules history NAME, or MCP forge_schedule_show) and the run list before deciding."
        )
    } else {
        format!("{what} Inspect state before retrying; keep the same request key.")
    }
}

#[cfg(test)]
mod uncertainty_tests {
    use super::*;
    #[test]
    fn trigger_transport_guidance_never_suggests_a_reusable_request_key() {
        let t = uncertain("schedule_trigger", "Forge request could not be confirmed.");
        assert!(!t.contains("same request key") && t.contains("no request key"));
        assert!(t.contains("may or may not have started") && t.contains("not retried"));
        assert!(t.contains("starts another run") && t.contains("schedules history"));
        let k = uncertain("run", "Forge request could not be confirmed.");
        assert!(k.ends_with("Inspect state before retrying; keep the same request key."));
    }
}
