//! `claude-commander-server` — exposes a local `CommanderService` over HTTP +
//! WebSocket so clients on other machines can drive Commander sessions.
//!
//! This binary is one of two frontends over the same library: the TUI embeds the
//! server in-process (see [`claude_commander_server::embed`]) when
//! `[server] auto_start` is on. Everything worth testing — the token policy, the
//! bind — lives in the library; this file resolves flags and logs.

use clap::Parser;
use claude_commander_core::api::{BackgroundOpts, CommanderService};
use claude_commander_core::config::ServerConfig;
use claude_commander_core::telemetry::FrontendInfo;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use claude_commander_server::auth::AuthConfig;
use claude_commander_server::config::{check_no_auth_bind, resolve};
use claude_commander_server::embed::{self, TokenDecision};

/// Identify this binary to the telemetry layer (required by `CommanderService`).
fn frontend() -> FrontendInfo {
    FrontendInfo::new("claude-commander-server", env!("CARGO_PKG_VERSION"))
}

#[derive(Parser, Debug)]
#[command(name = "claude-commander-server", version, about)]
struct Cli {
    /// Interface to bind (overrides config). Defaults to 127.0.0.1.
    #[arg(long)]
    bind: Option<std::net::IpAddr>,

    /// Port to listen on (overrides config). Defaults to 7878.
    #[arg(long)]
    port: Option<u16>,

    /// Pre-shared bearer token (overrides config / auto-generation).
    #[arg(long)]
    token: Option<String>,

    /// Disable authentication entirely. Loopback-only dev convenience —
    /// never use on a non-loopback bind.
    #[arg(long)]
    allow_no_auth: bool,

    /// Enable TLS (requires the `tls` build feature and cert/key paths in config).
    #[arg(long)]
    tls: bool,

    /// Verbose (debug-level) logging.
    #[arg(long)]
    debug: bool,
}

fn setup_logging(debug: bool) {
    let filter = if debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("claude_commander_server=info,claude_commander_core=info,warn")
        })
    };
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

/// Layer this binary's CLI flags over the `[server]` table core loaded from
/// `config.toml` (and the `CC_SERVER_*` environment overrides [`resolve`] adds).
fn resolve_config(
    base: ServerConfig,
    cli: &Cli,
) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let mut cfg = resolve(base)?;
    if let Some(bind) = cli.bind {
        cfg.bind = bind;
    }
    if let Some(port) = cli.port {
        cfg.port = port;
    }
    if let Some(token) = &cli.token {
        cfg.token = Some(token.clone());
    }
    Ok(cfg)
}

/// Resolve the authentication policy from flags + config.
///
/// Precedence: `--allow-no-auth` disables auth; otherwise a token from
/// CLI/config is used; otherwise a fresh random token is generated, logged
/// once, and used (secure-by-default on a fresh install). The token value is
/// only logged when it was auto-generated (so the operator can copy it); a
/// configured token is never logged.
///
/// Unlike the TUI's embedded server, this binary does **not** persist a
/// generated token: a `systemd`-style deployment's config file may well be
/// read-only or managed, and an operator running the binary by hand can read the
/// token off the terminal. Set `[server] token` to make it stable.
fn resolve_auth(cfg: &ServerConfig, allow_no_auth: bool) -> AuthConfig {
    if allow_no_auth {
        warn!("authentication disabled (--allow-no-auth); only safe on a loopback bind");
        return AuthConfig::Disabled;
    }
    match embed::token_decision(cfg) {
        TokenDecision::Existing(token) => AuthConfig::Token(token),
        TokenDecision::Generated(token) => {
            info!("no token configured; generated a one-time bearer token for this run: {token}");
            info!("set `[server] token` in config.toml (or pass --token) to persist it");
            AuthConfig::Token(token)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    setup_logging(cli.debug);

    let config = claude_commander_core::Config::load()?;
    let cfg = resolve_config(config.server.clone(), &cli)?;

    // Hard error (not just a warning) if --allow-no-auth is used on a
    // non-loopback bind: that would expose an unauthenticated API to the
    // network. `embed::start` enforces this too; checking here as well means
    // the operator gets the message on stderr before anything else runs.
    check_no_auth_bind(cfg.bind, cli.allow_no_auth)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let auth = resolve_auth(&cfg, cli.allow_no_auth);

    if cli.tls {
        warn!("--tls requested; TLS support requires the `tls` build feature (not yet wired)");
    }

    let commander_enabled = config.commander_enabled;
    let service = CommanderService::for_cli(config, frontend())?;
    // Drive the same background loops the local TUI runs (agent-state polling,
    // PR-status checks, project auto-pull, state-sync) so remote clients see live
    // data via `/workspace` + `/agent-states` polls. Handles run for the process
    // lifetime; we don't need to hold them. `embed::start` deliberately starts
    // none of this, because when the TUI embeds it these are already running.
    let _background = service.spawn_background_tasks(BackgroundOpts { commander_enabled });
    // The server is a long-lived frontend, so drive the idle-hibernation loop
    // (no-op unless hibernate_enabled and the check interval is non-zero), just
    // as the TUI does. Without this a server-only deployment — the many-idle-
    // sessions case hibernation targets — would never hibernate.
    service.start_hibernation_loop();
    // List any recognised agent harness installed on this host that the
    // program picker doesn't offer yet, as the TUI does at startup.
    if let Err(e) = service.discover_programs() {
        warn!("program discovery failed: {e}");
    }

    let server = embed::start(service, &cfg, auth).await?;
    info!("claude-commander-server listening on {}", server.url());

    server.join().await;
    Ok(())
}
