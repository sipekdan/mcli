//! The launch pipeline: resolve a version, fetch its files, assemble Java
//! arguments, and start the game process.

use crate::assets::sync_assets;
use crate::auth::get_auth;
use crate::cli::ProfileOverrides;
use crate::config::{Paths, Result, CLASSPATH_SEP, MOJANG_OS};
use crate::http::{download_file, extract_zip};
use crate::profile::{apply_overrides, load_profiles, save_profiles, Profile};
use crate::runtime::ensure_java;
use crate::version::{maven_to_path, resolve_blueprint};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub async fn download_game_files(
    client: &Client,
    paths: &Paths,
    blueprint: &Value,
    game_dir: Option<&Path>,
    fast: bool,
) -> Result<(String, PathBuf, PathBuf)> {
    // `_jar_version` is the base version that owns the client JAR. A modded
    // child profile may have a different visible version ID.
    let jar_version = blueprint["_jar_version"].as_str().unwrap();
    let client_jar_path = paths
        .versions
        .join(jar_version)
        .join(format!("{}.jar", jar_version));

    if let Some(url) = blueprint["downloads"]["client"]["url"].as_str() {
        download_file(client, url, &client_jar_path, true).await?;
    }

    let natives_dir = paths
        .versions
        .join(jar_version)
        .join(format!("natives-{}", MOJANG_OS));
    let mut classpath_jars: Vec<String> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut seen_artifacts: HashSet<String> = HashSet::new();
    // `(URL, destination, is_native_archive)`. Build this list first so
    // independent library downloads can run concurrently below.
    let mut downloads: Vec<(String, PathBuf, bool)> = Vec::new();
    let mut scheduled_paths: HashSet<PathBuf> = HashSet::new();

    println!("[!] Processing libraries and natives...");
    if let Some(libs) = blueprint["libraries"].as_array() {
        for lib in libs {
            // Mojang metadata can include libraries for a different OS.
            if let Some(rules) = lib["rules"].as_array() {
                let mut allowed = false;
                for rule in rules {
                    let action = rule["action"].as_str().unwrap_or("");
                    let rule_os = rule["os"]["name"].as_str();
                    if action == "allow" && (rule_os == Some(MOJANG_OS) || rule_os.is_none()) {
                        allowed = true;
                    } else if action == "disallow" && rule_os == Some(MOJANG_OS) {
                        allowed = false;
                    }
                }
                if !allowed {
                    continue;
                }
            }

            // A child version may repeat a parent's Maven artifact. Keep the
            // first matching coordinate in the classpath.
            if let Some(coord) = lib["name"].as_str() {
                let parts: Vec<&str> = coord.split(':').collect();
                if parts.len() >= 2 && lib.get("natives").is_none() {
                    let classifier = parts.get(3).copied().unwrap_or("");
                    let key = format!("{}:{}:{}", parts[0], parts[1], classifier);
                    if !seen_artifacts.insert(key) {
                        continue;
                    }
                }
            }

            // Modern metadata gives an exact artifact URL. Older metadata has
            // no `downloads` object, so derive its Maven repository path.
            if let Some(artifact) = lib.get("downloads").and_then(|d| d.get("artifact")) {
                if let (Some(url), Some(p)) = (artifact["url"].as_str(), artifact["path"].as_str())
                {
                    crate::config::validate_relative_path(p)?;
                    let lib_path = paths.libraries.join(p);
                    let s = lib_path.to_string_lossy().to_string();
                    if seen_paths.insert(s.clone()) {
                        classpath_jars.push(s);
                    }
                    if scheduled_paths.insert(lib_path.clone()) && !lib_path.exists() {
                        downloads.push((url.to_string(), lib_path, false));
                    }
                }
            } else if lib.get("downloads").is_none() {
                let Some(name) = lib["name"].as_str() else {
                    continue;
                };
                let rel_path = maven_to_path(name)
                    .ok_or_else(|| format!("Invalid Maven library coordinate: '{name}'"))?;
                crate::config::validate_relative_path(&rel_path)?;
                let mut base_url = lib["url"]
                    .as_str()
                    .unwrap_or("https://libraries.minecraft.net/")
                    .to_string();
                if !base_url.ends_with('/') {
                    base_url.push('/');
                }
                let lib_path = paths.libraries.join(&rel_path);
                let s = lib_path.to_string_lossy().to_string();
                if seen_paths.insert(s.clone()) {
                    classpath_jars.push(s);
                }
                if scheduled_paths.insert(lib_path.clone()) && !lib_path.exists() {
                    downloads.push((base_url + &rel_path, lib_path, false));
                }
            }

            // Native classifier names can include `${arch}`. Download and
            // extract the platform-specific archive separately from classpath jars.
            if let Some(nat_tmpl) = lib.get("natives").and_then(|n| n[MOJANG_OS].as_str()) {
                let arch_bits = if cfg!(target_pointer_width = "32") {
                    "32"
                } else {
                    "64"
                };
                let key = nat_tmpl.replace("${arch}", arch_bits);
                if let Some(nat_info) = lib
                    .get("downloads")
                    .and_then(|d| d.get("classifiers"))
                    .and_then(|c| c.get(&key))
                {
                    if let (Some(url), Some(p)) =
                        (nat_info["url"].as_str(), nat_info["path"].as_str())
                    {
                        crate::config::validate_relative_path(p)?;
                        let nat_jar = paths.libraries.join(p);
                        if scheduled_paths.insert(nat_jar.clone()) {
                            if nat_jar.exists() {
                                extract_zip(&nat_jar, &natives_dir, true)?;
                            } else {
                                downloads.push((url.to_string(), nat_jar, true));
                            }
                        }
                    }
                }
            }
        }
    }

    if !downloads.is_empty() {
        println!("Downloading {} libraries...", downloads.len());
        let progress = ProgressBar::new(downloads.len() as u64);
        progress.set_style(ProgressStyle::with_template(
            "  {spinner:.green} {wide_bar:.green/white} {pos}/{len} • {eta}",
        )?);
        progress.enable_steady_tick(std::time::Duration::from_millis(100));

        // Downloading libraries is network-bound; cap concurrency just as with
        // assets while keeping extraction itself serialized.
        let mut stream = stream::iter(downloads)
            .map(|(url, path, is_native)| {
                let client = client.clone();
                async move {
                    download_file(&client, &url, &path, true).await?;
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>((path, is_native))
                }
            })
            .buffer_unordered(16);

        while let Some(result) = stream.next().await {
            let (path, is_native) = result?;
            if is_native {
                extract_zip(&path, &natives_dir, true)?;
            }
            progress.inc(1);
        }
        progress.finish_with_message("libraries ready");
    }

    let client_str = client_jar_path.to_string_lossy().to_string();
    if seen_paths.insert(client_str.clone()) {
        classpath_jars.push(client_str);
    }

    let active_assets = sync_assets(client, paths, blueprint, game_dir, fast).await?;
    Ok((
        classpath_jars.join(CLASSPATH_SEP),
        natives_dir,
        active_assets,
    ))
}

#[allow(clippy::too_many_arguments)]
fn legacy_arguments(
    blueprint: &Value,
    username: &str,
    uuid: &str,
    access_token: &str,
    user_type: &str,
    game_dir: &Path,
    assets_dir: &Path,
    version: &str,
) -> Option<Vec<String>> {
    // Pre-1.13 metadata stores one placeholder-filled argument string instead
    // of the modern structured argument array.
    let args = blueprint["minecraftArguments"].as_str()?;
    Some(
        args.split_whitespace()
            .map(|arg| {
                arg.replace("${auth_player_name}", username)
                    .replace("${auth_uuid}", uuid)
                    .replace("${auth_session}", access_token)
                    .replace("${auth_access_token}", access_token)
                    .replace("${game_directory}", &game_dir.to_string_lossy())
                    .replace("${game_assets}", &assets_dir.to_string_lossy())
                    .replace("${version_name}", version)
                    .replace("${user_type}", user_type)
                    .replace("${user_properties}", "{}")
            })
            .collect(),
    )
}

pub async fn run_game(
    target: String,
    overrides: ProfileOverrides,
    offline: bool,
    server: Option<String>,
    port: String,
    client: &Client,
    paths: &Paths,
) -> Result<()> {
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

    let target_version = current_profile.version.clone();
    let bp = resolve_blueprint(client, paths, &target_version).await?;

    if is_new {
        profiles.insert(target.clone(), current_profile.clone());
        save_profiles(paths, &profiles)?;
    }

    let jar_version = bp["_jar_version"].as_str().unwrap_or(&target_version);

    let auth = if offline {
        None
    } else {
        Some(get_auth(client, paths, overrides.account.as_deref()).await
            .ok_or("No valid Minecraft session. Run 'mcli account login', or use --offline for offline play.")?)
    };
    let username = auth
        .as_ref()
        .map(|a| &a.username)
        .unwrap_or(&current_profile.username);
    let uuid = auth
        .as_ref()
        .map(|a| &a.uuid)
        .unwrap_or(&current_profile.uuid);
    let access_token = auth
        .as_ref()
        .map(|a| a.access_token.as_str())
        .unwrap_or("0");
    let user_type = if offline { "legacy" } else { "msa" };

    println!("=== mcli Launcher ===");
    println!(
        "Profile: {} | Version: {} | User: {} ({})",
        target,
        target_version,
        username,
        if offline { "Offline" } else { "Online" }
    );

    let game_dir = PathBuf::from(&current_profile.game_dir);
    fs::create_dir_all(&game_dir)?;

    let use_fast = current_profile.fast_launch || overrides.fast;
    let (classpath_str, natives_dir, active_assets) =
        download_game_files(client, paths, &bp, Some(&game_dir), use_fast).await?;
    let java_exe = ensure_java(client, paths, &bp, Some(&current_profile.java_bin)).await?;

    // Construct the Java command only after all downloadable prerequisites are
    // present. This avoids leaving a child process running after a failed sync.
    let mut cmd = Command::new(&java_exe);
    for flag in current_profile.jvm_args.split_whitespace() {
        cmd.arg(flag);
    }

    // Mojang supplies logging configurations (including security fixes) for
    // older releases. Apply them instead of relying on the configuration in the JAR.
    if let Some(logging) = bp.get("logging").and_then(|l| l.get("client")) {
        if let (Some(id), Some(url), Some(argument)) = (
            logging["file"]["id"].as_str(),
            logging["file"]["url"].as_str(),
            logging["argument"].as_str(),
        ) {
            crate::config::validate_id(id)?;
            let logging_path = paths.assets.join("log_configs").join(id);
            download_file(client, url, &logging_path, true).await?;
            cmd.arg(argument.replace("${path}", &logging_path.to_string_lossy()));
        }
    }

    #[cfg(target_os = "macos")]
    cmd.arg("-XstartOnFirstThread");

    cmd.arg(format!("-Djava.library.path={}", natives_dir.display()))
        .arg("-cp")
        .arg(&classpath_str)
        .arg(
            bp["mainClass"]
                .as_str()
                .unwrap_or("net.minecraft.client.main.Main"),
        );

    // Pick the metadata format appropriate for the requested game version.
    if let Some(args) = legacy_arguments(
        &bp,
        username,
        uuid,
        access_token,
        user_type,
        &game_dir,
        &active_assets,
        &target_version,
    ) {
        cmd.args(args);
    } else {
        cmd.args(["--username", username])
            .args(["--version", &target_version])
            .args(["--gameDir", &game_dir.to_string_lossy()])
            .args(["--assetsDir", &active_assets.to_string_lossy()])
            .args([
                "--assetIndex",
                bp["assetIndex"]["id"].as_str().unwrap_or(jar_version),
            ])
            .args(["--uuid", uuid])
            .args(["--accessToken", access_token])
            .args(["--userType", user_type]);
    }

    if current_profile.fullscreen || overrides.fullscreen {
        cmd.arg("--fullscreen");
    } else if current_profile.width != "auto" && current_profile.height != "auto" {
        cmd.args([
            "--width",
            &current_profile.width,
            "--height",
            &current_profile.height,
        ]);
    }

    if let Some(srv) = server {
        cmd.args(["--server", &srv, "--port", &port]);
    }

    println!("\n[!] Launching Minecraft {}...", target_version);
    let status = cmd.current_dir(&game_dir).spawn()?.wait()?;
    if !status.success() {
        return Err(format!("Minecraft exited with {status}").into());
    }
    println!("\n[!] Game closed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::legacy_arguments;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn expands_legacy_launcher_placeholders() {
        let blueprint = json!({
            "minecraftArguments": "--username ${auth_player_name} --token ${auth_access_token} --type ${user_type} --gameDir ${game_directory}"
        });
        let args = legacy_arguments(
            &blueprint,
            "Alex",
            "uuid",
            "token",
            "msa",
            Path::new("game"),
            Path::new("assets"),
            "1.0",
        )
        .unwrap();

        assert_eq!(
            args,
            [
                "--username",
                "Alex",
                "--token",
                "token",
                "--type",
                "msa",
                "--gameDir",
                "game"
            ]
        );
    }
}
