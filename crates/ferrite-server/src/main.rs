use clap::{Parser, Subcommand};
use ferrite_core::{Config, Core};
use ferrite_server::AppState;
use ferrite_server::tools::{McpHandler, UserCtx};
use std::path::PathBuf;

/// Ferrite: a fast, self-hosted collaboration server — files, Talk, boards, live documents,
/// the ArtCraft Crafting Apps, and MCP-native AI.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Configuration file (TOML). Environment variables FERRITE_* override it.
    #[arg(long, short, global = true, env = "FERRITE_CONFIG", default_value = "ferrite.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (default).
    Serve,
    /// Create a user.
    UserAdd {
        username: String,
        #[arg(long)]
        password: String,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        admin: bool,
    },
    /// Reset a user's password.
    Passwd {
        username: String,
        #[arg(long)]
        password: String,
    },
    /// Create an app token (for MCP clients, WebDAV, scripts).
    Token {
        username: String,
        #[arg(long, default_value = "cli")]
        name: String,
        /// Comma separated scopes (files,talk,boards,docs,groupware,craft,ai,mcp,admin or *).
        #[arg(long, default_value = "*")]
        scopes: String,
    },
    /// Serve MCP over stdio as a user (for local MCP clients that launch a command).
    Mcp {
        #[arg(long)]
        user: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tune_allocator();
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve);
    // Logs go to stderr so `ferrite mcp` keeps stdout clean for the protocol.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,tower_http=warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let config = Config::load(Some(&cli.config))?;
    let core = Core::open(config).await?;
    match command {
        Command::Serve => serve(core).await,
        Command::UserAdd {
            username,
            password,
            display_name,
            admin,
        } => {
            let u = core
                .create_user(ferrite_core::auth::NewUser {
                    username,
                    display_name,
                    email: None,
                    password,
                    is_admin: admin,
                    quota_bytes: None,
                })
                .await?;
            println!("created {} ({})", u.username, u.id);
            Ok(())
        }
        Command::Passwd { username, password } => {
            let u = core.user_by_name(&username).await?;
            core.update_user(
                &u.id,
                ferrite_core::auth::UserUpdate {
                    password: Some(password),
                    ..Default::default()
                },
            )
            .await?;
            println!("password updated");
            Ok(())
        }
        Command::Token { username, name, scopes } => {
            let u = core.user_by_name(&username).await?;
            let (_, secret) = core.create_token(&u.id, "app", &name, &scopes, None).await?;
            println!("{secret}");
            Ok(())
        }
        Command::Mcp { user } => {
            let u = core.user_by_name(&user).await?;
            let state = AppState::new(core);
            let server = ferrite_mcp::Server::new(McpHandler { tools: state.tools.clone() });
            let ctx = UserCtx { user: u, scopes: "*".into() };
            server.serve_stream(&ctx, tokio::io::stdin(), tokio::io::stdout()).await?;
            Ok(())
        }
    }
}

async fn serve(core: Core) -> anyhow::Result<()> {
    let listen = core.config.listen.clone();
    let state = AppState::new(core.clone());
    state.start_background();
    let app = ferrite_server::router(state);
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!("Ferrite listening on http://{listen} (public URL {})", core.config.public_url);
    if core.user_count().await? == 0 {
        tracing::info!("no users yet: open the web UI to create the administrator account");
    }
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    core.flush_all_sessions().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

/// glibc raises its mmap threshold after large blocks are freed, so the 19 MiB working
/// buffer of every argon2 password hash would then stay on the heap for good. A fixed
/// threshold keeps large, short-lived buffers in mmap, returning them to the OS on free.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_allocator() {
    // SAFETY: mallopt only adjusts allocator parameters; it is safe to call at any time.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_allocator() {}
