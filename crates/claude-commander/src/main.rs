//! Claude Commander - Terminal UI for managing Claude coding sessions
//!
//! Run with `claude-commander` or `claude-commander --help` for usage.

use std::io::{IsTerminal, Write};

use clap::Parser;
use color_eyre::eyre::Result;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use claude_commander_core::{
    config::{AppState, Config, ConfigStore, StateStore},
    tmux::{AttachResult, attach_to_session},
};
use claude_commander_tui::App;

use crate::cli_args::{Cli, Commands, ReviewCommand, SetReviewedArgs, cli_reference};

mod cli_args;
mod serve;

/// This binary's name and version. Everything user-facing (`--version`, the
/// startup log line, the telemetry frontend) comes from this package's
/// metadata, never from `claude_commander_core::VERSION`, which is the
/// library's.
const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identify this binary to the telemetry layer. Required by
/// `CommanderService`/`App` — they panic if a frontend isn't supplied.
fn frontend() -> claude_commander_core::telemetry::FrontendInfo {
    claude_commander_core::telemetry::FrontendInfo::new(NAME, VERSION)
}

/// The remote-backend factory injected into the TUI. Keeps core free of a
/// dependency on the HTTP client crate: core hands each `[[remote_servers]]`
/// config entry to this closure, which maps it to a `RemoteServerSpec` and
/// builds a `RemoteBackend`. A construction failure (malformed URL, etc.) is
/// surfaced as a `BackendError` that the TUI renders as a degraded server.
fn remote_backend_factory() -> claude_commander_core::backend::RemoteBackendFactory {
    std::sync::Arc::new(build_remote_backend)
}

/// Build a `RemoteBackend` for one `[[remote_servers]]` config entry, boxed as
/// the object-safe `CommanderBackend`. Shared by the TUI's backend factory and
/// the CLI's `--remote` path so both map a config entry to a client the same
/// way. A construction failure (malformed URL, etc.) surfaces as a
/// `BackendError`.
fn build_remote_backend(
    cfg: &claude_commander_core::config::RemoteServerConfig,
) -> claude_commander_core::backend::BResult<
    std::sync::Arc<dyn claude_commander_core::backend::CommanderBackend>,
> {
    use claude_commander_remote::{RemoteBackend, RemoteServerSpec, SecretString};
    let spec = RemoteServerSpec {
        name: cfg.name.clone(),
        base_url: cfg.url.clone(),
        token: cfg.token.clone().map(SecretString::new),
    };
    let backend = RemoteBackend::new(spec)?;
    Ok(std::sync::Arc::new(backend)
        as std::sync::Arc<
            dyn claude_commander_core::backend::CommanderBackend,
        >)
}

/// Resolve a `CommanderBackend` for a one-shot CLI command. `remote` is the
/// value of a `--remote <name>` flag: `None` builds the in-process
/// [`LocalBackend`] (the historical CLI behaviour), `Some(name)` looks the name
/// up in `[[remote_servers]]` and connects to that server. An unknown name is a
/// hard error listing the configured servers (see
/// `Config::find_remote_server`).
fn resolve_cli_backend(
    config: Config,
    remote: Option<&str>,
) -> Result<std::sync::Arc<dyn claude_commander_core::backend::CommanderBackend>> {
    match remote {
        None => {
            let service =
                claude_commander_core::api::CommanderService::for_cli(config, frontend())?;
            Ok(std::sync::Arc::new(
                claude_commander_core::backend::LocalBackend::new(service),
            ))
        }
        Some(name) => {
            let server = config.find_remote_server(name)?.clone();
            Ok(build_remote_backend(&server)?)
        }
    }
}

/// Resolve a session by exact title or full ID for a command that mutates or
/// inspects one session, exiting with the CLI's usual message when it is
/// missing or ambiguous.
async fn resolve_session_exact(
    service: &claude_commander_core::api::CommanderService,
    session: &str,
) -> Result<claude_commander_core::api::SessionInfo> {
    match service.find_session_exact(session).await? {
        claude_commander_core::session::SessionLookup::Found(i) => Ok(i),
        claude_commander_core::session::SessionLookup::NotFound => {
            eprintln!("Session not found: {}", session);
            eprintln!("Use 'claude-commander list' to see available sessions.");
            std::process::exit(1);
        }
        claude_commander_core::session::SessionLookup::Ambiguous(n) => {
            eprintln!(
                "\"{}\" matches {} sessions. Use the exact title or full ID.",
                session, n
            );
            eprintln!("Use 'claude-commander list' to see available sessions.");
            std::process::exit(1);
        }
    }
}

/// Print a `review` command's expected refusal and exit 1, or hand back a
/// genuine failure for the usual error report.
fn exit_on_review_refusal(
    err: claude_commander_core::Error,
    session: &str,
    writes: bool,
) -> color_eyre::eyre::Report {
    if let Some(report) = claude_commander_core::cli::review_refusal(&err, session, writes) {
        eprintln!("{report}");
        std::process::exit(1);
    }
    err.into()
}

/// Run a `review` subcommand against the local service.
async fn execute_review(config: Config, command: ReviewCommand) -> Result<()> {
    use claude_commander_core::cli;

    let service = claude_commander_core::api::CommanderService::for_cli(config, frontend())?;
    match command {
        ReviewCommand::Files {
            session,
            unreviewed,
            json,
        } => {
            let info = resolve_session_exact(&service, &session).await?;
            let peek = service.peek_review(&info.session_id).await?;
            let entries = cli::review_file_entries(&peek, unreviewed);
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                println!("{}", cli::format_review_files_human(&entries));
            }
        }
        ReviewCommand::Diff {
            session,
            files,
            unreviewed,
        } => {
            let info = resolve_session_exact(&service, &session).await?;
            let peek = service.peek_review(&info.session_id).await?;
            let selected = cli::select_review_files(&peek, &files, unreviewed)
                .map_err(|e| exit_on_review_refusal(e.into(), &session, false))?;
            // Buffered: stdout is line-buffered, so a large diff would
            // otherwise cost a write per line.
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            for file in selected {
                out.write_all(cli::render_file_diff(file).as_bytes())?;
            }
            out.flush()?;
        }
        ReviewCommand::Mark(args) => set_reviewed(&service, args, true).await?,
        ReviewCommand::Unmark(args) => set_reviewed(&service, args, false).await?,
    }
    Ok(())
}

/// `review mark` / `review unmark`: set every named (or `--all`) file's mark.
async fn set_reviewed(
    service: &claude_commander_core::api::CommanderService,
    args: SetReviewedArgs,
    reviewed: bool,
) -> Result<()> {
    use claude_commander_core::reviewed::FileSelection;

    let info = resolve_session_exact(service, &args.session).await?;
    let selection = if args.all {
        FileSelection::All
    } else {
        FileSelection::Paths(args.files)
    };
    let outcome = service
        .set_files_reviewed(&info.session_id, &selection, reviewed)
        .await
        .map_err(|e| exit_on_review_refusal(e, &args.session, true))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        println!(
            "{}",
            claude_commander_core::cli::format_set_reviewed_human(&outcome, reviewed)
        );
    }
    Ok(())
}

fn setup_logging(debug: bool, to_file: bool) -> Result<()> {
    let filter = if debug {
        EnvFilter::new("debug")
    } else {
        // Use info level for our crate, warn for dependencies. The embedded
        // server shares this subscriber — installing its own would panic on the
        // second global init — so its target is named explicitly here.
        EnvFilter::new("info")
            .add_directive("claude_commander_server=info".parse()?)
            .add_directive("gix=warn".parse()?)
            .add_directive("tokio=warn".parse()?)
    };

    if to_file {
        // Log to file when running TUI (so logs don't interfere with display)
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/claude-commander.log")?;

        tracing_subscriber::registry()
            .with(fmt::layer().with_writer(file).with_target(false))
            .with(filter)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(fmt::layer().with_target(false))
            .with(filter)
            .init();
    }

    Ok(())
}

/// Raise the soft limit on open file descriptors toward the hard cap, so
/// fan-out subprocess work (gh, git diff, tmux) doesn't EMFILE on the
/// stingy macOS launchd default of 256.
///
/// Best-effort: failures are logged to stderr and do not abort startup.
#[cfg(unix)]
fn raise_fd_limit() {
    // Target soft limit. macOS's hard cap is typically kern.maxfilesperproc
    // (~184k); Linux varies. We cap our request to keep things reasonable.
    const TARGET_SOFT: nix::libc::rlim_t = 8192;

    let mut rlim = nix::libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    // SAFETY: rlim is a valid stack pointer to a properly-sized rlimit.
    let rc = unsafe { nix::libc::getrlimit(nix::libc::RLIMIT_NOFILE, &mut rlim) };
    if rc != 0 {
        eprintln!(
            "Warning: getrlimit(RLIMIT_NOFILE) failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    if rlim.rlim_cur >= TARGET_SOFT {
        return;
    }

    let new_soft = TARGET_SOFT.min(rlim.rlim_max);
    if new_soft <= rlim.rlim_cur {
        return;
    }

    let new_rlim = nix::libc::rlimit {
        rlim_cur: new_soft,
        rlim_max: rlim.rlim_max,
    };
    // SAFETY: new_rlim is a valid stack pointer to a properly-sized rlimit.
    let rc = unsafe { nix::libc::setrlimit(nix::libc::RLIMIT_NOFILE, &new_rlim) };
    if rc != 0 {
        eprintln!(
            "Warning: setrlimit(RLIMIT_NOFILE, {}) failed: {}",
            new_soft,
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

/// Execute async PTY-based attach to a tmux session
async fn execute_attach(session_name: &str, editor_triggers: Vec<Vec<u8>>) {
    // CLI `attach` resolves a Claude session by title/ID, never a shell. The
    // review toggle has no standalone UI here, so it's disabled (empty triggers).
    // Voice input needs the TUI's conversation runtime, so it's disabled here too.
    match attach_to_session(
        session_name,
        editor_triggers,
        Vec::new(),
        Vec::new(),
        None,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        true,
    )
    .await
    {
        Ok(outcome) => match outcome.result {
            AttachResult::Detached
            | AttachResult::SwitchToShell
            | AttachResult::SwitchToReview
            | AttachResult::OpenEditor
            // The CLI attach sets `switcher_enabled: false`, so Ctrl+Space
            // reaches the pane and this never fires; it is here so adding a
            // frontend-serviced result can't silently fall through.
            | AttachResult::OpenSwitcher => {
                info!("Detached from session");
            }
            AttachResult::SessionEnded => {
                info!("Session ended");
            }
            AttachResult::Error(e) => {
                eprintln!("Attach error: {}", e);
            }
        },
        Err(e) => {
            eprintln!("Failed to attach: {}", e);
        }
    }
}

/// Attach to a session on a remote server over the backend's WebSocket
/// transport, driving the same interactive loop the local PTY path uses. The
/// driver itself lives in core (`attach_backend_session`); this only maps the
/// outcome to CLI logging.
async fn execute_remote_attach(
    backend: std::sync::Arc<dyn claude_commander_core::backend::CommanderBackend>,
    query: &str,
    editor_triggers: Vec<Vec<u8>>,
) {
    match claude_commander_core::tmux::attach_backend_session(backend, query, editor_triggers).await
    {
        Ok(outcome) => match outcome.result {
            AttachResult::Detached
            | AttachResult::SwitchToShell
            | AttachResult::SwitchToReview
            | AttachResult::OpenEditor
            | AttachResult::OpenSwitcher => info!("Detached from remote session"),
            AttachResult::SessionEnded => info!("Remote session ended"),
            AttachResult::Error(e) => eprintln!("Attach error: {e}"),
        },
        Err(e) => eprintln!("Failed to attach: {e}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install color-eyre error hooks
    color_eyre::install()?;

    // macOS launchd hands processes a soft RLIMIT_NOFILE of 256, which is
    // easily exhausted once we fan out subprocesses across many sessions
    // (gh pr list, git diff, tmux commands). Raise our own soft limit
    // toward the hard cap before anything else runs.
    raise_fd_limit();

    let cli = Cli::parse();

    // Load configuration
    let config = Config::load().unwrap_or_else(|e| {
        eprintln!("Warning: Failed to load config, using defaults: {}", e);
        Config::default()
    });

    // Ensure required directories exist
    if let Err(e) = config.ensure_directories() {
        eprintln!("Warning: Failed to create directories: {}", e);
    }

    match cli.command {
        None | Some(Commands::Tui) => {
            setup_logging(cli.debug, true)?;
            info!("Starting Claude Commander TUI v{}", VERSION);

            // Settle the server plan (and persist any generated token) before
            // the config store exists — see `serve::prepare`.
            let mut config = config;
            let plan = serve::prepare(
                &mut config,
                &Config::config_file_path()?,
                cli.serve,
                cli.no_serve,
            );

            let config_store = std::sync::Arc::new(ConfigStore::new(config.clone())?);
            let app_state = AppState::load_or_exit();
            let store = std::sync::Arc::new(StateStore::new(app_state)?);
            let mut app = App::new(
                config_store,
                store,
                frontend(),
                remote_backend_factory(),
                cli_reference(),
            );

            // Held for the rest of `main`: dropping the guard stops the server,
            // so the listener goes away exactly when the TUI does.
            //
            // This binds before `app.run()`, which means a client can be served
            // during the TUI's startup reconciliation (dropping stale `Creating`
            // sessions, syncing status against live tmux) and briefly see
            // pre-reconcile state. Accepted deliberately: the window is a few
            // milliseconds, it self-corrects on the client's next poll, and
            // closing it would mean either delaying the listener behind the TUI's
            // startup or plumbing the server into `App::run` — which would make
            // the terminal frontend depend on axum.
            let _server = match plan {
                Some(plan) => {
                    let (guard, status) = serve::start(app.service_handle(), plan).await;
                    app.set_embedded_server(status);
                    guard
                }
                None => None,
            };

            app.run().await?;
        }

        Some(Commands::List {
            all,
            json,
            workspace,
        }) => {
            setup_logging(cli.debug, false)?;

            if json {
                let service = claude_commander_core::api::CommanderService::for_cli(
                    config.clone(),
                    frontend(),
                )?;
                let projects = service.list_projects().await;
                let workspaces = claude_commander_core::cli::CliWorkspaces::resolve(
                    &config,
                    projects.iter().filter_map(|p| p.workspace.as_deref()),
                    workspace.as_deref(),
                )?;
                let tag_of = |id| {
                    projects
                        .iter()
                        .find(|p| p.id == id)
                        .and_then(|p| p.workspace.clone())
                };
                let sessions = service.list_sessions(all).await?;
                let entries: Vec<_> = sessions
                    .iter()
                    .map(|s| (s, tag_of(s.project_id)))
                    .filter(|(_, tag)| workspaces.includes(tag.as_deref()))
                    .map(|(s, tag)| {
                        claude_commander_core::cli::SessionJsonEntry::from_info(s)
                            .with_workspace(tag)
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                let app_state = AppState::load_or_exit();
                let workspaces = claude_commander_core::cli::CliWorkspaces::resolve(
                    &config,
                    app_state
                        .projects
                        .values()
                        .filter_map(|p| p.workspace.as_deref()),
                    workspace.as_deref(),
                )?;

                println!("Sessions:");
                println!();

                if app_state.projects.is_empty() {
                    println!("  No projects. Use 'claude-commander' to add one.");
                    return Ok(());
                }

                for project in app_state
                    .projects
                    .values()
                    .filter(|p| workspaces.includes(p.workspace.as_deref()))
                {
                    if workspaces.show_column() {
                        println!(
                            "  {} ({})  [{}]",
                            project.name,
                            project.main_branch,
                            workspaces.label(project.workspace.as_deref())
                        );
                    } else {
                        println!("  {} ({})", project.name, project.main_branch);
                    }

                    let sessions: Vec<_> = project
                        .worktrees
                        .iter()
                        .filter_map(|id| app_state.sessions.get(id))
                        .filter(|s| all || s.status.is_active())
                        .collect();

                    if sessions.is_empty() {
                        println!("    (no sessions)");
                    } else {
                        for session in sessions {
                            let status_icon = match session.status {
                                claude_commander_core::SessionStatus::Creating
                                | claude_commander_core::SessionStatus::Merging
                                | claude_commander_core::SessionStatus::Pushing => "⠋",
                                claude_commander_core::SessionStatus::Running => "●",
                                claude_commander_core::SessionStatus::Stopped => "○",
                                claude_commander_core::SessionStatus::CascadePaused => "⏸",
                            };
                            match claude_commander_core::session::display_branch(
                                &session.title,
                                &session.branch,
                            ) {
                                Some(shown_branch) => println!(
                                    "    {} {} [{}] ({})",
                                    status_icon, session.title, shown_branch, session.program
                                ),
                                None => println!(
                                    "    {} {} ({})",
                                    status_icon, session.title, session.program
                                ),
                            }
                        }
                    }
                    println!();
                }
            }
        }

        Some(Commands::Status { session, json }) => {
            setup_logging(cli.debug, false)?;

            let service =
                claude_commander_core::api::CommanderService::for_cli(config, frontend())?;
            let detail = match service.get_session_detail(&session, None).await? {
                Some(d) => d,
                None => {
                    eprintln!("Session not found: {}", session);
                    eprintln!("Use 'claude-commander list' to see available sessions.");
                    std::process::exit(1);
                }
            };

            let entry = claude_commander_core::cli::StatusJsonEntry::from_detail(&detail);

            if json {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                println!(
                    "{}",
                    claude_commander_core::cli::format_status_human(&entry)
                );
            }
        }

        Some(Commands::Delete { session, force }) => {
            setup_logging(cli.debug, false)?;

            let service =
                claude_commander_core::api::CommanderService::for_cli(config, frontend())?;

            let info = resolve_session_exact(&service, &session).await?;

            match claude_commander_core::cli::delete_guard(force, std::io::stdin().is_terminal()) {
                claude_commander_core::cli::DeleteGuard::RequireForce => {
                    eprintln!(
                        "Refusing to delete \"{}\" without confirmation. Re-run with --force.",
                        info.title
                    );
                    std::process::exit(1);
                }
                claude_commander_core::cli::DeleteGuard::Prompt => {
                    print!(
                        "Delete \"{}\"? This kills its tmux session and removes the worktree. [y/N] ",
                        info.title
                    );
                    std::io::stdout().flush()?;
                    let mut answer = String::new();
                    std::io::stdin().read_line(&mut answer)?;
                    if !claude_commander_core::cli::parse_yes_no(&answer) {
                        println!("Aborted.");
                        return Ok(());
                    }
                }
                claude_commander_core::cli::DeleteGuard::Proceed => {}
            }

            service.delete_session(&info.session_id).await?;
            println!("Session deleted: {}", info.title);
        }

        Some(Commands::KeepAlive { session, on, off }) => {
            setup_logging(cli.debug, false)?;

            let service =
                claude_commander_core::api::CommanderService::for_cli(config, frontend())?;

            let info = resolve_session_exact(&service, &session).await?;

            // `on`/`off` set explicitly; with neither flag, toggle.
            let keep_alive = if on {
                service.set_keep_alive(&info.session_id, true).await?
            } else if off {
                service.set_keep_alive(&info.session_id, false).await?
            } else {
                service.toggle_keep_alive(&info.session_id).await?
            };
            println!(
                "Keep-alive {} for \"{}\"",
                if keep_alive { "on" } else { "off" },
                info.title
            );
        }

        Some(Commands::Log { session, lines }) => {
            setup_logging(cli.debug, false)?;

            let service =
                claude_commander_core::api::CommanderService::for_cli(config, frontend())?;
            match service.get_pane_content(&session, Some(lines)).await? {
                Some(content) => {
                    if content.ends_with('\n') {
                        print!("{}", content);
                    } else {
                        println!("{}", content);
                    }
                }
                None => {
                    eprintln!("Session not found or has no live tmux session: {}", session);
                    eprintln!("Use 'claude-commander list' to see available sessions.");
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::New {
            name,
            program,
            path,
            project,
            initial_prompt,
            effort,
            mode,
            model,
            base_branch,
            section,
            remote,
            workspace,
        }) => {
            setup_logging(cli.debug, false)?;

            let backend = resolve_cli_backend(config, remote.as_deref())?;

            // Path resolution precedence:
            //   --project → look the name up in the backend's projects (works
            //               for a remote without knowing its server-side path),
            //   --path    → use it verbatim (also seeds a brand-new project),
            //   neither   → the cwd locally; an error for a remote (the cwd
            //               names nothing on the server).
            // `--project` and `--path` are mutually exclusive (clap-enforced).
            let project_path = match (project, path, remote.as_deref()) {
                (Some(name), _, _) => {
                    let snapshot = backend.snapshot().await?;
                    claude_commander_core::session::resolve_project_path(&snapshot.projects, &name)?
                }
                (None, Some(p), _) => p,
                (None, None, None) => std::env::current_dir().unwrap_or_default(),
                (None, None, Some(server)) => {
                    eprintln!(
                        "--path or --project is required with --remote {server} (the project path is resolved on the server, not this machine)."
                    );
                    std::process::exit(2);
                }
            };

            // `--workspace` (clap-bound to `--path`): register the project in that
            // workspace first, so `create_session`'s own ensure finds it. An
            // existing project keeps its workspace — say so rather than
            // silently ignoring the flag.
            if let Some(typed) = workspace.as_deref() {
                let snapshot = backend.snapshot().await?;
                let tag =
                    claude_commander_core::cli::resolve_new_session_workspace(&snapshot, typed);
                let id = match backend
                    .ensure_project(project_path.clone(), tag.clone())
                    .await
                {
                    Ok(id) => id,
                    Err(claude_commander_core::backend::BackendError::InvalidRequest(msg)) => {
                        clap::Error::raw(clap::error::ErrorKind::InvalidValue, format!("{msg}\n"))
                            .exit();
                    }
                    Err(e) => return Err(e.into()),
                };
                if let Some(existing) = snapshot.projects.iter().find(|p| p.id == id)
                    && existing.workspace != tag
                {
                    eprintln!(
                        "Note: project '{}' is already registered in workspace '{}'; it was not moved.",
                        existing.name,
                        existing.workspace.as_deref().unwrap_or("main")
                    );
                }
            }

            match &remote {
                Some(server) => println!("Creating session '{name}' on remote '{server}'..."),
                None => println!("Creating session '{name}'..."),
            }
            let session_id = match backend
                .create_session(claude_commander_core::api::CreateSessionOpts {
                    project_path,
                    title: name,
                    program,
                    initial_prompt,
                    effort,
                    mode,
                    model,
                    base_branch,
                    section,
                    stack_parent: None,
                })
                .await
            {
                Ok(id) => id,
                // Bad input (invalid program flags/name) is a usage error, not a
                // transport failure — surface it the way clap would, whether it
                // came from the local backend or a remote server's 400.
                Err(claude_commander_core::backend::BackendError::InvalidRequest(msg)) => {
                    clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, format!("{msg}\n"))
                        .exit();
                }
                Err(e) => return Err(e.into()),
            };

            println!("Session created: {}", session_id);
            println!();
            match &remote {
                Some(server) => {
                    println!("Attach with: claude-commander attach --remote {server} {session_id}")
                }
                None => println!("Attach with: claude-commander attach {}", session_id),
            }
        }

        Some(Commands::Attach { session, remote }) => {
            setup_logging(cli.debug, false)?;

            let triggers = claude_commander_core::editor_trigger_bytes(&config.keybindings);

            match remote {
                Some(name) => {
                    let server = config.find_remote_server(&name)?.clone();
                    let backend = build_remote_backend(&server)?;
                    execute_remote_attach(backend, &session, triggers).await;
                }
                None => {
                    let app_state = AppState::load_or_exit();

                    match claude_commander_core::session::find_session(&app_state, &session) {
                        Some(s) => {
                            let tmux_name = s.tmux_session_name.clone();
                            execute_attach(&tmux_name, triggers).await;
                        }
                        None => {
                            eprintln!("Session not found: {}", session);
                            eprintln!("Use 'claude-commander list' to see available sessions.");
                        }
                    }
                }
            }
        }

        Some(Commands::Commander) => {
            setup_logging(cli.debug, false)?;

            // One-shot CLI process: a bare executor is sufficient (no shared
            // semaphore to honour as there is in the long-lived TUI). The
            // enable gate lives in `ensure_session`, so the disabled case
            // surfaces as a typed error rather than an inline check here.
            let tmux = claude_commander_core::tmux::TmuxExecutor::new();
            match claude_commander_core::commander::ensure_session(&config, &tmux, &cli_reference())
                .await
            {
                Ok(name) => {
                    let triggers = claude_commander_core::editor_trigger_bytes(&config.keybindings);
                    execute_attach(&name, triggers).await;
                }
                Err(
                    e @ claude_commander_core::Error::Session(
                        claude_commander_core::error::SessionError::CommanderDisabled,
                    ),
                ) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
                Err(e) => return Err(e.into()),
            }
        }

        Some(Commands::ListenToggle { start, stop }) => {
            setup_logging(cli.debug, false)?;

            use claude_commander_core::conversation::{ListenAction, ipc};
            let action = if start {
                ListenAction::Start
            } else if stop {
                ListenAction::Stop
            } else {
                ListenAction::Toggle
            };
            match ipc::send_default(action).await {
                Ok(reply) => println!("{reply}"),
                Err(e) => {
                    eprintln!(
                        "Could not reach claude-commander: {e}\n\
                         Is the TUI running with STT enabled?"
                    );
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Review { command }) => {
            setup_logging(cli.debug, false)?;
            execute_review(config, command).await?;
        }

        Some(Commands::Config { init }) => {
            setup_logging(cli.debug, false)?;

            if init {
                config.save()?;
                println!(
                    "Configuration initialized at {:?}",
                    Config::config_file_path()?
                );
            } else {
                println!("Configuration:");
                println!("{}", toml::to_string_pretty(&config)?);
                println!("\nConfig file: {:?}", Config::config_file_path()?);
                println!("Data dir: {:?}", Config::data_dir()?);
                println!("State file: {:?}", Config::state_file_path()?);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_identifies_binary_package() {
        let frontend = frontend();
        // Pin the actual telemetry identity, not just "matches the macro" —
        // this is the string dashboards key on, and it must not silently
        // drift back to the core crate's name (claude-commander-core).
        assert_eq!(frontend.name, "claude-commander");
        assert_eq!(frontend.version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn cli_and_telemetry_report_one_identity() {
        // The CLI tree, the telemetry frontend, and the startup log line all
        // describe the same program, so they must agree — and on this package,
        // not on the library crate behind it.
        assert_eq!(cli_args::cli_command().get_name(), NAME);
        assert_eq!(frontend().name, NAME);
        assert_ne!(NAME, "claude-commander-core");
    }
}
