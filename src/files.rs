//! The set of files a version needs, with their published checksums.
//!
//! Launching and `mcli verify` share this plan: a launch downloads entries
//! that are missing or have the wrong size (cheap, one `stat` each), while
//! `verify` additionally re-hashes every file against its official SHA-1.

use crate::config::{validate_id, validate_relative_path, Paths, Result, MOJANG_OS};
use crate::http::{download_verified, needs_download};
use crate::rules::{rules_allow, Features};
use crate::version::maven_to_path;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    ClientJar,
    Library,
    /// A pre-1.19 natives archive, extracted into the natives directory.
    Native,
    LogConfig,
    Asset,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Empty for files a mod-loader installer generates locally.
    pub url: String,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub name: String,
    pub kind: Kind,
}

impl FileEntry {
    fn new(path: PathBuf, url: &str, info: &Value, name: &str, kind: Kind) -> Self {
        Self {
            path,
            url: url.to_string(),
            sha1: info["sha1"].as_str().map(str::to_ascii_lowercase),
            size: info["size"].as_u64(),
            name: name.to_string(),
            kind,
        }
    }
}

pub struct GamePlan {
    /// Library and natives files, plus the base client jar and log config.
    pub files: Vec<FileEntry>,
    /// Classpath entries in launch order (without the client jar).
    pub classpath: Vec<PathBuf>,
    /// The base version's jar, as published by Mojang.
    pub parent_jar: PathBuf,
    /// The jar the game launches with (a per-loader copy for child versions).
    pub launch_jar: PathBuf,
    pub natives_dir: PathBuf,
    /// JVM argument enabling Mojang's log4j configuration, if any.
    pub log_argument: Option<String>,
}

pub fn plan_game_files(paths: &Paths, blueprint: &Value) -> Result<GamePlan> {
    // `_jar_version` is the base version that owns the client JAR. A modded
    // child profile may have a different visible version ID.
    let jar_version = blueprint["_jar_version"]
        .as_str()
        .ok_or("Blueprint is missing its base version")?;
    let parent_jar = paths
        .versions
        .join(jar_version)
        .join(format!("{jar_version}.jar"));
    let id = blueprint["id"].as_str().unwrap_or(jar_version);
    validate_id(id)?;
    // Child versions (mod loaders) launch from their own copy named after
    // their id, like the official launcher: Forge's `-DignoreList=...,
    // ${version_name}.jar` relies on that name.
    let launch_jar = if id == jar_version {
        parent_jar.clone()
    } else {
        paths.versions.join(id).join(format!("{id}.jar"))
    };

    let mut files = Vec::new();
    let client = &blueprint["downloads"]["client"];
    if let Some(url) = client["url"].as_str() {
        files.push(FileEntry::new(
            parent_jar.clone(),
            url,
            client,
            &format!("{jar_version}.jar"),
            Kind::ClientJar,
        ));
    }

    let natives_dir = paths
        .versions
        .join(jar_version)
        .join(format!("natives-{MOJANG_OS}"));
    let mut classpath = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut seen_artifacts: HashSet<String> = HashSet::new();
    let no_features = Features::default();

    for lib in blueprint["libraries"].as_array().into_iter().flatten() {
        // Mojang metadata can include libraries for a different OS/arch.
        if !rules_allow(lib.get("rules"), &no_features) {
            continue;
        }
        let name = lib["name"].as_str().unwrap_or("<unnamed>");

        // A child version may repeat a parent's Maven artifact. Keep the
        // first matching coordinate (the child's) in the classpath.
        let parts: Vec<&str> = name.split(':').collect();
        if parts.len() >= 2 && lib.get("natives").is_none() {
            let classifier = parts.get(3).copied().unwrap_or("");
            if !seen_artifacts.insert(format!("{}:{}:{}", parts[0], parts[1], classifier)) {
                continue;
            }
        }

        // Modern metadata gives an exact artifact URL. Older and third-party
        // metadata only has a Maven coordinate plus a repository base URL.
        let artifact = lib.get("downloads").and_then(|d| d.get("artifact"));
        let entry = if let Some(artifact) = artifact {
            let rel = match artifact["path"].as_str() {
                Some(p) => p.to_string(),
                None => maven_to_path(name)
                    .ok_or_else(|| format!("Invalid Maven library coordinate: '{name}'"))?,
            };
            artifact["url"]
                .as_str()
                .map(|url| (url.to_string(), rel, artifact))
        } else if lib.get("downloads").is_none() && lib.get("natives").is_none() {
            let rel = maven_to_path(name)
                .ok_or_else(|| format!("Invalid Maven library coordinate: '{name}'"))?;
            let mut base_url = lib["url"]
                .as_str()
                .unwrap_or("https://libraries.minecraft.net/")
                .replace(
                    "http://files.minecraftforge.net/maven/",
                    "https://maven.minecraftforge.net/",
                );
            if !base_url.ends_with('/') {
                base_url.push('/');
            }
            // Fabric/Quilt put `sha1`/`size` on the library itself.
            Some((base_url + &rel, rel, lib))
        } else {
            None
        };
        if let Some((url, rel, info)) = entry {
            let rel = rel.replace('\\', "/");
            validate_relative_path(&rel)?;
            let path = paths.libraries.join(&rel);
            if seen_paths.insert(path.clone()) {
                classpath.push(path.clone());
                files.push(FileEntry::new(path, &url, info, name, Kind::Library));
            }
        }

        // Pre-1.19 natives: a platform classifier archive that must be
        // extracted. The classifier name can include `${arch}`.
        if let Some(template) = lib.get("natives").and_then(|n| n[MOJANG_OS].as_str()) {
            let bits = if cfg!(target_pointer_width = "32") {
                "32"
            } else {
                "64"
            };
            let key = template.replace("${arch}", bits);
            let info = lib
                .get("downloads")
                .and_then(|d| d.get("classifiers"))
                .and_then(|c| c.get(&key));
            if let Some(info) = info {
                if let (Some(url), Some(p)) = (info["url"].as_str(), info["path"].as_str()) {
                    validate_relative_path(p)?;
                    let path = paths.libraries.join(p);
                    if seen_paths.insert(path.clone()) {
                        files.push(FileEntry::new(path, url, info, name, Kind::Native));
                    }
                }
            }
        }
    }

    // Mojang supplies logging configurations (including security fixes) for
    // older releases. Use them instead of the configuration inside the jar.
    let mut log_argument = None;
    if let Some(logging) = blueprint.get("logging").and_then(|l| l.get("client")) {
        let file = &logging["file"];
        if let (Some(id), Some(url), Some(argument)) = (
            file["id"].as_str(),
            file["url"].as_str(),
            logging["argument"].as_str(),
        ) {
            validate_id(id)?;
            let path = paths.assets.join("log_configs").join(id);
            log_argument = Some(argument.replace("${path}", &path.to_string_lossy()));
            files.push(FileEntry::new(path, url, file, id, Kind::LogConfig));
        }
    }

    Ok(GamePlan {
        files,
        classpath,
        parent_jar,
        launch_jar,
        natives_dir,
        log_argument,
    })
}

/// Download every entry that is missing or has the wrong size, verifying
/// each download against its SHA-1. Returns how many files were fetched.
pub async fn fetch_missing(client: &Client, entries: &[FileEntry], label: &str) -> Result<usize> {
    let mut pending = Vec::new();
    let mut scheduled = HashSet::new();
    for entry in entries {
        // Several asset names can share one object; download it once, or
        // concurrent tasks would race on the same `.part` file.
        if !needs_download(&entry.path, entry.size) || !scheduled.insert(&entry.path) {
            continue;
        }
        if entry.url.is_empty() {
            // Forge/NeoForge installers generate some libraries locally and
            // publish them with an empty URL.
            return Err(format!(
                "{} is missing or damaged and has no download URL. Reinstall the mod loader \
                 (mcli version install <mc> --loader <name> --loader-version <version>).",
                entry.name
            )
            .into());
        }
        pending.push(entry.clone());
    }
    let count = pending.len();
    if count == 0 {
        return Ok(0);
    }

    println!("Downloading {} {label}...", pending.len());
    let progress = ProgressBar::new(pending.len() as u64);
    progress.set_style(ProgressStyle::with_template(
        "  {spinner:.green} {wide_bar:.green/white} {pos}/{len} • {eta}",
    )?);
    progress.enable_steady_tick(std::time::Duration::from_millis(100));

    // Network-bound and independent: bounded concurrency speeds up a first
    // install without overwhelming the CDN or the local connection.
    let mut stream = stream::iter(pending)
        .map(|entry| {
            let client = client.clone();
            async move {
                download_verified(
                    &client,
                    &entry.url,
                    &entry.path,
                    entry.sha1.as_deref(),
                    entry.size,
                    true,
                )
                .await
                .map_err(|e| format!("{}: {e}", entry.name))
            }
        })
        .buffer_unordered(16);
    while let Some(result) = stream.next().await {
        result?;
        progress.inc(1);
    }
    progress.finish_and_clear();
    Ok(count)
}
