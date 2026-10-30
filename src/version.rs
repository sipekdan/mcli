//! Mojang version metadata, inheritance resolution, and version subcommands.

use crate::cli::VersionAction;
use crate::config::{Paths, Result, MANIFEST_URL};
use crate::http::{download_file, get_json};
use crate::launcher::download_game_files;
use crate::runtime::ensure_java;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

pub fn maven_to_path(coord: &str) -> Option<String> {
    let parts: Vec<&str> = coord.split(':').collect();
    if parts.len() < 3 || parts[..3].iter().any(|part| part.is_empty()) {
        return None;
    }
    let group = parts[0].replace('.', "/");
    let artifact = parts.get(1).copied().unwrap_or("");
    let version = parts.get(2).copied().unwrap_or("");
    let classifier = if parts.len() > 3 {
        format!("-{}", parts[3])
    } else {
        String::new()
    };
    Some(format!(
        "{}/{}/{}/{}-{}{}.jar",
        group, artifact, version, artifact, version, classifier
    ))
}

pub async fn resolve_blueprint(client: &Client, paths: &Paths, version_id: &str) -> Result<Value> {
    // Mod loaders use `inheritsFrom`: resolve the complete parent chain before
    // combining it into the single blueprint consumed by the launcher.
    let mut chain: Vec<Value> = Vec::new();
    let mut curr_id = version_id.to_string();
    let mut manifest_cache: Option<Value> = None;
    let mut visited = HashSet::new();

    loop {
        crate::config::validate_id(&curr_id)?;
        // A malformed third-party profile can otherwise loop forever.
        if !visited.insert(curr_id.clone()) {
            return Err(format!("Circular inheritsFrom reference involving '{}'", curr_id).into());
        }
        let local_json = paths
            .versions
            .join(&curr_id)
            .join(format!("{}.json", curr_id));
        let bp: Value = if local_json.exists() {
            serde_json::from_str(&fs::read_to_string(&local_json)?)?
        } else {
            if manifest_cache.is_none() {
                manifest_cache = Some(get_json(client, MANIFEST_URL, None).await?);
            }
            let versions = manifest_cache.as_ref().unwrap()["versions"]
                .as_array()
                .ok_or("Invalid manifest")?;
            let ver_url = versions
                .iter()
                .find(|v| v["id"].as_str() == Some(&curr_id))
                .and_then(|v| v["url"].as_str())
                .ok_or_else(|| {
                    format!(
                        "Version '{}' not found locally or on Mojang servers!",
                        curr_id
                    )
                })?;

            download_file(client, ver_url, &local_json, true).await?;
            serde_json::from_str(&fs::read_to_string(&local_json)?)?
        };

        let next_parent = bp
            .get("inheritsFrom")
            .and_then(|v| v.as_str())
            .map(String::from);
        chain.push(bp);
        if let Some(parent_id) = next_parent {
            curr_id = parent_id;
        } else {
            break;
        }
    }

    chain.reverse();
    let mut base = chain.remove(0);
    let jar_version = base["id"].as_str().unwrap_or(version_id).to_string();

    for child in chain {
        // Child libraries come first so their versions win where the launcher
        // later de-duplicates group/artifact coordinates.
        if let (Some(base_libs), Some(child_libs)) = (
            base.get_mut("libraries").and_then(|v| v.as_array_mut()),
            child.get("libraries").and_then(|v| v.as_array()),
        ) {
            let mut combined = child_libs.clone();
            combined.extend(base_libs.clone());
            *base_libs = combined;
        }
        if let Some(mc) = child.get("mainClass") {
            base["mainClass"] = mc.clone();
        }
        if let Some(id) = child.get("id") {
            base["id"] = id.clone();
        }
    }
    base["_jar_version"] = Value::String(jar_version);
    Ok(base)
}

pub async fn download_server_jar(
    client: &Client,
    paths: &Paths,
    version: &str,
    output: Option<String>,
) -> Result<()> {
    let bp = resolve_blueprint(client, paths, version).await?;
    let Some(url) = bp["downloads"]["server"]["url"].as_str() else {
        println!(
            "[!] No official server JAR listed by Mojang for version {}.",
            version
        );
        return Ok(());
    };
    let out_path = output.map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap()
            .join(format!("minecraft_server.{}.jar", version))
    });
    download_file(client, url, &out_path, false).await?;
    println!("[+] Server JAR saved to: {}", out_path.display());
    Ok(())
}

pub async fn handle_version_cmd(
    action: VersionAction,
    client: &Client,
    paths: &Paths,
) -> Result<()> {
    match action {
        VersionAction::List { snapshots, local } => {
            println!("=== Locally Installed Versions ===");
            if paths.versions.exists() {
                let installed: Vec<String> = fs::read_dir(&paths.versions)?
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect();
                println!(
                    "{}",
                    if installed.is_empty() {
                        "None".to_string()
                    } else {
                        installed.join(", ")
                    }
                );
            } else {
                println!("None");
            }

            if !local {
                println!("\n=== Official Mojang Versions ===");
                let manifest = get_json(client, MANIFEST_URL, None).await?;
                println!(
                    "Latest Release:  {}",
                    manifest["latest"]["release"].as_str().unwrap_or("")
                );
                println!(
                    "Latest Snapshot: {}",
                    manifest["latest"]["snapshot"].as_str().unwrap_or("")
                );
                if let Some(vers) = manifest["versions"].as_array() {
                    let filtered: Vec<&str> = vers
                        .iter()
                        .filter(|v| snapshots || v["type"].as_str() == Some("release"))
                        .filter_map(|v| v["id"].as_str())
                        .collect();
                    let preview: Vec<&str> = filtered.iter().copied().take(30).collect();
                    println!(
                        "Available: {} ... (+{} more)",
                        preview.join(", "),
                        filtered.len().saturating_sub(30)
                    );
                }
            }
        }
        VersionAction::Install { version } => {
            println!("=== Installing Minecraft {} ===", version);
            let bp = resolve_blueprint(client, paths, &version).await?;
            download_game_files(client, paths, &bp, None, false).await?;
            ensure_java(client, paths, &bp, None).await?;
            println!("\n[+] Installation of {} complete!", version);
        }
        VersionAction::Uninstall { version } => {
            crate::config::validate_id(&version)?;
            let version_dir = paths.versions.join(&version);
            if version_dir.exists() {
                fs::remove_dir_all(&version_dir)?;
                println!("[+] Successfully uninstalled Minecraft {}.", version);
            } else {
                println!("[!] Version '{}' is not installed locally.", version);
            }
        }
        VersionAction::Server { version, output } => {
            download_server_jar(client, paths, &version, output).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::maven_to_path;

    #[test]
    fn converts_maven_coordinates_to_a_jar_path() {
        assert_eq!(
            maven_to_path("com.example:library:1.2.3:client"),
            Some("com/example/library/1.2.3/library-1.2.3-client.jar".to_string())
        );
    }

    #[test]
    fn rejects_incomplete_maven_coordinates() {
        assert_eq!(maven_to_path("com.example:library"), None);
    }
}
