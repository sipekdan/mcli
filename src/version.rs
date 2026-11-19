//! Mojang version metadata, inheritance resolution, and version subcommands.

use crate::cli::VersionAction;
use crate::config::{Paths, Result, MANIFEST_URL};
use crate::http::{download_verified, get_json};
use crate::launcher::download_game_files;
use crate::loader::{self, Loader};
use crate::runtime::ensure_java;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

pub fn maven_to_path(coord: &str) -> Option<String> {
    // group:artifact:version[:classifier][@extension]
    let (coord, ext) = match coord.split_once('@') {
        Some((c, e)) => (c, e),
        None => (coord, "jar"),
    };
    let parts: Vec<&str> = coord.split(':').collect();
    if parts.len() < 3 || parts[..3].iter().any(|part| part.is_empty()) {
        return None;
    }
    let group = parts[0].replace('.', "/");
    let artifact = parts[1];
    let version = parts[2];
    let classifier = if parts.len() > 3 {
        format!("-{}", parts[3])
    } else {
        String::new()
    };
    Some(format!(
        "{group}/{artifact}/{version}/{artifact}-{version}{classifier}.{ext}"
    ))
}

pub fn version_json_path(paths: &Paths, id: &str) -> PathBuf {
    paths.versions.join(id).join(format!("{id}.json"))
}

pub fn read_local_version(paths: &Paths, id: &str) -> Option<Value> {
    let text = fs::read_to_string(version_json_path(paths, id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The vanilla game version underneath a (possibly modded) version id.
pub fn base_game_version(paths: &Paths, id: &str) -> String {
    let mut current = id.to_string();
    let mut seen = HashSet::new();
    while let Some(parent) = read_local_version(paths, &current)
        .and_then(|v| v["inheritsFrom"].as_str().map(String::from))
    {
        if !seen.insert(parent.clone()) {
            break;
        }
        current = parent;
    }
    current
}

pub async fn fetch_manifest(client: &Client) -> Result<Value> {
    get_json(client, MANIFEST_URL, None).await
}

/// `latest`/`release` and `snapshot` follow Mojang's manifest.
fn alias_target(id: &str, manifest: &Value) -> Option<String> {
    let key = match id {
        "latest" | "release" | "latest-release" => "release",
        "snapshot" | "latest-snapshot" => "snapshot",
        _ => return None,
    };
    manifest["latest"][key].as_str().map(String::from)
}

pub async fn resolve_alias(client: &Client, paths: &Paths, id: &str) -> Result<String> {
    if !matches!(
        id,
        "latest" | "release" | "latest-release" | "snapshot" | "latest-snapshot"
    ) || version_json_path(paths, id).exists()
    {
        return Ok(id.to_string());
    }
    let manifest = fetch_manifest(client).await?;
    alias_target(id, &manifest).ok_or_else(|| "Manifest has no latest version".into())
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
        let local_json = version_json_path(paths, &curr_id);
        let bp: Value = if local_json.exists() {
            serde_json::from_str(&fs::read_to_string(&local_json)?)
                .map_err(|e| format!("Corrupt version file {}: {e}", local_json.display()))?
        } else {
            if manifest_cache.is_none() {
                manifest_cache = Some(fetch_manifest(client).await?);
            }
            let manifest = manifest_cache.as_ref().unwrap();
            let entry = manifest["versions"]
                .as_array()
                .ok_or("Invalid manifest")?
                .iter()
                .find(|v| v["id"].as_str() == Some(&curr_id))
                .ok_or_else(|| {
                    format!(
                        "Version '{}' not found locally or on Mojang servers!",
                        curr_id
                    )
                })?;
            let url = entry["url"].as_str().ok_or("Manifest entry has no url")?;
            download_verified(client, url, &local_json, entry["sha1"].as_str(), None, true).await?;
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
    let mut jar_version = base["id"].as_str().unwrap_or(version_id).to_string();
    for child in chain {
        if let Some(jar) = child["jar"].as_str() {
            jar_version = jar.to_string();
        }
        merge_child(&mut base, child);
    }
    base["_jar_version"] = Value::String(jar_version);
    Ok(base)
}

/// Merge a child version into its resolved parent, as the official launcher
/// does: libraries are child-first (so the child's versions win on
/// de-duplication), argument lists are parent-then-child, and every other
/// top-level field present in the child replaces the parent's.
pub(crate) fn merge_child(base: &mut Value, child: Value) {
    let Value::Object(child) = child else {
        return;
    };
    for (key, value) in child {
        match key.as_str() {
            "inheritsFrom" | "jar" => {}
            "libraries" => {
                let mut combined = value.as_array().cloned().unwrap_or_default();
                combined.extend(base["libraries"].as_array().cloned().unwrap_or_default());
                base["libraries"] = Value::Array(combined);
            }
            "arguments" => {
                let Value::Object(sections) = value else {
                    continue;
                };
                if !base["arguments"].is_object() {
                    base["arguments"] = Value::Object(Default::default());
                }
                for (section, items) in sections {
                    let mut merged = base["arguments"][&section]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    merged.extend(items.as_array().cloned().unwrap_or_default());
                    base["arguments"][&section] = Value::Array(merged);
                }
            }
            _ => {
                // An empty child value (Forge ships `"logging": {}`) must not
                // wipe the parent's, e.g. Mojang's patched log4j config.
                let empty = match &value {
                    Value::Null => true,
                    Value::Object(o) => o.is_empty(),
                    Value::String(s) => s.is_empty(),
                    _ => false,
                };
                if !empty {
                    base[key] = value;
                }
            }
        }
    }
}

pub async fn download_server_jar(
    client: &Client,
    paths: &Paths,
    version: &str,
    output: Option<String>,
) -> Result<()> {
    let version = resolve_alias(client, paths, version).await?;
    let bp = resolve_blueprint(client, paths, &version).await?;
    let server = &bp["downloads"]["server"];
    let Some(url) = server["url"].as_str() else {
        println!(
            "[!] No official server JAR listed by Mojang for version {}.",
            version
        );
        return Ok(());
    };
    let out_path = match output {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()?.join(format!("minecraft_server.{}.jar", version)),
    };
    download_verified(
        client,
        url,
        &out_path,
        server["sha1"].as_str(),
        server["size"].as_u64(),
        false,
    )
    .await?;
    println!("[+] Server JAR saved to: {}", out_path.display());
    Ok(())
}

/// Download everything a version needs to launch (and its Java runtime).
pub async fn install_version(client: &Client, paths: &Paths, version: &str) -> Result<()> {
    let bp = resolve_blueprint(client, paths, version).await?;
    download_game_files(client, paths, &bp, None).await?;
    ensure_java(client, paths, &bp, None).await?;
    Ok(())
}

fn installed_versions(paths: &Paths) -> Vec<String> {
    let mut installed: Vec<String> = fs::read_dir(&paths.versions)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|id| version_json_path(paths, id).exists())
        .collect();
    installed.sort_by(|a, b| crate::rules::compare_versions(b, a).then(a.cmp(b)));
    installed
}

pub async fn handle_version_cmd(
    action: VersionAction,
    client: &Client,
    paths: &Paths,
) -> Result<()> {
    match action {
        VersionAction::List { snapshots, local } => {
            println!("=== Locally Installed Versions ===");
            let installed = installed_versions(paths);
            if installed.is_empty() {
                println!("None");
            }
            for id in &installed {
                let parent = read_local_version(paths, id)
                    .and_then(|v| v["inheritsFrom"].as_str().map(String::from));
                match parent {
                    Some(p) => println!("  {id}  (on {p})"),
                    None => println!("  {id}"),
                }
            }

            if !local {
                println!("\n=== Official Mojang Versions ===");
                let manifest = fetch_manifest(client).await?;
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
        VersionAction::Install {
            version,
            loader,
            loader_version,
        } => {
            let version = resolve_alias(client, paths, &version).await?;
            let id = match loader.unwrap_or(Loader::Vanilla) {
                Loader::Vanilla => {
                    println!("=== Installing Minecraft {} ===", version);
                    version.clone()
                }
                l => loader::install(client, paths, l, &version, loader_version.as_deref()).await?,
            };
            install_version(client, paths, &id).await?;
            println!("\n[+] Installation of {} complete!", id);
            if id != version {
                println!("    Launch it with: mcli run <profile> --version {id}");
            }
        }
        VersionAction::Loaders {
            loader,
            version,
            all,
        } => {
            let version = resolve_alias(client, paths, &version).await?;
            let list = loader::list_versions(client, loader, &version).await?;
            println!("=== {loader} versions for Minecraft {version} ===");
            let shown: Vec<_> = list.iter().filter(|v| all || v.stable).collect();
            let shown = if shown.is_empty() {
                list.iter().collect()
            } else {
                shown
            };
            for (i, v) in shown
                .iter()
                .take(if all { usize::MAX } else { 20 })
                .enumerate()
            {
                let mut tags = Vec::new();
                if v.recommended {
                    tags.push("recommended");
                }
                if i == 0 {
                    tags.push("latest");
                }
                if !v.stable {
                    tags.push("unstable");
                }
                let tag = if tags.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", tags.join(", "))
                };
                println!("  {}{}", v.version, tag);
            }
            if !all && shown.len() > 20 {
                println!("  ... {} more (use --all)", shown.len() - 20);
            }
        }
        VersionAction::Uninstall { version } => {
            crate::config::validate_id(&version)?;
            let version_dir = paths.versions.join(&version);
            if version_dir.exists() {
                // Children of this version would no longer resolve.
                let dependants: Vec<String> = installed_versions(paths)
                    .into_iter()
                    .filter(|id| {
                        read_local_version(paths, id)
                            .is_some_and(|v| v["inheritsFrom"].as_str() == Some(&version))
                    })
                    .collect();
                if !dependants.is_empty() {
                    println!(
                        "[!] Note: {} inherit from {version} and will re-download it on next launch.",
                        dependants.join(", ")
                    );
                }
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
    use super::{maven_to_path, merge_child};
    use serde_json::json;

    #[test]
    fn converts_maven_coordinates_to_a_jar_path() {
        assert_eq!(
            maven_to_path("com.example:library:1.2.3:client"),
            Some("com/example/library/1.2.3/library-1.2.3-client.jar".to_string())
        );
        assert_eq!(
            maven_to_path("com.example:data:1.0@zip"),
            Some("com/example/data/1.0/data-1.0.zip".to_string())
        );
    }

    #[test]
    fn rejects_incomplete_maven_coordinates() {
        assert_eq!(maven_to_path("com.example:library"), None);
    }

    #[test]
    fn merges_child_like_the_official_launcher() {
        let mut base = json!({
            "id": "1.21.1",
            "type": "release",
            "mainClass": "net.minecraft.client.main.Main",
            "libraries": [{"name": "a:parent:1"}],
            "arguments": {"game": ["--version", "${version_name}"], "jvm": ["-cp", "${classpath}"]},
            "javaVersion": {"majorVersion": 21}
        });
        let child = json!({
            "id": "fabric-loader-0.16.9-1.21.1",
            "inheritsFrom": "1.21.1",
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient",
            "libraries": [{"name": "a:child:1"}],
            "arguments": {"game": [], "jvm": ["-DFabricMcEmu= net.minecraft.client.main.Main "]}
        });
        merge_child(&mut base, child);
        assert_eq!(base["id"], "fabric-loader-0.16.9-1.21.1");
        assert_eq!(
            base["mainClass"],
            "net.fabricmc.loader.impl.launch.knot.KnotClient"
        );
        assert_eq!(base["libraries"][0]["name"], "a:child:1");
        assert_eq!(base["libraries"][1]["name"], "a:parent:1");
        assert_eq!(
            base["arguments"]["jvm"],
            json!([
                "-cp",
                "${classpath}",
                "-DFabricMcEmu= net.minecraft.client.main.Main "
            ])
        );
        assert_eq!(base["javaVersion"]["majorVersion"], 21);
        assert!(base.get("inheritsFrom").is_none());
    }

    #[test]
    fn empty_child_fields_keep_parent_values() {
        let mut base =
            json!({"id": "1.12.2", "libraries": [], "logging": {"client": {"argument": "x"}}});
        merge_child(
            &mut base,
            json!({"id": "forge", "libraries": [], "logging": {}}),
        );
        assert_eq!(base["logging"]["client"]["argument"], "x");
    }

    #[test]
    fn legacy_child_arguments_replace_parent() {
        let mut base = json!({"id": "1.7.10", "minecraftArguments": "--a", "libraries": []});
        merge_child(
            &mut base,
            json!({"id": "forge", "minecraftArguments": "--a --tweakClass X", "libraries": []}),
        );
        assert_eq!(base["minecraftArguments"], "--a --tweakClass X");
    }
}
