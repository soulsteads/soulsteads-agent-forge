use super::{Error, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    pub home_id: String,
    pub application: String,
    pub schema: String,
    pub app_version: String,
    pub executor_id: String,
    pub(crate) database_url: String,
    pub work_concurrency: i32,
    pub model_concurrency: i32,
    pub server_concurrency: i32,
    pub max_connections: u32,
    pub connect_timeout_seconds: u64,
}
impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("home_id", &self.home_id)
            .field("application", &self.application)
            .field("schema", &self.schema)
            .field("app_version", &self.app_version)
            .field("executor_id", &self.executor_id)
            .field("database_url", &"[redacted]")
            .finish()
    }
}
impl RuntimeConfig {
    pub fn from_json(raw: &str) -> Result<Self> {
        let config: Self = serde_json::from_str(raw)
            .map_err(|_| Error::Config("invalid explicit DBOS configuration"))?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        let name = |v: &str, min, max| {
            (min..=max).contains(&v.len())
                && v.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        };
        if self.home_id.len() != 32
            || !self
                .home_id
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        {
            return Err(Error::Config(
                "home_id must be explicit lowercase 128-bit hex",
            ));
        }
        if !name(&self.application, 3, 256)
            || !name(&self.schema, 1, 63)
            || self.schema.as_bytes()[0].is_ascii_digit()
            || self.schema.contains('-')
        {
            return Err(Error::Config(
                "invalid explicit application or PostgreSQL schema identity",
            ));
        }
        if !self.application.ends_with(&format!("-{}", self.home_id))
            || self.executor_id != format!("forge-v1-{}", self.home_id)
        {
            return Err(Error::Config(
                "application suffix and stable executor_id must bind home_id",
            ));
        }
        if self.app_version.is_empty()
            || self.app_version.len() > 128
            || self.app_version.chars().any(char::is_control)
            || self.executor_id.is_empty()
            || self.executor_id.len() > 128
            || self.executor_id.chars().any(char::is_control)
        {
            return Err(Error::Config(
                "app_version and executor_id must be explicit bounded identities",
            ));
        }
        let url = reqwest::Url::parse(&self.database_url)
            .map_err(|_| Error::Config("an explicit PostgreSQL database URL is required"))?;
        if !["postgres", "postgresql"].contains(&url.scheme())
            || url.path().trim_matches('/').is_empty()
            || url.fragment().is_some()
        {
            return Err(Error::Config(
                "an explicit PostgreSQL database URL is required",
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        if url
            .query_pairs()
            .any(|(key, _)| !seen.insert(key.into_owned()))
        {
            return Err(Error::Config("duplicate PostgreSQL connection option"));
        }
        let pairs: std::collections::BTreeMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let allowed = [
            "host",
            "port",
            "dbname",
            "user",
            "password",
            "application_name",
            "sslmode",
            "ssl-mode",
            "sslrootcert",
            "ssl-root-cert",
            "ssl-ca",
            "sslcert",
            "ssl-cert",
            "sslkey",
            "ssl-key",
            "statement-cache-capacity",
            "options",
        ];
        if pairs.keys().any(|key| {
            !allowed.contains(&key.as_str()) && !(key.starts_with("options[") && key.ends_with(']'))
        }) {
            return Err(Error::Config(
                "unsupported or ambiguous PostgreSQL connection option",
            ));
        }
        let host = pairs
            .get("host")
            .map(String::as_str)
            .or_else(|| url.host_str());
        let user = pairs
            .get("user")
            .map(String::as_str)
            .unwrap_or(url.username());
        let port = if let Some(port) = pairs.get("port") {
            port.parse::<u16>().ok()
        } else {
            url.port()
        };
        if host.is_none_or(str::is_empty)
            || user.is_empty()
            || port.is_none_or(|p| p == 0)
            || pairs.get("dbname").is_some_and(String::is_empty)
        {
            return Err(Error::Config(
                "PostgreSQL target host/socket, port, user and database must be explicit",
            ));
        }
        if self.work_concurrency < 1
            || self.model_concurrency < 1
            || self.server_concurrency < 1
            || self.server_concurrency > self.model_concurrency
            || self.max_connections < 2
            || self.max_connections > 100
            || !(1..=300).contains(&self.connect_timeout_seconds)
        {
            return Err(Error::Config(
                "invalid bounded queue or connection capacity",
            ));
        }
        Ok(())
    }
    pub fn work_queue(&self) -> String {
        format!("forge-v1-{}-work", self.home_id)
    }
    pub fn model_queue(&self) -> String {
        format!("forge-v1-{}-models", self.home_id)
    }
    pub(crate) fn sdk(&self) -> ::dbos::Config {
        ::dbos::Config {
            schema: self.schema.clone(),
            app_version: Some(self.app_version.clone()),
            executor_id: Some(self.executor_id.clone()),
            max_connections: self.max_connections,
            migrate: false,
            listen_queues: Some(vec![self.work_queue(), self.model_queue()]),
            ..::dbos::Config::new(&self.application, &self.database_url)
        }
    }
    pub(crate) fn database_binding(&self) -> String {
        let mut url = reqwest::Url::parse(&self.database_url).expect("validated PostgreSQL URL");
        if url.password().is_some() {
            url.set_password(None)
                .expect("credential-bearing hierarchical URL");
        }
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| {
                !matches!(
                    key.to_ascii_lowercase().as_str(),
                    "password" | "passfile" | "sslpassword"
                )
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
        format!("sha256:{:x}", Sha256::digest(url.as_str().as_bytes()))
    }
    pub(crate) fn client(&self) -> ::dbos::ClientConfig {
        ::dbos::ClientConfig {
            app_name: Some(self.application.clone()),
            schema: self.schema.clone(),
            max_connections: self.max_connections,
            ..::dbos::ClientConfig::new(&self.database_url)
        }
    }
    pub async fn connect_client(&self) -> Result<::dbos::Client> {
        self.validate()?;
        Ok(::dbos::Client::connect(self.client()).await?)
    }
}
