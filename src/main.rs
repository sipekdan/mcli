//! Program entry point: parse a command, create shared services, and dispatch it.

mod arguments;
mod assets;
mod auth;
mod cli;
mod config;
mod files;
mod http;
mod launcher;
mod loader;
mod profile;
mod rules;
mod runtime;
mod verify;
mod version;

use clap::Parser;
use cli::{Cli, Commands};
use config::{Paths, Result};
use http::build_http_client;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
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
            world,
            dry_run,
            detach,
        } => {
            let options = launcher::RunOptions {
                offline,
                server,
                port,
                world,
                dry_run,
                detach,
            };
            launcher::run_game(target, overrides, options, &client, &paths).await?;
        }
        Commands::Profile { action } => {
            profile::handle_profile_cmd(action, &client, &paths).await?;
        }
        Commands::Version { action } => {
            version::handle_version_cmd(action, &client, &paths).await?;
        }
        Commands::Account { action } => {
            auth::handle_account_cmd(action, &client, &paths).await?;
        }
        Commands::Verify { target } => {
            verify::verify(&target, &client, &paths).await?;
        }
        Commands::Server { version, output } => {
            version::download_server_jar(&client, &paths, &version, output).await?;
        }
    }

    Ok(())
}
