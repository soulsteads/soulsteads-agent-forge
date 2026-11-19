//! Versioned domain admission and same-ID submission metadata, never execution state.
use super::RuntimeConfig;
use crate::settings::Paths;
use ::dbos::sysdb::types::{QualifiedWorkflowName, WorkflowFilter, WorkflowRecord, WorkflowStatus};
use ::dbos::{Client, EnqueueOptions, SerializationFormat, WorkflowIdReusePolicy};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub(crate) const APPLICATION_ID: i32 = 0x46444231;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("DBOS admission refused: {0}")]
    Invalid(&'static str),
    #[error("DBOS admission SQLite failure: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("DBOS admission filesystem failure: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sdk(Box<::dbos::Error>),
    #[error(
        "DBOS admission commit outcome is unknown; inspect the same identity before retrying: {0}"
    )]
    Commit(#[source] rusqlite::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
// The SDK error types are large; they are boxed so every `Result` stays small.
impl From<::dbos::Error> for Error {
    fn from(error: ::dbos::Error) -> Self {
        Self::Sdk(Box::new(error))
    }
}

fn invalid(reason: &'static str) -> Error {
    Error::Invalid(reason)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn home_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopedReference {
    pub scope: String,
    pub object_id: String,
    pub sha256: String,
}
impl ScopedReference {
    pub fn validate(&self, scope: &str) -> Result<()> {
        if self.scope != scope
            || self.object_id.is_empty()
            || self.object_id.len() > 128
            || !self
                .object_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
            || !digest(&self.sha256)
        {
            return Err(invalid("invalid scoped immutable reference"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    format_version: u8,
    home_id: String,
    scope: String,
    workflow_name: String,
    request_id: String,
    workflow_version: String,
    principal: String,
    workspace: String,
    plugin: String,
    resource: String,
    policy_ref: String,
    definition_sha256: String,
    action_sha256: String,
    parameters: BTreeMap<String, ScopedReference>,
    evidence: Vec<ScopedReference>,
    authorization_expires_at_ms: Option<i64>,
}
impl Invocation {
    pub fn from_json(raw: &str) -> Result<Self> {
        let value: Self =
            serde_json::from_str(raw).map_err(|_| invalid("invalid closed invocation envelope"))?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.format_version != 1
            || !home_id(&self.home_id)
            || ![
                &self.scope,
                &self.workflow_name,
                &self.request_id,
                &self.workflow_version,
                &self.principal,
                &self.workspace,
                &self.plugin,
                &self.resource,
                &self.policy_ref,
            ]
            .into_iter()
            .all(|s| bounded(s))
            || !digest(&self.definition_sha256)
            || !digest(&self.action_sha256)
        {
            return Err(invalid("invalid versioned invocation identity"));
        }
        if self.parameters.len() > 64
            || self.evidence.len() > 64
            || self.evidence.windows(2).any(|v| v[0] >= v[1])
        {
            return Err(invalid(
                "references must be bounded and evidence sorted/unique",
            ));
        }
        for (key, reference) in &self.parameters {
            if !bounded(key) {
                return Err(invalid("invalid parameter reference key"));
            }
            reference.validate(&self.scope)?;
        }
        for reference in &self.evidence {
            reference.validate(&self.scope)?;
        }
        Ok(())
    }
    pub fn home_id(&self) -> &str {
        &self.home_id
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn workflow_name(&self) -> &str {
        &self.workflow_name
    }
    pub fn workflow_version(&self) -> &str {
        &self.workflow_version
    }
    pub fn principal(&self) -> &str {
        &self.principal
    }
    pub fn resource(&self) -> &str {
        &self.resource
    }
    pub fn policy_ref(&self) -> &str {
        &self.policy_ref
    }
    pub fn action_sha256(&self) -> &str {
        &self.action_sha256
    }
    pub fn authorization_expires_at_ms(&self) -> Option<i64> {
        self.authorization_expires_at_ms
    }
    pub fn source_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("closed metadata types")
    }
    pub fn dbos_id(&self) -> String {
        Self::derive_dbos_id(
            &self.home_id,
            &self.scope,
            &self.workflow_name,
            &self.request_id,
        )
    }
    /// The canonical identity an invocation with these identity fields has. Lets
    /// readers locate an existing admission without constructing or admitting one.
    pub fn derive_dbos_id(
        home_id: &str,
        scope: &str,
        workflow_name: &str,
        request_id: &str,
    ) -> String {
        let tuple = crate::canonical::to_vec(&serde_json::json!([
            home_id,
            scope,
            workflow_name,
            request_id
        ]))
        .expect("string tuple");
        format!("forge-v1-{}", hash(&tuple))
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn workspace(&self) -> &str {
        &self.workspace
    }
    pub fn sha256(&self) -> String {
        hash(&serde_json::to_vec(self).expect("closed metadata types"))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub format_version: u8,
    pub dbos_id: String,
    pub invocation_sha256: String,
    pub invocation: Invocation,
}
impl Envelope {
    pub fn new(invocation: Invocation) -> Self {
        Self {
            format_version: 1,
            dbos_id: invocation.dbos_id(),
            invocation_sha256: invocation.sha256(),
            invocation,
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.invocation.validate()?;
        if self.format_version != 1
            || self.dbos_id != self.invocation.dbos_id()
            || self.invocation_sha256 != self.invocation.sha256()
        {
            return Err(invalid("invocation mapping or digest conflict"));
        }
        Ok(())
    }
    pub fn json(&self) -> String {
        serde_json::to_string(self).expect("closed metadata types")
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionState {
    Admitted,
    SubmissionUnknown,
    Submitted,
    Projected,
}
impl SubmissionState {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "admitted" => Ok(Self::Admitted),
            "submission_unknown" => Ok(Self::SubmissionUnknown),
            "submitted" => Ok(Self::Submitted),
            "projected" => Ok(Self::Projected),
            _ => Err(invalid("invalid stored submission state")),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AdmissionRecord {
    pub owner: &'static str,
    pub envelope: Envelope,
    pub state: SubmissionState,
    pub revision: i64,
    pub runtime_status: Option<String>,
}
pub struct SubmissionAttempt {
    dbos_id: String,
    digest: String,
    revision: i64,
}
pub struct AdmissionStore {
    home_id: String,
    path: PathBuf,
    connection: Mutex<Connection>,
}
impl AdmissionStore {
    pub fn initialize(paths: &Paths, id: &str) -> Result<Self> {
        Self::open(paths, id, true)
    }
    pub fn read_only(paths: &Paths, id: &str) -> Result<Self> {
        Self::open(paths, id, false)
    }
    fn open(paths: &Paths, id: &str, write: bool) -> Result<Self> {
        let home = &paths.home;
        if !home_id(id) || !home.is_absolute() {
            return Err(invalid("explicit absolute home and home ID required"));
        }
        let path = paths.dbos_admission.clone();
        super::private_directory(home).map_err(|_| invalid("private home required"))?;
        if write {
            // The state directory is created by the layout the service resolves (serve),
            // and here for direct store users: `init` no longer pre-creates it.
            crate::safety::private_mkdir(&paths.state)
                .map_err(|_| invalid("private state required"))?;
        }
        super::private_directory(&paths.state).map_err(|_| invalid("private state required"))?;
        super::private_directory(
            path.parent()
                .ok_or_else(|| invalid("private state required"))?,
        )
        .map_err(|_| invalid("private state required"))?;
        let fresh = if write {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(file) => {
                    file.sync_all()?;
                    true
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(e) => return Err(e.into()),
            }
        } else {
            false
        };
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(invalid("one private owned admission database required"));
        }
        let mut connection = Connection::open_with_flags(
            &path,
            if write {
                OpenFlags::SQLITE_OPEN_READ_WRITE
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY
            },
        )?;
        let after = std::fs::symlink_metadata(&path)?;
        if (after.dev(), after.ino()) != (metadata.dev(), metadata.ino()) || after.nlink() != 1 {
            return Err(invalid("admission database changed during open"));
        }
        connection.busy_timeout(Duration::from_secs(5))?;
        if fresh {
            connection.pragma_update(None, "journal_mode", "DELETE")?;
            connection.pragma_update(None, "synchronous", "FULL")?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
            transaction.execute_batch(include_str!("admission.sql"))?;
            transaction.execute("INSERT INTO dbos_domain_meta VALUES('home_id',?1)", [id])?;
            transaction.execute(
                "INSERT INTO dbos_domain_meta VALUES('bridge_check_seq','0')",
                [],
            )?;
            transaction.commit().map_err(Error::Commit)?;
            std::fs::File::open(&paths.state)?.sync_all()?;
        }
        let app: i32 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
        if app != APPLICATION_ID {
            return Err(invalid("not a versioned DBOS admission database"));
        }
        let mode: String = connection.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(invalid("DBOS admission requires journal_mode=DELETE"));
        }
        let stored: String = connection.query_row(
            "SELECT value FROM dbos_domain_meta WHERE key='home_id'",
            [],
            |r| r.get(0),
        )?;
        if stored != id {
            return Err(invalid("admission database belongs to another home"));
        }
        if write {
            connection.pragma_update(None, "synchronous", "FULL")?;
        }
        Ok(Self {
            home_id: id.to_owned(),
            path,
            connection: Mutex::new(connection),
        })
    }
    /// The store's single connection. Waiting for it can take minutes while an online
    /// backup holds it for a consistent cut, so on a multi-thread runtime the wait runs
    /// under `block_in_place`: the runtime moves other tasks to another worker instead of
    /// stalling every request (and the backup's own database dump) behind this one.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        let wait = || {
            self.connection
                .lock()
                .map_err(|_| invalid("admission mutex poisoned"))
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait)
            }
            _ => wait(),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn write<T>(&self, action: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut connection = self.lock()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = action(&tx)?;
        tx.commit().map_err(Error::Commit)?;
        Ok(value)
    }
    pub fn admit(&self, invocation: &Invocation) -> Result<AdmissionRecord> {
        invocation.validate()?;
        if invocation.home_id() != self.home_id {
            return Err(invalid("cross-home admission refused"));
        }
        let envelope = Envelope::new(invocation.clone());
        let bytes = envelope.json();
        self.write(|connection|{
            if let Some(existing)=read(connection,&envelope.dbos_id)?{if existing.envelope!=envelope{return Err(invalid("same ID has a different immutable invocation"))}return Ok(existing)}
            connection.execute("INSERT INTO dbos_invocations(dbos_id,envelope,invocation_digest,state,revision,last_checked_ms) VALUES(?1,?2,?3,'admitted',0,0)",params![envelope.dbos_id,bytes,envelope.invocation_sha256])?;
            read(connection,&envelope.dbos_id)?.ok_or_else(||invalid("admitted invocation disappeared"))
        })
    }
    pub fn inspect(&self, id: &str) -> Result<Option<AdmissionRecord>> {
        let connection = self.lock()?;
        read(&connection, id)
    }
    pub fn verify(&self, envelope: &Envelope) -> Result<()> {
        self.verify_home(envelope)?;
        let connection = self.lock()?;
        Self::verify_admission(&connection, envelope)
    }

    fn verify_home(&self, envelope: &Envelope) -> Result<()> {
        envelope.validate()?;
        if envelope.invocation.home_id() != self.home_id {
            return Err(invalid("cross-home invocation verification refused"));
        }
        Ok(())
    }

    fn verify_admission(connection: &Connection, envelope: &Envelope) -> Result<()> {
        let entry = read(connection, &envelope.dbos_id)?
            .ok_or_else(|| invalid("invocation lacks canonical Forge admission"))?;
        if &entry.envelope != envelope {
            return Err(invalid("invocation differs from canonical Forge admission"));
        }
        Ok(())
    }

    /// Read a verified invocation without waiting for a consistency cut.
    /// Busy is distinct from an absent fence and from an authority error.
    pub(crate) fn try_verified_read<T>(
        &self,
        envelope: &Envelope,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<Option<T>> {
        self.verify_home(envelope)?;
        let connection = match self.connection.try_lock() {
            Ok(connection) => connection,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(invalid("admission mutex poisoned"));
            }
        };
        Self::verify_admission(&connection, envelope)?;
        Ok(Some(read(&connection)?))
    }
    pub fn begin_submission(&self, id: &str) -> Result<SubmissionAttempt> {
        self.write(|connection|{
            let entry=read(connection,id)?.ok_or_else(||invalid("admitted identity missing"))?;
            if !matches!(entry.state,SubmissionState::Admitted|SubmissionState::SubmissionUnknown){return Err(invalid("submission state is not sendable"))}
            connection.execute("UPDATE dbos_invocations SET state='submission_unknown',revision=revision+1 WHERE dbos_id=?1 AND revision=?2",params![id,entry.revision])?;
            Ok(SubmissionAttempt{dbos_id:id.to_owned(),digest:entry.envelope.invocation_sha256,revision:entry.revision+1})
        })
    }
    pub fn mark_submitted(&self, attempt: &SubmissionAttempt) -> Result<bool> {
        self.write(|connection|Ok(connection.execute("UPDATE dbos_invocations SET state='submitted',revision=revision+1 WHERE dbos_id=?1 AND invocation_digest=?2 AND revision=?3 AND state='submission_unknown'",params![attempt.dbos_id,attempt.digest,attempt.revision])?==1))
    }
    pub fn project(&self, id: &str, status: &str) -> Result<()> {
        if ![
            "SUCCESS",
            "ERROR",
            "CANCELLED",
            "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
        ]
        .contains(&status)
        {
            return Err(invalid(
                "only observed native terminal statuses can project",
            ));
        }
        self.write(|connection|{
            let entry=read(connection,id)?.ok_or_else(||invalid("projection identity missing"))?;
            if entry.state==SubmissionState::Projected{if entry.runtime_status.as_deref()!=Some(status){return Err(invalid("conflicting terminal projection"))}return Ok(())}
            if entry.state!=SubmissionState::Submitted{return Err(invalid("submission is not confirmed"))}
            connection.execute("UPDATE dbos_invocations SET state='projected',runtime_status=?2,revision=revision+1 WHERE dbos_id=?1 AND state='submitted'",params![id,status])?;Ok(())
        })
    }
    fn batch(&self, limit: u32) -> Result<Vec<AdmissionRecord>> {
        if !(1..=64).contains(&limit) {
            return Err(invalid("bridge batch must be 1..64"));
        }
        let connection = self.lock()?;
        let ids=connection.prepare("SELECT dbos_id FROM dbos_invocations WHERE state!='projected' ORDER BY last_checked_ms,rowid LIMIT ?1")?.query_map([limit],|row|row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
        ids.iter()
            .map(|id| read(&connection, id)?.ok_or_else(|| invalid("batch identity missing")))
            .collect()
    }
    fn checked(&self, id: &str) -> Result<()> {
        self.write(|connection|{
            connection.execute("UPDATE dbos_domain_meta SET value=CAST(value AS INTEGER)+1 WHERE key='bridge_check_seq'",[])?;
            connection.execute("UPDATE dbos_invocations SET last_checked_ms=(SELECT CAST(value AS INTEGER) FROM dbos_domain_meta WHERE key='bridge_check_seq') WHERE dbos_id=?1",[id])?;
            Ok(())
        })
    }
    /// Admissions the store records as confirmed-submitted and not yet terminal.
    pub fn submitted_ids(&self) -> Result<Vec<String>> {
        let connection = self.lock()?;
        Ok(connection
            .prepare("SELECT dbos_id FROM dbos_invocations WHERE state='submitted'")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }
    pub fn remaining(&self) -> Result<u64> {
        let connection = self.lock()?;
        Ok(connection.query_row(
            "SELECT count(*) FROM dbos_invocations WHERE state!='projected'",
            [],
            |r| r.get(0),
        )?)
    }
}
fn read(connection: &Connection, id: &str) -> Result<Option<AdmissionRecord>> {
    let row: Option<(String, String, i64, Option<String>)> = connection
        .query_row(
            "SELECT envelope,state,revision,runtime_status FROM dbos_invocations WHERE dbos_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    row.map(|(data, state, revision, runtime_status)| {
        let envelope: Envelope =
            serde_json::from_str(&data).map_err(|_| invalid("invalid stored invocation"))?;
        envelope.validate()?;
        if envelope.dbos_id != id {
            return Err(invalid("stored identity mapping mismatch"));
        }
        Ok(AdmissionRecord {
            owner: "dbos",
            envelope,
            state: SubmissionState::parse(&state)?,
            revision,
            runtime_status,
        })
    })
    .transpose()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BridgeProgress {
    pub processed: u32,
    pub submitted: u32,
    pub projected: u32,
    pub remaining: u64,
    pub busy: bool,
    pub refused: u32,
}
pub struct SubmissionBridge {
    pub store: std::sync::Arc<AdmissionStore>,
    config: RuntimeConfig,
    gate: tokio::sync::Mutex<()>,
    allowed: std::collections::BTreeSet<String>,
}
impl SubmissionBridge {
    pub async fn verify_existing(&self, envelope: &Envelope) -> Result<()> {
        self.store.verify(envelope)?;
        let client = self
            .config
            .connect_client()
            .await
            .map_err(|error| match error {
                super::Error::Sdk(error) => Error::Sdk(error),
                _ => invalid("native client setup failed"),
            })?;
        let result = self.lookup(&client, envelope).await;
        client.close().await;
        if result?.is_none() {
            return Err(invalid("native invocation missing during startup"));
        }
        Ok(())
    }
    pub fn new(
        store: std::sync::Arc<AdmissionStore>,
        config: RuntimeConfig,
        allowed: std::collections::BTreeSet<String>,
    ) -> Result<Self> {
        config
            .validate()
            .map_err(|_| invalid("invalid explicit SDK configuration"))?;
        if store.home_id != config.home_id {
            return Err(invalid("bridge and configured home differ"));
        }
        Ok(Self {
            store,
            config,
            gate: tokio::sync::Mutex::new(()),
            allowed,
        })
    }
    /// Serialize one caller's admission-to-native handoff and read back its exact
    /// identity before acknowledging dispatch or applying cancellation.
    pub async fn submit(&self, envelope: &Envelope) -> Result<()> {
        let _guard = self.gate.lock().await;
        self.store.verify(envelope)?;
        let record = self
            .store
            .inspect(&envelope.dbos_id)?
            .ok_or_else(|| invalid("admission missing"))?;
        let client = self.config.connect_client().await.map_err(|e| match e {
            super::Error::Sdk(e) => Error::Sdk(e),
            _ => invalid("native client setup failed"),
        })?;
        let result = self.reconcile_record(&client, &record).await;
        client.close().await;
        result.map(|_| ())
    }
    pub async fn reconcile(&self, limit: u32) -> Result<BridgeProgress> {
        let Ok(_guard) = self.gate.try_lock() else {
            return Ok(BridgeProgress {
                remaining: self.store.remaining()?,
                busy: true,
                ..Default::default()
            });
        };
        let client = self
            .config
            .connect_client()
            .await
            .map_err(|error| match error {
                super::Error::Sdk(e) => Error::Sdk(e),
                _ => invalid("native client setup failed"),
            })?;
        let result = self.pass(&client, limit).await;
        client.close().await;
        result
    }
    async fn pass(&self, client: &Client, limit: u32) -> Result<BridgeProgress> {
        let mut progress = BridgeProgress::default();
        for record in self.store.batch(limit)? {
            match self.reconcile_record(client, &record).await {
                Ok((submitted, projected)) => {
                    progress.submitted += u32::from(submitted);
                    progress.projected += u32::from(projected);
                }
                Err(Error::Invalid(_)) => {
                    progress.refused += 1;
                }
                Err(error) => return Err(error),
            }
            self.store.checked(&record.envelope.dbos_id)?;
            progress.processed += 1;
        }
        progress.remaining = self.store.remaining()?;
        Ok(progress)
    }

    async fn reconcile_record(
        &self,
        client: &Client,
        record: &AdmissionRecord,
    ) -> Result<(bool, bool)> {
        let envelope = &record.envelope;
        if envelope.invocation.workflow_version() != self.config.app_version
            || !self.allowed.contains(envelope.invocation.workflow_name())
        {
            return Err(invalid("invocation release or registered target differs"));
        }
        let attempt = if matches!(
            record.state,
            SubmissionState::Admitted | SubmissionState::SubmissionUnknown
        ) {
            Some(self.store.begin_submission(&envelope.dbos_id)?)
        } else {
            None
        };
        let mut observed = self.lookup(client, envelope).await?;
        if observed.is_none() {
            if attempt.is_none() {
                return Err(invalid(
                    "previously confirmed native submission disappeared",
                ));
            }
            let queue = self.config.work_queue();
            let options = EnqueueOptions {
                workflow_id: Some(&envelope.dbos_id),
                workflow_id_reuse_policy: WorkflowIdReusePolicy::Reject,
                serialization: SerializationFormat::Native,
                app_name: Some(&self.config.application),
                app_version: Some(&self.config.app_version),
                ..EnqueueOptions::new(&queue)
            };
            let sent: ::dbos::Result<::dbos::WorkflowHandle<serde_json::Value>> = client
                .enqueue_with(envelope.invocation.workflow_name(), envelope, options)
                .await;
            match sent {
                Ok(_) => {}
                Err(::dbos::Error::SystemDatabase(::dbos::sysdb::Error::WorkflowIdInUse {
                    ..
                })) => {}
                Err(error) => return Err(error.into()),
            }
            observed = self.lookup(client, envelope).await?;
            if observed.is_none() {
                return Err(invalid(
                    "native submission acknowledged without readable identity",
                ));
            }
        }
        let submitted = if let Some(attempt) = attempt {
            if !self.store.mark_submitted(&attempt)? {
                return Ok((false, false));
            }
            true
        } else {
            false
        };
        let observed = observed.unwrap();
        let projected = matches!(
            observed.status,
            WorkflowStatus::Success
                | WorkflowStatus::Error
                | WorkflowStatus::Cancelled
                | WorkflowStatus::MaxRecoveryAttemptsExceeded
        );
        if projected {
            self.store
                .project(&envelope.dbos_id, observed.status.as_str())?
        }
        Ok((submitted, projected))
    }
    async fn lookup(&self, client: &Client, envelope: &Envelope) -> Result<Option<WorkflowRecord>> {
        let rows = client
            .list_workflows(&WorkflowFilter {
                workflow_ids: vec![&envelope.dbos_id],
                load_input: true,
                load_output: false,
                ..Default::default()
            })
            .await?;
        if rows.len() > 1 {
            return Err(invalid("native ID did not resolve uniquely"));
        }
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        if row.workflow_id != envelope.dbos_id
            || row.qualified_name()
                != Some(QualifiedWorkflowName::new(
                    envelope.invocation.workflow_name(),
                ))
            || row.app_name.as_deref() != Some(&self.config.application)
            || row.app_version.as_deref() != Some(&self.config.app_version)
            || row.serialization.as_deref() != Some("rust_serde")
            || row.input.as_deref() != Some(envelope.json().as_str())
            || (row.queue_name.as_deref() != Some(self.config.work_queue().as_str())
                && !(row.queue_name.is_none()
                    && matches!(
                        row.status,
                        WorkflowStatus::Cancelled | WorkflowStatus::MaxRecoveryAttemptsExceeded
                    )))
            || row.queue_partition_key.is_some()
            || row.parent_workflow_id.is_some()
            || row.forked_from.is_some()
        {
            return Err(invalid(
                "native same-ID row conflicts with immutable invocation binding",
            ));
        }
        Ok(Some(row))
    }
}
