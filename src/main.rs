//! Claude Commander - Terminal UI for managing Claude coding sessions
//!
//! Run with `claude-commander` or `claude-commander --help` for usage.

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use claude_commander::{
    APP_NAME, VERSION,
    config::{AppState, Config, ConfigStore, StateStore},
    tmux::{AttachResult, attach_to_session},
    tui::App,
};

#[derive(Parser)]
#[command(name = APP_NAME)]
#[command(version = VERSION)]
#[command(about = "A high-performance terminal UI for managing Claude coding sessions")]
#[command(long_about = None)]
struct Cli {
    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Path to config file
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the interactive TUI (default)
    Tui,

    /// List all sessions
    List {
        /// Show all sessions including stopped ones
        #[arg(short, long)]
        all: bool,
    },

    /// Create a new session
    New {
        /// Session name
        name: String,

        /// Program to run (default: claude)
        #[arg(short, long)]
        program: Option<String>,

        /// Project path (default: current directory)
        #[arg(short = 'd', long)]
        path: Option<std::path::PathBuf>,
    },

    /// Attach to an existing session
    Attach {
        /// Session name or ID
        session: String,
    },

    /// Show configuration
    Config {
        /// Initialize config file with defaults
        #[arg(long)]
        init: bool,
    },
}

fn setup_logging(debug: bool, to_file: bool) -> Result<()> {
    let filter = if debug {
        EnvFilter::new("debug")
    } else {
        // Use info level for our crate, warn for dependencies
        EnvFilter::new("info")
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

/// Execute async PTY-based attach to a tmux session
async fn execute_attach(session_name: &str, editor_triggers: Vec<Vec<u8>>) {
    // CLI `attach` resolves a Claude session by title/ID, never a shell.
    match attach_to_session(session_name, editor_triggers, true).await {
        Ok(AttachResult::Detached | AttachResult::SwitchToShell | AttachResult::OpenEditor) => {
            info!("Detached from session");
        }
        Ok(AttachResult::SessionEnded) => {
            info!("Session ended");
        }
        Ok(AttachResult::Error(e)) => {
            eprintln!("Attach error: {}", e);
        }
        Err(e) => {
            eprintln!("Failed to attach: {}", e);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install color-eyre error hooks
    color_eyre::install()?;

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

            let config_store = std::sync::Arc::new(ConfigStore::new(config.clone())?);
            let app_state = AppState::load().unwrap_or_else(|_| AppState::new());
            let store = std::sync::Arc::new(StateStore::new(app_state)?);
            let mut app = App::new(config_store, store);
            app.run().await?;
        }

        Some(Commands::List { all }) => {
            setup_logging(cli.debug, false)?;

            let app_state = AppState::load().unwrap_or_else(|_| AppState::new());

            println!("Sessions:");
            println!();

            if app_state.projects.is_empty() {
                println!("  No projects. Use 'claude-commander' to add one.");
                return Ok(());
            }

            for project in app_state.projects.values() {
                println!("  {} ({})", project.name, project.main_branch);

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
                            claude_commander::SessionStatus::Creating
                            | claude_commander::SessionStatus::Merging
                            | claude_commander::SessionStatus::Pushing => "⠋",
                            claude_commander::SessionStatus::Running => "●",
                            claude_commander::SessionStatus::Stopped => "○",
                            claude_commander::SessionStatus::CascadePaused => "⏸",
                        };
                        match claude_commander::session::display_branch(
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

        Some(Commands::New {
            name,
            program,
            path,
        }) => {
            setup_logging(cli.debug, false)?;

            let path = path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            use claude_commander::session::SessionManager;
            use claude_commander::tui::theme::Theme;
            use std::sync::Arc;

            let config_store = Arc::new(ConfigStore::new(config)?);
            let app_state = AppState::load().unwrap_or_else(|_| AppState::new());
            let store = Arc::new(StateStore::new(app_state)?);
            let manager = SessionManager::new(
                config_store,
                store.clone(),
                Theme::default().tmux_status_style(),
            );

            // Check tmux
            manager.check_tmux().await?;

            // First, try to find or add the project
            let project_id = {
                let state = store.read().await;
                state
                    .projects
                    .values()
                    .find(|p| p.repo_path == path)
                    .map(|p| p.id)
            };

            let project_id = match project_id {
                Some(id) => id,
                None => {
                    println!("Adding project from {:?}...", path);
                    manager.add_project(path).await?
                }
            };

            println!("Creating session '{}'...", name);
            let session_id = manager
                .prepare_session(&project_id, name, program, None)
                .await?;
            manager.finalize_session(&session_id).await?;

            println!("Session created: {}", session_id);
            println!();
            println!("Attach with: claude-commander attach {}", session_id);
        }

        Some(Commands::Attach { session }) => {
            setup_logging(cli.debug, false)?;

            let app_state = AppState::load().unwrap_or_else(|_| AppState::new());

            // Find session by name or ID prefix
            let tmux_name = app_state
                .sessions
                .iter()
                .find(|(id, s)| {
                    s.title.to_lowercase() == session.to_lowercase()
                        || id.to_string().starts_with(&session)
                })
                .map(|(_, s)| s.tmux_session_name.clone());

            match tmux_name {
                Some(name) => {
                    let triggers = claude_commander::editor_trigger_bytes(&config.keybindings);
                    execute_attach(&name, triggers).await;
                }
                None => {
                    eprintln!("Session not found: {}", session);
                    eprintln!("Use 'claude-commander list' to see available sessions.");
                }
            }
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
