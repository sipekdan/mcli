//! Program entry point: parse a command, create shared services, and dispatch it.

mod assets;
mod auth;
mod cli;
mod config;
mod http;
mod launcher;
mod profile;
mod runtime;
mod version;

use clap::Parser;
use cli::{Cli, Commands};
use config::{Paths, Result};
use http::build_http_client;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new();
    let client = build_http_client()?;

    match cli.command {
        Commands::Run {
            target,
            overrides,
            offline,
            server,
            port,
        } => {
            launcher::run_game(target, overrides, offline, server, port, &client, &paths).await?;
        }
        Commands::Profile { action } => {
            profile::handle_profile_cmd(action, &paths)?;
        }
        Commands::Version { action } => {
            version::handle_version_cmd(action, &client, &paths).await?;
        }
        Commands::Account { action } => {
            auth::handle_account_cmd(action, &client, &paths).await?;
        }
        Commands::Server { version, output } => {
            version::download_server_jar(&client, &paths, &version, output).await?;
        }
    }

    Ok(())
}
