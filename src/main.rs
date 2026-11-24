use agent_forge::operations::{
    Approvals, Backups, Definitions, Operation, Plugins, Providers, Queues, Runners, Runs,
    Schedules, Service, Settings, Skills, Subagents, Work,
};
use clap::{Parser, Subcommand};
use std::{net::IpAddr, path::PathBuf};
#[derive(Parser)]
#[command(
    name = "soulsteads-forge",
    version,
    about = "Native Rust DBOS work execution"
)]
struct Cli {
    /// Forge home. Default: FORGE_HOME, then $XDG_DATA_HOME/forge, then ~/.local/share/forge.
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    /// Service URL. Default: FORGE_URL, then the home's config.json `server` section.
    #[arg(long, global = true)]
    url: Option<String>,
    /// PostgreSQL URL for serve, backup and restore. Prefer FORGE_DATABASE_URL or
    /// config.json `database.url_env`: command-line values are visible to other users.
    #[arg(long, global = true)]
    database_url: Option<String>,
    #[arg(long, global = true, default_value = "FORGE_TOKEN")]
    token_env: String,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Init,
    Skill,
    /// Read-only diagnosis of config, database, runners, plugins, definitions, lost runs and
    /// authority gaps. Uses the running service; with it stopped runs the offline subset.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Skip the service and run only the checks that need the home and the database.
        #[arg(long)]
        offline: bool,
    },
    /// Export and import work and schedule definitions as JSON.
    Definitions {
        #[command(subcommand)]
        command: Definitions,
    },
    /// Preview or install a Linux systemd user or system service.
    Service {
        #[command(subcommand)]
        command: Service,
    },
    /// Health, upcoming work and recent failures.
    Overview,
    /// Active and recent agent (harness/ACP) runs by runner.
    Agents {
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Durable approval requests: list, approve, reject.
    Approvals {
        #[command(subcommand)]
        command: Approvals,
    },
    /// Subagents: spawn, poll, list and cancel harness runs on operator-approved runner profiles.
    Subagents {
        #[command(subcommand)]
        command: Subagents,
    },
    /// Read-only catalog of skills compiled into this build.
    Skills {
        #[command(subcommand)]
        command: Skills,
    },
    /// Effective settings with sources; save config.json-backed values.
    Settings {
        #[command(subcommand)]
        command: Settings,
    },
    /// Model aliases and runner profiles: saved versus running; validated edits.
    Providers {
        #[command(subcommand)]
        command: Providers,
    },
    /// Online backups through the service (restore stays the offline `restore` command).
    Backups {
        #[command(subcommand)]
        command: Backups,
    },
    Serve {
        /// Loopback address. Default: FORGE_HOST, then config.json `server.host`.
        #[arg(long)]
        host: Option<IpAddr>,
        /// Port. Default: FORGE_PORT, then config.json `server.port`.
        #[arg(long)]
        port: Option<u16>,
    },
    Mcp,
    Snapshot,
    Work {
        #[command(subcommand)]
        command: Work,
    },
    /// Author, inspect and run multi-step workflows (human-readable; --json for stable results).
    Workflows {
        #[command(subcommand)]
        command: agent_forge::Workflows,
    },
    Runs {
        #[command(subcommand)]
        command: Runs,
    },
    Schedules {
        #[command(subcommand)]
        command: Schedules,
    },
    /// Native DBOS queue capacity, waiting and running work, and why work waits.
    Queues {
        #[command(subcommand)]
        command: Queues,
    },
    Plugins {
        #[command(subcommand)]
        command: Plugins,
    },
    Runners {
        #[command(subcommand)]
        command: Runners,
    },
    Backup {
        destination: PathBuf,
    },
    Restore {
        source: PathBuf,
    },
}
#[tokio::main(worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let result = run(cli).await;
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if let Some(status) = error.downcast_ref::<agent_forge::client::ExitStatus>() {
                return std::process::ExitCode::from(status.0);
            }
            let denied = error
                .downcast_ref::<agent_forge::core::Denied>()
                .map(|_| error.to_string());
            let message = if let Some(text) = &denied {
                text.as_str()
            } else if let Some(e) = error.downcast_ref::<agent_forge::client::ClientError>() {
                &e.0
            } else if let Some(e) = error.downcast_ref::<agent_forge::core::Invalid>() {
                e.0
            } else if let Some(e) = error.downcast_ref::<agent_forge::settings::ConfigError>() {
                &e.0
            } else if let Some(
                agent_forge::dbos::Error::Config(e) | agent_forge::dbos::Error::Home(e),
            ) = error.downcast_ref::<agent_forge::dbos::Error>()
            {
                e
            } else {
                "Operation failed: check explicit configuration, active home ownership, database connectivity and recovery markers"
            };
            eprintln!("{message}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run(cli: Cli) -> anyhow::Result<()> {
    use agent_forge::settings::{self, Overrides, Settings};
    let overrides = Overrides {
        database_url: cli.database_url.clone(),
        host: None,
        port: None,
        url: cli.url.clone(),
    };
    let env = &settings::process_env;
    // Client commands only need the service URL.
    let client_url = || -> anyhow::Result<String> {
        let home = settings::resolve_home(cli.home.clone(), env)?;
        settings::client_url(&home, &overrides, env)
    };
    let command = match cli.command {
        Command::Mcp => {
            let url = client_url()?;
            return agent_forge::mcp::serve(url, cli.token_env).await;
        }
        Command::Snapshot => {
            return Operation::Snapshot
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Overview => {
            return Operation::Overview
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Agents { limit } => {
            return Operation::Agents { limit }
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Approvals { command } => {
            return Operation::Approvals(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Subagents { command } => {
            return Operation::Subagents(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Skills { command } => {
            return Operation::Skills(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Settings { command } => {
            return Operation::Settings(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Providers { command } => {
            return Operation::Providers(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Backups { command } => {
            return Operation::Backups(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Work { command } => {
            return Operation::Work(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Workflows { command } => {
            return Operation::Workflows(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Runs { command } => {
            return Operation::Runs(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Schedules { command } => {
            return Operation::Schedules(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Queues { command } => {
            return Operation::Queues(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Definitions { command } => {
            return Operation::Definitions(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Runners { command } => {
            return Operation::Runners(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        Command::Plugins { command } => {
            return Operation::Plugins(command)
                .execute(&client_url()?, &cli.token_env)
                .await;
        }
        other => other,
    };
    let home = settings::resolve_home(cli.home.clone(), env)?;
    match command {
        Command::Doctor { json, offline } => {
            let url = client_url()?;
            agent_forge::doctor::cli(home, overrides, url, &cli.token_env, json, offline).await
        }
        Command::Service { command } => {
            let (action, name, env_file, start, scope) = match command {
                Service::Preview {
                    scope,
                    name,
                    env_file,
                } => ("preview", name, env_file, false, scope),
                Service::Install {
                    scope,
                    name,
                    env_file,
                    start,
                } => ("install", name, env_file, start, scope),
                Service::Uninstall { name } => ("uninstall", name, None, false, Default::default()),
                Service::Status { name } => ("status", name, None, false, Default::default()),
            };
            if !scope.system && scope.run_as.is_some() {
                return Err(agent_forge::settings::ConfigError(
                    "--run-as requires --system".into(),
                )
                .into());
            }
            let result = if scope.system {
                let system_home = cli
                    .home
                    .as_deref()
                    .filter(|path| path.is_absolute())
                    .ok_or_else(|| {
                        settings::ConfigError(
                            "--system requires an explicit absolute --home path".into(),
                        )
                    })?;
                if cli.database_url.is_some() || cli.url.is_some() {
                    return Err(settings::ConfigError(
                        "system service settings come from the home and optional --env-file, not --url or --database-url".into(),
                    ).into());
                }
                let run_as = scope.run_as.as_deref().ok_or_else(|| {
                    settings::ConfigError("--system requires --run-as USER".into())
                })?;
                let exe = std::env::current_exe()?;
                agent_forge::service_unit::run_system(
                    action,
                    system_home,
                    &exe,
                    agent_forge::service_unit::SystemRequest {
                        name: name.as_deref(),
                        env_file: env_file.as_deref(),
                        run_as,
                        start,
                    },
                )?
            } else {
                let policy = agent_forge::core::load_policy(&home)?;
                let resolved = Settings::resolve(home, &policy, &overrides, env)?;
                agent_forge::service_unit::run(
                    action,
                    &resolved,
                    &policy,
                    agent_forge::service_unit::Request {
                        name: name.as_deref(),
                        env_file: env_file.as_deref(),
                        start,
                    },
                    agent_forge::settings::systemd_user_dir(env)?,
                )
                .await?
            };
            if action == "preview" {
                print!("{}", result["text"].as_str().unwrap_or(""));
            } else {
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            Ok(())
        }
        Command::Skill => {
            print!("{}", agent_forge::skills::operating_skill());
            Ok(())
        }
        Command::Init => {
            agent_forge::core::init(&home)?;
            println!("Forge home initialized; keep config.json private");
            Ok(())
        }
        Command::Serve { host, port } => {
            let policy = agent_forge::core::load_policy(&home)?;
            let overrides = Overrides {
                host,
                port,
                ..overrides
            };
            let settings = Settings::resolve(home, &policy, &overrides, env)?;
            agent_forge::server::serve(settings, policy).await
        }
        Command::Backup { destination } => {
            let policy = agent_forge::core::load_policy(&home)?;
            let settings = Settings::resolve(home, &policy, &overrides, env)?;
            agent_forge::backup::backup(settings, std::path::absolute(destination)?).await
        }
        Command::Restore { source } => {
            agent_forge::backup::restore(home, std::path::absolute(source)?, &overrides, env).await
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operational_arguments_require_intent_and_preserve_mcp() {
        for args in [
            vec!["soulsteads-forge", "work", "run", "w"],
            vec![
                "soulsteads-forge",
                "schedules",
                "create",
                "s",
                "--work-id",
                "w",
                "--cron",
                "* * * * *",
            ],
            vec![
                "soulsteads-forge",
                "runs",
                "reconcile",
                "r",
                "--decision",
                "not-applied",
                "--note",
                "evidence",
            ],
            vec!["soulsteads-forge", "--token", "secret", "snapshot"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
        let parsed = Cli::try_parse_from([
            "soulsteads-forge",
            "mcp",
            "--url",
            "http://127.0.0.1:8110",
            "--token-env",
            "CUSTOM_TOKEN",
        ])
        .unwrap();
        assert_eq!(parsed.token_env, "CUSTOM_TOKEN");
        assert!(matches!(parsed.command, Command::Mcp));
        let parsed = Cli::try_parse_from([
            "soulsteads-forge",
            "runs",
            "reconcile",
            "r",
            "--decision",
            "not-applied",
            "--note",
            "provider proof",
            "--settled",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Command::Runs {
                command: Runs::Reconcile { settled: true, .. }
            }
        ));
        let parsed = Cli::try_parse_from([
            "soulsteads-forge",
            "work",
            "register",
            "w",
            "--definition",
            "-",
            "--url",
            "http://127.0.0.1:19100",
        ])
        .unwrap();
        assert_eq!(parsed.url.as_deref(), Some("http://127.0.0.1:19100"));
    }

    #[test]
    fn plugins_run_passes_everything_after_the_command_through() {
        let parsed = Cli::try_parse_from([
            "soulsteads-forge",
            "plugins",
            "run",
            "demo",
            "hello",
            "--flag",
            "-x",
            "value",
        ])
        .unwrap();
        let Command::Plugins {
            command: Plugins::Run { id, command },
        } = parsed.command
        else {
            panic!("expected plugins run");
        };
        assert_eq!(id, "demo");
        assert_eq!(command, ["hello", "--flag", "-x", "value"]);
        assert!(Cli::try_parse_from(["soulsteads-forge", "plugins", "run", "demo"]).is_err());
        assert!(Cli::try_parse_from(["soulsteads-forge", "plugins", "install"]).is_err());
        assert!(Cli::try_parse_from(["soulsteads-forge", "plugins", "enable", "demo"]).is_ok());
    }

    #[test]
    fn parity_commands_parse_and_conflicting_flags_are_refused() {
        let ok = |args: &[&str]| {
            let mut v = vec!["soulsteads-forge"];
            v.extend_from_slice(args);
            Cli::try_parse_from(v)
        };
        assert!(matches!(
            ok(&["doctor", "--json", "--offline"]).unwrap().command,
            Command::Doctor {
                json: true,
                offline: true
            }
        ));
        assert!(matches!(
            ok(&["runners", "test", "codex"]).unwrap().command,
            Command::Runners {
                command: Runners::Test { .. }
            }
        ));
        assert!(matches!(
            ok(&["definitions", "import", "f.json", "--replace", "--dry-run"])
                .unwrap()
                .command,
            Command::Definitions {
                command: Definitions::Import {
                    replace: true,
                    dry_run: true,
                    ..
                }
            }
        ));
        let edit = ok(&[
            "schedules",
            "edit",
            "s",
            "--cron",
            "0 9 * * *",
            "--tz",
            "America/Chicago",
            "--tag",
            "a",
            "--tag",
            "b",
            "--disable",
        ])
        .unwrap();
        let Command::Schedules {
            command:
                Schedules::Edit {
                    tags,
                    disable,
                    enable,
                    ..
                },
        } = edit.command
        else {
            panic!("expected schedules edit");
        };
        assert_eq!(
            (tags, disable, enable),
            (vec!["a".into(), "b".into()], true, false)
        );
        assert!(ok(&["schedules", "edit", "s", "--enable", "--disable"]).is_err());
        assert!(ok(&["schedules", "edit", "s", "--tag", "a", "--clear-tags"]).is_err());
        assert!(
            ok(&[
                "schedules",
                "edit",
                "s",
                "--params",
                "p.json",
                "--clear-params"
            ])
            .is_err()
        );
        assert!(ok(&["schedules", "status", "--tag", "nightly"]).is_ok());
        assert!(
            ok(&[
                "service",
                "install",
                "--name",
                "forge-x",
                "--env-file",
                "e",
                "--start"
            ])
            .is_ok()
        );
        assert!(ok(&["service", "uninstall"]).is_ok());
        let system = Cli::try_parse_from([
            "soulsteads-forge",
            "service",
            "preview",
            "--system",
            "--run-as",
            "forge-service",
        ]);
        if let Err(error) = system {
            panic!("system preview arguments should parse: {error}");
        }
        assert!(
            Cli::try_parse_from([
                "soulsteads-forge",
                "service",
                "preview",
                "--user",
                "--system",
            ])
            .is_err()
        );
    }
}
