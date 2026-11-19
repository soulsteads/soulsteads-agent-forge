//! Native DBOS executor substrate. No legacy controller or fallback executes here.
pub mod admission;
mod config;
pub mod effects;
mod maintenance;
use crate::settings::Paths;
use ::dbos::sysdb::{INTERNAL_QUEUE, SystemDatabase, postgres};
use ::dbos::{Client, DBOS, Queue, QueueConflict, QueueOptions};
pub use config::RuntimeConfig;
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("DBOS configuration refused: {0}")]
    Config(&'static str),
    #[error("DBOS database bootstrap timed out before executor launch")]
    BootstrapTimeout,
    #[error("DBOS home owner or identity refused: {0}")]
    Home(&'static str),
    #[error("DBOS home filesystem failure: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sdk(Box<::dbos::Error>),
    #[error("DBOS database bootstrap failed: {0}")]
    Database(Box<::dbos::sysdb::Error>),
    #[error("DBOS submission metadata boundary refused startup: {0}")]
    Admission(#[from] admission::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
// The SDK error types are large; they are boxed so every `Result` stays small.
impl From<::dbos::Error> for Error {
    fn from(error: ::dbos::Error) -> Self {
        Self::Sdk(Box::new(error))
    }
}
impl From<::dbos::sysdb::Error> for Error {
    fn from(error: ::dbos::sysdb::Error) -> Self {
        Self::Database(Box::new(error))
    }
}

struct HomeOwner {
    _marker: crate::essentials::PidMarker,
}
fn private_directory(path: &Path) -> Result<File> {
    let directory = crate::configstore_files::directory(path)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(Error::Home(
            "home and state must be private effective-user-owned directories",
        ));
    }
    Ok(directory)
}
fn seal_identity(paths: &Paths, config: &RuntimeConfig) -> Result<()> {
    let home = &paths.home;
    let path = paths.dbos_identity.clone();
    let identity_directory = path
        .parent()
        .ok_or(Error::Home("identity file requires a parent directory"))?
        .to_path_buf();
    let origin = home.canonicalize()?;
    let origin = origin
        .to_str()
        .ok_or(Error::Home("home origin must be UTF-8"))?;
    let value = serde_json::json!({"format_version":1,"home_id":config.home_id,"home_origin":origin,"database_binding":config.database_binding(),"application":config.application,"schema":config.schema,"app_version":config.app_version,"executor_id":config.executor_id});
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(
                (crate::canonical::to_string(&value).expect("identity is JSON") + "\n").as_bytes(),
            )?;
            file.sync_all()?;
            File::open(&identity_directory)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)?;
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.nlink() != 1
                || meta.mode() & 0o077 != 0
                || meta.uid() != unsafe { libc::geteuid() }
            {
                return Err(Error::Home(
                    "identity is not one private owned regular file",
                ));
            }
            let mut bytes = String::new();
            file.read_to_string(&mut bytes)?;
            if serde_json::from_str::<serde_json::Value>(&bytes)
                .ok()
                .as_ref()
                != Some(&value)
            {
                return Err(Error::Home(
                    "explicit identity differs from the sealed home; controlled upgrade/rebind required",
                ));
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub struct Runtime {
    config: RuntimeConfig,
    owner: Arc<HomeOwner>,
    engine: DBOS,
    launched: bool,
    registrations: Mutex<std::collections::BTreeSet<String>>,
    bridge: Option<maintenance::InstalledBridge>,
    pub application: Option<Arc<crate::core::Core>>,
}
impl Runtime {
    pub fn new(paths: &Paths, config: RuntimeConfig) -> Result<Self> {
        let home = &paths.home;
        config.validate()?;
        if !home.is_absolute() {
            return Err(Error::Home("an explicit absolute home is required"));
        }
        let _home_directory = private_directory(home)?;
        let _state_directory = private_directory(&paths.state)?;
        if std::env::var("DBOS__CLOUD").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
            return Err(Error::Config(
                "DBOS Cloud identity overrides are not allowed for this local executor",
            ));
        }
        let marker = crate::essentials::PidMarker::acquire(paths)
            .map_err(|_| Error::Home("exclusive home executor is unavailable"))?;
        seal_identity(paths, &config)?;
        let engine = DBOS::new(config.sdk());
        Ok(Self {
            config,
            owner: Arc::new(HomeOwner { _marker: marker }),
            engine,
            launched: false,
            registrations: Mutex::new(std::collections::BTreeSet::new()),
            bridge: None,
            application: None,
        })
    }
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
    pub fn install_submission_bridge(
        &mut self,
        store: Arc<admission::AdmissionStore>,
    ) -> Result<Arc<admission::SubmissionBridge>> {
        if self.launched || self.bridge.is_some() {
            return Err(Error::Config("one bridge must be installed before launch"));
        }
        let installed = maintenance::install(self, store)?;
        let bridge = Arc::clone(&installed.bridge);
        self.bridge = Some(installed);
        Ok(bridge)
    }
    pub fn register_workflow<P, R, E, F, Fut>(
        &self,
        name: &str,
        workflow: F,
    ) -> Result<::dbos::WorkflowRef<P, R, E>>
    where
        F: Fn(P) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ::dbos::Result<R, E>> + Send + 'static,
        P: Serialize + DeserializeOwned + Send + 'static,
        R: Serialize + DeserializeOwned + Send + 'static,
        E: ::dbos::DurableError,
    {
        if self.launched || self.bridge.is_some() {
            return Err(Error::Config("workflows must register before launch"));
        }
        let owner = Arc::clone(&self.owner);
        let registered = self
            .engine
            .register_workflow(name.to_owned(), move |input| {
                let owner = Arc::clone(&owner);
                let future = workflow(input);
                async move {
                    let _owner = owner;
                    future.await
                }
            })?;
        self.registrations
            .lock()
            .map_err(|_| Error::Config("registration set poisoned"))?
            .insert(name.to_owned());
        Ok(registered)
    }
    pub async fn launch(&mut self) -> Result<()> {
        if self.launched {
            return Err(Error::Config("executor is already launched"));
        }
        self.prepare().await?;
        let client = Client::connect(self.config.client()).await?;
        let preflight = self.verify_backlog(&client).await;
        client.close().await;
        preflight?;
        self.engine.launch().await?;
        self.launched = true;
        Ok(())
    }

    /// Migrate and verify metadata without starting recovery or dequeuing work.
    pub async fn prepare(&self) -> Result<()> {
        if self.launched {
            return Err(Error::Config("cannot prepare policy after launch"));
        }
        let bootstrap = tokio::time::timeout(
            std::time::Duration::from_secs(self.config.connect_timeout_seconds),
            postgres::PostgresSystemDatabase::connect(&postgres::Config {
                max_connections: self.config.max_connections,
                settings: postgres::Settings {
                    schema: &self.config.schema,
                    application_name: Some(&self.config.application),
                    ..Default::default()
                },
                ..postgres::Config::new(&self.config.database_url)
            }),
        )
        .await
        .map_err(|_| Error::BootstrapTimeout)??;
        bootstrap.close().await;
        let client = Client::connect(self.config.client()).await?;
        let checked = async {
            self.verify_queues(&client).await?;
            if let Some(bridge) = &self.bridge {
                bridge
                    .prepare(&client, &self.config, self.application.as_deref())
                    .await?;
            } else if !client.list_schedules(&Default::default()).await?.is_empty() {
                return Err(Error::Config(
                    "unregistered native schedules refuse startup",
                ));
            }
            Ok(())
        }
        .await;
        client.close().await;
        checked
    }
    async fn verify_backlog(&self, client: &Client) -> Result<()> {
        use ::dbos::sysdb::types::{WorkflowFilter, WorkflowStatus};
        let known = self
            .registrations
            .lock()
            .map_err(|_| Error::Config("registration set poisoned"))?
            .clone();
        // SDK offset paging orders only by created_at, which is not unique.
        // Enumerate the complete stopped metadata identity set first, then read
        // bounded exact-ID batches; tied timestamps cannot skip validation.
        let ids = client
            .list_workflows(&WorkflowFilter {
                status: vec![
                    WorkflowStatus::Pending,
                    WorkflowStatus::Enqueued,
                    WorkflowStatus::Delayed,
                ],
                limit: None,
                load_input: false,
                load_output: false,
                ..Default::default()
            })
            .await?
            .into_iter()
            .map(|row| row.workflow_id)
            .collect::<Vec<_>>();
        let mut seen = std::collections::BTreeSet::new();
        for batch in ids.chunks(256) {
            let rows = client
                .list_workflows(&WorkflowFilter {
                    workflow_ids: batch.iter().map(String::as_str).collect(),
                    limit: None,
                    load_input: self.bridge.is_some(),
                    load_output: false,
                    ..Default::default()
                })
                .await?;
            if rows.len() != batch.len() {
                return Err(Error::Config(
                    "native backlog changed during stopped preflight",
                ));
            }
            for row in rows {
                if !seen.insert(row.workflow_id.clone()) {
                    return Err(Error::Config(
                        "native backlog changed during stopped preflight",
                    ));
                }
                let name = row
                    .qualified_name()
                    .map(|v| v.to_string())
                    .ok_or(Error::Config(
                        "pending workflow has no complete registered name",
                    ))?;
                if !known.contains(&name)
                    || row.app_name.as_deref() != Some(&self.config.application)
                    || row.app_version.as_deref() != Some(&self.config.app_version)
                {
                    return Err(Error::Config(
                        "pending workflow has unknown handler or incompatible identity",
                    ));
                }
                if let Some(bridge) = &self.bridge {
                    if name == crate::core::SCHEDULED {
                        self.application
                            .as_ref()
                            .ok_or(Error::Config("application validator missing"))?
                            .validate_scheduled_input(&row)
                            .map_err(|_| {
                                Error::Config("scheduled input failed scope validation")
                            })?;
                    } else {
                        bridge.verify_pending(&row).await?;
                    }
                }
            }
        }
        Ok(())
    }
    async fn verify_queues(&self, client: &Client) -> Result<()> {
        let work = QueueOptions {
            concurrency: Some(self.config.work_concurrency),
            ..Default::default()
        };
        let models = QueueOptions {
            concurrency: Some(self.config.model_concurrency),
            partition_concurrency: Some(self.config.server_concurrency),
            ..Default::default()
        };
        for (name, desired) in [
            (self.config.work_queue(), work),
            (self.config.model_queue(), models),
        ] {
            let actual = client
                .register_queue(&name, desired.clone(), QueueConflict::NeverUpdate)
                .await?;
            if !queue_matches(&actual, &desired) {
                return Err(Error::Config(
                    "stored queue policy differs; quiesce/drain and authorize a policy revision before updating",
                ));
            }
        }
        for queue in client.list_queues().await? {
            if ![
                INTERNAL_QUEUE,
                self.config.work_queue().as_str(),
                self.config.model_queue().as_str(),
            ]
            .contains(&queue.name())
            {
                return Err(Error::Config(
                    "unexpected application queue; fixed two-queue topology required",
                ));
            }
        }
        Ok(())
    }
    pub fn retrieve<R: DeserializeOwned, E: ::dbos::DurableError>(
        &self,
        id: &str,
    ) -> Result<::dbos::WorkflowHandle<R, E>> {
        Ok(self.engine.retrieve_workflow(id)?)
    }
    pub async fn shutdown(&mut self) {
        if self.launched {
            self.engine.shutdown().await;
            self.launched = false;
        }
    }
}
fn queue_matches(actual: &Queue, desired: &QueueOptions) -> bool {
    actual.concurrency() == desired.concurrency
        && actual.worker_concurrency() == desired.worker_concurrency
        && actual.rate_limit() == desired.rate_limit
        && actual.partition_concurrency() == desired.partition_concurrency
        && actual.partition_worker_concurrency() == desired.partition_worker_concurrency
        && actual.partition_rate_limit() == desired.partition_rate_limit
        && actual.polling_interval() == desired.polling_interval
        && actual.is_partitioned() == desired.partition_concurrency.is_some()
}
