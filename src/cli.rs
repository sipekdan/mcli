//! Command-line interface definitions.
//!
//! This module intentionally only describes user input. Command behavior lives in
//! the matching `profile`, `version`, `auth`, and `launcher` modules.

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mcli", version, about = "Minimalist CLI Minecraft Launcher")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Launch a profile or Minecraft version
    Run {
        target: String,
        #[command(flatten)]
        overrides: ProfileOverrides,
        #[arg(long, help = "Force offline mode even if logged in")]
        offline: bool,
        #[arg(long, help = "Auto-connect to server IP")]
        server: Option<String>,
        #[arg(long, default_value = "25565", help = "Server port")]
        port: String,
    },
    /// Manage profiles (list, create, edit, rename, remove)
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Manage game versions (list, install, uninstall, server)
    Version {
        #[command(subcommand)]
        action: VersionAction,
    },
    /// Manage Microsoft accounts (list, login, switch, logout)
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Shortcut to download the official server.jar for a version
    Server {
        version: String,
        #[arg(short, long, help = "Output file path")]
        output: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ProfileAction {
    /// List all saved profiles
    List,
    /// Create a new profile
    Create {
        name: String,
        #[command(flatten)]
        overrides: ProfileOverrides,
    },
    /// Edit an existing profile's saved settings
    Edit {
        name: String,
        #[command(flatten)]
        overrides: ProfileOverrides,
    },
    /// Rename a profile
    Rename { name: String, new_name: String },
    /// Remove a profile (pass --purge to also delete saves/files)
    Remove {
        name: String,
        #[arg(long, help = "Also delete the profile's game directory & saves")]
        purge: bool,
    },
}

#[derive(Subcommand)]
pub enum VersionAction {
    /// List installed and available Mojang versions
    List {
        #[arg(long, help = "Include snapshots in version list")]
        snapshots: bool,
        #[arg(long, help = "Only show locally installed versions")]
        local: bool,
    },
    /// Download a version's files and Java runtime without launching
    Install { version: String },
    /// Remove a locally installed version to free disk space
    Uninstall { version: String },
    /// Download the official server.jar for a version
    Server {
        version: String,
        #[arg(short, long, help = "Output file path")]
        output: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum AccountAction {
    /// List saved Microsoft accounts
    List,
    /// Log in via Microsoft Device Code
    Login,
    /// Switch the active Microsoft account
    Switch { name: String },
    /// Log out of the active account (or --all)
    Logout {
        #[arg(long, help = "Remove all saved accounts")]
        all: bool,
    },
}

#[derive(Args, Clone, Default)]
pub struct ProfileOverrides {
    // These are optional so `run` can temporarily override a saved profile.
    // `profile create` and `profile edit` persist the same values instead.
    #[arg(long, help = "Minecraft version (e.g., 1.20.4)")]
    pub version: Option<String>,
    #[arg(long, help = "Custom game directory path")]
    pub game_dir: Option<String>,
    #[arg(long, help = "Shortcut to set RAM in JVM args (e.g., 4G)")]
    pub ram: Option<String>,
    #[arg(long, help = "Full custom JVM arguments string")]
    pub jvm_args: Option<String>,
    #[arg(long, help = "Path to custom Java executable (or 'bundled')")]
    pub java_bin: Option<String>,
    #[arg(long, help = "Window width (or 'auto')")]
    pub width: Option<String>,
    #[arg(long, help = "Window height (or 'auto')")]
    pub height: Option<String>,
    #[arg(long, help = "Launch in fullscreen mode")]
    pub fullscreen: bool,
    #[arg(long, help = "Skip asset verification for faster launch")]
    pub fast: bool,
    #[arg(long, help = "Account username to use")]
    pub account: Option<String>,
    #[arg(long, help = "Offline username override")]
    pub username: Option<String>,
    #[arg(long, help = "Offline UUID override")]
    pub uuid: Option<String>,
}
