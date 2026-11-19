//! `mcli verify`: re-hash every file a profile uses against the official
//! SHA-1 checksums and repair anything missing or damaged.
//!
//! Normal launches only check presence and size (one `stat` per file); this
//! is the full corruption check, run on demand.

use crate::assets::load_assets;
use crate::config::{validate_id, Paths, Result};
use crate::files::{plan_game_files, FileEntry, Kind};
use crate::http::file_sha1;
use crate::launcher::download_game_files;
use crate::profile::load_profiles;
use crate::version::{resolve_alias, resolve_blueprint, version_json_path};
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

enum Status {
    Ok,
    Missing,
    Damaged(String),
    /// Present, but no checksum is published for it.
    Unverifiable,
}

fn check(entry: &FileEntry) -> Status {
    let Ok(meta) = fs::metadata(&entry.path) else {
        return Status::Missing;
    };
    if let Some(size) = entry.size {
        if meta.len() != size {
            return Status::Damaged(format!("size {} != {size}", meta.len()));
        }
    }
    let Some(expected) = &entry.sha1 else {
        return Status::Unverifiable;
    };
    match file_sha1(&entry.path) {
        Ok(actual) if actual.eq_ignore_ascii_case(expected) => Status::Ok,
        Ok(_) => Status::Damaged("checksum mismatch".into()),
        Err(e) => Status::Damaged(format!("unreadable: {e}")),
    }
}

/// Damaged files are deleted so the normal download step replaces them.
/// That needs a source: a download URL, or a local original (a loader's
/// client-jar link, or a reconstructed legacy asset copy).
fn repairable(entry: &FileEntry) -> bool {
    !entry.url.is_empty() || matches!(entry.kind, Kind::ClientJar | Kind::Asset)
}

pub async fn verify(target: &str, client: &Client, paths: &Paths) -> Result<()> {
    validate_id(target)?;
    let profiles = load_profiles(paths);
    let (version, game_dir) = match profiles.get(target) {
        Some(profile) => (
            resolve_alias(client, paths, &profile.version).await?,
            Some(PathBuf::from(&profile.game_dir)),
        ),
        None if version_json_path(paths, target).exists() => (target.to_string(), None),
        None => {
            return Err(format!("No profile or installed version named '{target}'").into());
        }
    };
    println!("=== Verifying {version} ===");

    let blueprint = resolve_blueprint(client, paths, &version).await?;
    let plan = plan_game_files(paths, &blueprint)?;
    let mut entries = plan.files.clone();
    if plan.launch_jar != plan.parent_jar {
        // The loader's copy of the client jar must match Mojang's jar too.
        if let Some(client_jar) = entries.iter().find(|e| e.kind == Kind::ClientJar) {
            let name = plan
                .launch_jar
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            entries.push(FileEntry {
                path: plan.launch_jar.clone(),
                url: String::new(),
                name: name.unwrap_or_default(),
                ..client_jar.clone()
            });
        }
    }
    if let Some(assets) = load_assets(client, paths, &blueprint, game_dir.as_deref()).await? {
        entries.extend(assets.objects);
        entries.extend(assets.copies.into_iter().map(|(copy, _)| copy));
    }
    // Many asset names share one object; hash each file once.
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.path.clone()));

    let progress = ProgressBar::new(entries.len() as u64);
    progress.set_style(ProgressStyle::with_template(
        "  {spinner:.cyan} {wide_bar:.cyan/blue} {pos}/{len} • {eta}",
    )?);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut results = stream::iter(entries)
        .map(|entry| {
            tokio::task::spawn_blocking(move || {
                let status = check(&entry);
                (entry, status)
            })
        })
        .buffer_unordered(threads);

    let (mut ok, mut unverifiable, mut bytes) = (0usize, 0usize, 0u64);
    let mut missing = Vec::new();
    let mut damaged = Vec::new();
    let mut unrepairable = Vec::new();
    while let Some(result) = results.next().await {
        let (entry, status) = result?;
        progress.inc(1);
        match status {
            Status::Ok => {
                ok += 1;
                bytes += entry.size.unwrap_or(0);
            }
            Status::Unverifiable => unverifiable += 1,
            Status::Missing => missing.push(entry),
            Status::Damaged(reason) => {
                if repairable(&entry) {
                    fs::remove_file(&entry.path)?;
                    damaged.push((entry, reason));
                } else {
                    unrepairable.push((entry, reason));
                }
            }
        }
    }
    progress.finish_and_clear();

    println!(
        "Checked {} files: {ok} OK ({:.0} MB hashed), {} damaged, {} missing.",
        ok + unverifiable + missing.len() + damaged.len() + unrepairable.len(),
        bytes as f64 / 1_048_576.0,
        damaged.len() + unrepairable.len(),
        missing.len(),
    );
    for (entry, reason) in damaged.iter().chain(&unrepairable).take(15) {
        println!("  damaged: {} ({reason})", entry.name);
    }
    for entry in missing.iter().take(15) {
        println!("  missing: {}", entry.name);
    }
    if unverifiable > 0 {
        println!(
            "  {unverifiable} file(s) have no published checksum (old Forge libraries); \
             only their presence and size were checked."
        );
    }

    if !damaged.is_empty() || !missing.is_empty() {
        println!("Repairing...");
        download_game_files(client, paths, &blueprint, game_dir.as_deref()).await?;
        println!("[+] Repaired {} file(s).", damaged.len() + missing.len());
    }
    if !unrepairable.is_empty() {
        return Err(format!(
            "{} file(s) generated by the mod-loader installer are damaged. Reinstall the loader: \
             mcli version uninstall {version} && mcli version install <mc> --loader <name> --loader-version <version>",
            unrepairable.len()
        )
        .into());
    }
    if damaged.is_empty() && missing.is_empty() {
        println!("[+] All files match the official checksums.");
    }
    Ok(())
}
