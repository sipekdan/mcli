//! The launch pipeline: resolve a version, fetch its files, assemble Java
//! arguments, and start the game process.

use crate::arguments::{self, declares_feature, LaunchInputs, QuickPlay};
use crate::assets::sync_assets;
use crate::auth::get_auth;
use crate::cli::ProfileOverrides;
use crate::config::{Paths, Result, CLASSPATH_SEP, DEFAULT_JVM_ARGS};
use crate::files::{fetch_missing, plan_game_files, GamePlan, Kind};
use crate::http::{extract_zip, needs_download};
use crate::profile::{apply_loader, apply_overrides, load_profiles, save_profiles, Profile};
use crate::runtime::ensure_java;
use crate::version::{resolve_alias, resolve_blueprint};
use reqwest::Client;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

pub struct RunOptions {
    pub offline: bool,
    pub server: Option<String>,
    pub port: String,
    pub world: Option<String>,
    pub dry_run: bool,
    pub detach: bool,
}

pub struct GameFiles {
    pub classpath: String,
    pub natives_dir: PathBuf,
    pub game_assets: PathBuf,
    pub log_argument: Option<String>,
}

/// Give a child version its own client jar (see `files::plan_game_files`).
/// A hard link costs no disk space; fall back to copying across devices.
fn link_launch_jar(plan: &GamePlan) -> Result<()> {
    if plan.launch_jar == plan.parent_jar || !needs_download(&plan.launch_jar, None) {
        return Ok(());
    }
    if let Some(dir) = plan.launch_jar.parent() {
        fs::create_dir_all(dir)?;
    }
    if fs::hard_link(&plan.parent_jar, &plan.launch_jar).is_err() {
        fs::copy(&plan.parent_jar, &plan.launch_jar)?;
    }
    Ok(())
}

/// Download whatever is missing or the wrong size, extract natives, and
/// return what the launch needs.
pub async fn download_game_files(
    client: &Client,
    paths: &Paths,
    blueprint: &Value,
    game_dir: Option<&Path>,
) -> Result<GameFiles> {
    let plan = plan_game_files(paths, blueprint)?;
    fetch_missing(client, &plan.files, "libraries").await?;
    link_launch_jar(&plan)?;

    // Pre-1.19 natives come as archives. Re-extracting the few small files
    // each launch keeps the directory in sync with the verified archives.
    for entry in plan.files.iter().filter(|e| e.kind == Kind::Native) {
        extract_zip(&entry.path, &plan.natives_dir, true)?;
    }

    let mut classpath: Vec<String> = plan
        .classpath
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    classpath.push(plan.launch_jar.to_string_lossy().into_owned());

    let game_assets = sync_assets(client, paths, blueprint, game_dir).await?;
    Ok(GameFiles {
        classpath: classpath.join(CLASSPATH_SEP),
        natives_dir: plan.natives_dir,
        game_assets,
        log_argument: plan.log_argument,
    })
}

/// The UUID vanilla Minecraft derives for offline players:
/// a version-3 UUID of `OfflinePlayer:<name>`.
pub fn offline_uuid(username: &str) -> String {
    let mut bytes = md5::compute(format!("OfflinePlayer:{username}")).0;
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Split `host:port` (or bare `host` + the separate `--port`).
fn server_address(server: &str, port: &str) -> (String, String) {
    match server.rsplit_once(':') {
        Some((host, p)) if !host.contains(':') && p.parse::<u16>().is_ok() => {
            (host.to_string(), p.to_string())
        }
        _ => (server.to_string(), port.to_string()),
    }
}

/// JVM options chosen by the user, or the version's recommended defaults
/// when the profile still uses mcli's stock arguments.
fn user_jvm_args(
    profile_args: &str,
    blueprint: &Value,
    inputs: &LaunchInputs,
) -> Result<Vec<String>> {
    if profile_args.trim() == DEFAULT_JVM_ARGS.trim() {
        if let Some(defaults) = blueprint["arguments"].get("default-user-jvm") {
            let mut missing = BTreeSet::new();
            let mut args = arguments::collect(
                defaults,
                &inputs.placeholder_values(),
                &inputs.features(),
                &mut missing,
            );
            arguments::check_missing(&missing)?;
            args.extend(
                [
                    "-Dfile.encoding=UTF-8",
                    "-Dsun.stdout.encoding=UTF-8",
                    "-Dsun.stderr.encoding=UTF-8",
                ]
                .map(String::from),
            );
            return Ok(args);
        }
    }
    Ok(profile_args.split_whitespace().map(String::from).collect())
}

fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=+,@%".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

pub async fn run_game(
    target: String,
    overrides: ProfileOverrides,
    options: RunOptions,
    client: &Client,
    paths: &Paths,
) -> Result<()> {
    let started = Instant::now();
    crate::config::validate_id(&target)?;
    // A new target is treated as both its profile name and its version unless
    // `--version` explicitly says otherwise.
    let mut profiles = load_profiles(paths);
    let is_new = !profiles.contains_key(&target);

    let mut current_profile = profiles
        .get(&target)
        .cloned()
        .unwrap_or_else(|| Profile::new(paths, &target, overrides.version.as_deref()));

    apply_overrides(&mut current_profile, &overrides);
    apply_loader(client, paths, &mut current_profile, &overrides).await?;

    let target_version = resolve_alias(client, paths, &current_profile.version).await?;
    let bp = resolve_blueprint(client, paths, &target_version).await?;

    if is_new {
        profiles.insert(target.clone(), current_profile.clone());
        save_profiles(paths, &profiles)?;
    }

    let auth = if options.offline {
        None
    } else {
        let account = overrides.account.as_deref();
        Some(get_auth(client, paths, account).await.ok_or_else(|| {
            let who = account.map(|a| format!(" for account '{a}'")).unwrap_or_default();
            format!("No valid Minecraft session{who}. Run 'mcli account login', or use --offline for offline play.")
        })?)
    };
    let username = auth
        .as_ref()
        .map(|a| a.username.clone())
        .unwrap_or_else(|| current_profile.username.clone());
    let uuid = match &auth {
        Some(a) => a.uuid.clone(),
        None if current_profile.uuid.is_empty() || current_profile.uuid == "0" => {
            offline_uuid(&username)
        }
        None => current_profile.uuid.clone(),
    };
    let access_token = auth
        .as_ref()
        .map(|a| a.access_token.clone())
        .unwrap_or_else(|| "0".to_string());
    let user_type = if options.offline { "legacy" } else { "msa" };

    println!("=== mcli Launcher ===");
    println!(
        "Profile: {} | Version: {} | User: {} ({})",
        target,
        target_version,
        username,
        if options.offline { "Offline" } else { "Online" }
    );

    let game_dir = PathBuf::from(&current_profile.game_dir);
    fs::create_dir_all(&game_dir)?;

    let game_files = download_game_files(client, paths, &bp, Some(&game_dir)).await?;
    fs::create_dir_all(&game_files.natives_dir)?;
    let java_exe = ensure_java(client, paths, &bp, Some(&current_profile.java_bin)).await?;

    let fullscreen = current_profile.fullscreen;
    let resolution = (!fullscreen
        && current_profile.width != "auto"
        && current_profile.height != "auto")
        .then(|| {
            (
                current_profile.width.clone(),
                current_profile.height.clone(),
            )
        });

    // Prefer the version's own quick-play arguments (1.20+); older versions
    // only understand the legacy `--server`/`--port` pair.
    let mut extra_game_args: Vec<String> = Vec::new();
    let quick_play = if let Some(world) = &options.world {
        if !declares_feature(&bp, "is_quick_play_singleplayer") {
            return Err(format!(
                "--world needs Minecraft 1.20 or newer ({target_version} has no quick play)"
            )
            .into());
        }
        Some(QuickPlay::Singleplayer(world.clone()))
    } else if let Some(server) = &options.server {
        let (host, port) = server_address(server, &options.port);
        if declares_feature(&bp, "is_quick_play_multiplayer") {
            Some(QuickPlay::Multiplayer(format!("{host}:{port}")))
        } else {
            extra_game_args.extend(["--server".into(), host, "--port".into(), port]);
            None
        }
    } else {
        None
    };
    if let Some((w, h)) = &resolution {
        if !declares_feature(&bp, "has_custom_resolution") {
            extra_game_args.extend(["--width".into(), w.clone(), "--height".into(), h.clone()]);
        }
    }
    if fullscreen {
        extra_game_args.push("--fullscreen".into());
    }

    let asset_index = bp["assetIndex"]["id"]
        .as_str()
        .or_else(|| bp["assets"].as_str())
        .unwrap_or("legacy")
        .to_string();
    let inputs = LaunchInputs {
        username: username.clone(),
        uuid,
        access_token: access_token.clone(),
        user_type: user_type.to_string(),
        // Only used for telemetry by the game; mcli deliberately sends none.
        xuid: "0".to_string(),
        client_id: "0".to_string(),
        version_name: target_version.clone(),
        version_type: bp["type"].as_str().unwrap_or("release").to_string(),
        game_dir: game_dir.clone(),
        assets_root: paths.assets.clone(),
        game_assets: game_files.game_assets,
        asset_index,
        natives_dir: game_files.natives_dir,
        library_dir: paths.libraries.clone(),
        classpath: game_files.classpath,
        resolution,
        quick_play,
        demo: false,
    };
    let launch_args = arguments::build(&bp, &inputs)?;

    // Construct the Java command only after all downloadable prerequisites are
    // present. This avoids leaving a child process running after a failed sync.
    let mut all_args: Vec<String> = user_jvm_args(&current_profile.jvm_args, &bp, &inputs)?;

    // Mojang's log4j configuration (includes security fixes for old versions).
    all_args.extend(game_files.log_argument);

    all_args.extend(launch_args.jvm);
    all_args.push(
        bp["mainClass"]
            .as_str()
            .ok_or("Version metadata has no mainClass")?
            .to_string(),
    );
    all_args.extend(launch_args.game);
    all_args.extend(extra_game_args);

    if options.dry_run {
        let shown: Vec<String> = std::iter::once(java_exe.as_str())
            .chain(all_args.iter().map(String::as_str))
            .map(|a| {
                if a == access_token && access_token != "0" {
                    "<access-token>".to_string()
                } else {
                    shell_quote(a)
                }
            })
            .collect();
        println!("\n# working directory: {}", game_dir.display());
        println!("{}", shown.join(" "));
        return Ok(());
    }

    let mut cmd = Command::new(&java_exe);
    cmd.args(&all_args).current_dir(&game_dir);

    println!(
        "\n[!] Launching Minecraft {} (ready in {:.2}s)...",
        target_version,
        started.elapsed().as_secs_f64()
    );

    if options.detach {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            // Own process group: closing the terminal or Ctrl+C won't kill the game.
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        let child = cmd.spawn()?;
        println!(
            "[+] Started (pid {}). Logs: {}",
            child.id(),
            game_dir.join("logs").join("latest.log").display()
        );
        return Ok(());
    }

    let status = cmd.spawn()?.wait()?;
    if !status.success() {
        return Err(format!(
            "Minecraft exited with {status}. Check {} for details.\n\
             If it crashed on startup, damaged game files are a common cause: mcli verify {target}",
            game_dir.join("logs").join("latest.log").display()
        )
        .into());
    }
    println!("\n[!] Game closed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_uuid_matches_vanilla() {
        // Java: UUID.nameUUIDFromBytes("OfflinePlayer:Notch".getBytes(UTF_8))
        assert_eq!(offline_uuid("Notch"), "b50ad385829d3141a2167e7d7539ba7f");
    }

    #[test]
    fn parses_server_addresses() {
        assert_eq!(
            server_address("example.org", "25565"),
            ("example.org".into(), "25565".into())
        );
        assert_eq!(
            server_address("example.org:1234", "25565"),
            ("example.org".into(), "1234".into())
        );
        // A bare IPv6 address is not mistaken for host:port.
        assert_eq!(
            server_address("::1", "25565"),
            ("::1".into(), "25565".into())
        );
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("-Xmx2G"), "-Xmx2G");
        assert_eq!(
            shell_quote("-DFabricMcEmu= net.minecraft.client.main.Main "),
            "'-DFabricMcEmu= net.minecraft.client.main.Main '"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
