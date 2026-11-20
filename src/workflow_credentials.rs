//! Systemd supplies credential values; Forge only binds aliases to a single
//! direct workflow step and an operator-selected, status-only HTTP GET.
use crate::{
    core::{Core, Invalid, Policy},
    dbos::admission::Envelope,
    safety, settings,
};
use anyhow::Result;
use reqwest::{Url, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub const KIND: &str = "credential_check";
pub(crate) const DIRECTORY_ENV: &str = "CREDENTIALS_DIRECTORY";
const ALIAS_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$";
const MAX_TOKEN: u64 = 4096;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub actor: String,
    pub work_id: String,
    pub step_id: String,
    pub url: String,
}

pub(crate) fn catalog() -> Value {
    json!({"description":"Workflow step only: perform an operator-pinned HTTP status check with a systemd credential alias. No response content is returned.","schema":{"type":"object","properties":{"credential":{"type":"string","maxLength":64,"pattern":ALIAS_PATTERN}},"required":["credential"],"additionalProperties":false}})
}

pub(crate) fn valid_alias(alias: &str) -> bool {
    alias.len() <= 64
        && alias
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

fn target(value: &str) -> Result<Url> {
    let safe_error = Invalid(
        "credential check target requires HTTPS or a literal loopback IP, with no user info, query or fragment",
    );
    let url = Url::parse(value).map_err(|_| Invalid(safe_error.0))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
    if url.host().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(safe_error.into());
    }
    Ok(url)
}

pub(crate) fn validate_policy(policy: &Policy) -> Result<()> {
    let grants = &policy.workflow_credentials;
    if grants.is_empty() {
        return Ok(());
    }
    if !policy.runners.is_empty() || !policy.plugins.is_empty() {
        return settings::refuse(
            "workflow credentials require a Forge home without executable plugins or native harness runners",
        );
    }
    if grants.len() > 64 {
        return settings::refuse("workflow_credentials supports at most 64 grants");
    }
    for (alias, grant) in grants {
        if !valid_alias(alias) || !policy.actors.contains_key(&grant.actor) {
            return settings::refuse(
                "workflow credential grant requires a valid alias and configured actor",
            );
        }
        crate::core::identifier(&grant.work_id)?;
        crate::core::identifier(&grant.step_id)?;
        if grant.work_id.starts_with("plugin.")
            || grant.step_id.len() > 64
            || grant.step_id.starts_with(['.', '-'])
        {
            return settings::refuse(
                "workflow credential grant requires a direct step of a non-plugin workflow",
            );
        }
        target(&grant.url)?;
    }
    Ok(())
}

pub(crate) fn alias(input: &Value) -> Result<&str> {
    input
        .get("credential")
        .and_then(Value::as_str)
        .filter(|s| valid_alias(s))
        .filter(|_| input.as_object().is_some_and(|o| o.len() == 1))
        .ok_or_else(|| {
            Invalid("credential check input must contain only a literal credential alias").into()
        })
}

fn directory_path(path: &Path, uid: u32) -> io::Result<PathBuf> {
    let path = path
        .to_str()
        .ok_or_else(|| io::Error::other("invalid credential directory"))?;
    let user_root = format!("/run/user/{uid}/credentials/");
    let unit = path
        .strip_prefix(&user_root)
        .or_else(|| path.strip_prefix("/run/credentials/"));
    if !unit.is_some_and(|unit| {
        unit.len() <= 255
            && unit.ends_with(".service")
            && unit
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-@\\:".contains(&b))
    }) {
        return Err(io::Error::other(
            "not a systemd service credential directory",
        ));
    }
    Ok(PathBuf::from(path))
}

fn open_directory(path: &Path, uid: u32) -> io::Result<File> {
    let mut fd = File::open("/")?;
    for component in path.components().skip(1) {
        let Component::Normal(name) = component else {
            return Err(io::Error::other("invalid credential directory"));
        };
        fd = safety::open_at(
            &fd,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?;
        let meta = fd.metadata()?;
        if (meta.uid() != 0 && meta.uid() != uid) || meta.mode() & 0o022 != 0 {
            return Err(io::Error::other("unsafe credential directory"));
        }
    }
    let meta = fd.metadata()?;
    if meta.uid() != uid || meta.mode() & 0o277 != 0 {
        return Err(io::Error::other(
            "credential directory is not private and read-only",
        ));
    }
    Ok(fd)
}

fn read_token(directory: &Path, alias: &str) -> io::Result<header::HeaderValue> {
    let uid = unsafe { libc::geteuid() };
    let name = OsStr::new(alias);
    let parent = open_directory(directory, uid)?;
    let file = safety::open_at(
        &parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
    )?;
    let before = file.metadata()?;
    if !before.is_file()
        || before.nlink() != 1
        || before.uid() != uid
        || before.mode() & 0o377 != 0
        || before.len() > MAX_TOKEN
    {
        return Err(io::Error::other(
            "not one bounded read-only credential file",
        ));
    }
    let mut value = b"Bearer ".to_vec();
    let n = (&file).take(MAX_TOKEN + 1).read_to_end(&mut value)?;
    let after = file.metadata()?;
    let rebound = open_directory(directory, uid)?;
    if n as u64 != before.len()
        || before.len() != after.len()
        || before.modified()? != after.modified()?
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || parent.metadata()?.ino() != rebound.metadata()?.ino()
        || parent.metadata()?.dev() != rebound.metadata()?.dev()
    {
        return Err(io::Error::other("credential changed while reading"));
    }
    safety::rebind_entry(&rebound, &file, name)?;
    if value.last() == Some(&b'\n') {
        value.pop();
    }
    if value.len() <= 7 || value[7..].iter().any(|b| !(b'!'..=b'~').contains(b)) {
        return Err(io::Error::other("invalid bearer credential"));
    }
    let mut result = header::HeaderValue::from_bytes(&value).map_err(io::Error::other)?;
    result.set_sensitive(true);
    Ok(result)
}

pub(crate) struct Check {
    url: Url,
    auth: header::HeaderValue,
}

impl Check {
    pub(crate) async fn send(self) -> Result<Value> {
        let failed = || Invalid("credential check did not receive a successful HTTP response");
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| failed())?;
        let response = client
            .get(self.url)
            .header(header::AUTHORIZATION, self.auth)
            .header(header::USER_AGENT, "agent-forge")
            .send()
            .await
            .map_err(|_| failed())?;
        let status = response.status();
        if !status.is_success() {
            return Err(failed().into());
        }
        // Do not read or retain any upstream headers or body (even on success).
        Ok(json!({"status":status.as_u16()}))
    }
}

impl Core {
    pub(crate) fn credential_grant(&self, actor: &str, definition: &Value) -> Result<&Grant> {
        let alias = alias(&definition["input"])?;
        self.policy
            .workflow_credentials
            .get(alias)
            .filter(|grant| grant.actor == actor)
            .ok_or_else(|| Invalid("workflow credential alias is not granted to this actor").into())
    }

    pub(crate) fn authorize_credential_step(
        &self,
        e: &Envelope,
        slot: &str,
        action: &Value,
    ) -> Result<&Grant> {
        let grant = self.credential_grant(e.invocation.principal(), action)?;
        let root = self.flow_current(e)?;
        let exact_step = root["input"]["steps"].as_array().is_some_and(|steps| {
            steps
                .iter()
                .any(|step| step["id"] == grant.step_id && &step["action"] == action)
        });
        if e.invocation.workspace() != grant.work_id
            || slot != format!("root/step:{}", grant.step_id)
            || root["type"] != "workflow"
            || !exact_step
        {
            return Err(Invalid("workflow credential is not granted to this workflow step").into());
        }
        Ok(grant)
    }

    pub(crate) fn prepare_credential_check(
        &self,
        e: &Envelope,
        slot: &str,
        action: &Value,
    ) -> Result<Check> {
        let grant = self.authorize_credential_step(e, slot, action)?;
        let name = alias(&action["input"])?;
        let unavailable = || {
            Invalid(
                "workflow credential unavailable; load the alias with systemd and restart Forge",
            )
        };
        let directory = std::env::var_os(DIRECTORY_ENV).ok_or_else(unavailable)?;
        let directory = directory_path(Path::new(&directory), unsafe { libc::geteuid() })
            .map_err(|_| unavailable())?;
        // Forge copies the home when making a backup; never consume a credential
        // from inside it (filesystem namespace verification happens at open).
        if directory.starts_with(&self.paths.home) {
            return Err(unavailable().into());
        }
        let url = target(&grant.url)?;
        let auth = read_token(&directory, name).map_err(|_| unavailable())?;
        Ok(Check { url, auth })
    }
}
