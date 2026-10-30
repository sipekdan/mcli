//! Java runtime discovery and portable Adoptium runtime installation.

use crate::config::{Paths, Result, ADOPTIUM_ARCH, ADOPTIUM_OS};
use crate::http::{download_file, extract_zip};
use reqwest::Client;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_java_bin(dir: &Path, bin_name: &str) -> Option<PathBuf> {
    // Adoptium archives contain a versioned top-level directory, so the exact
    // extraction path cannot be assumed.
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_java_bin(&path, bin_name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(bin_name)
            && path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                == Some("bin")
        {
            return Some(path);
        }
    }
    None
}

pub async fn ensure_java(
    client: &Client,
    paths: &Paths,
    blueprint: &Value,
    custom_java: Option<&str>,
) -> Result<String> {
    if let Some(cj) = custom_java {
        if !cj.is_empty() && cj != "bundled" {
            return Ok(cj.to_string());
        }
    }

    // Mojang version metadata specifies the Java major required by each game.
    let major = blueprint["javaVersion"]["majorVersion"]
        .as_u64()
        .unwrap_or(8);
    let java_dir = paths.java.join(format!("java{}", major));
    let bin_name = if cfg!(windows) { "java.exe" } else { "java" };

    if let Some(found) = find_java_bin(&java_dir, bin_name) {
        return Ok(found.to_string_lossy().to_string());
    }

    println!(
        "\n[!] Java {} not found. Downloading Adoptium JRE for {}...",
        major, ADOPTIUM_OS
    );
    fs::create_dir_all(&java_dir)?;

    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let url = format!(
        "https://api.adoptium.net/v3/binary/latest/{}/ga/{}/{}/jre/hotspot/normal/eclipse",
        major, ADOPTIUM_OS, ADOPTIUM_ARCH
    );
    let archive_path = java_dir.join(format!("jre{}.{}", major, ext));

    download_file(client, &url, &archive_path, false).await?;
    println!("Extracting Java {}...", major);

    if cfg!(windows) {
        extract_zip(&archive_path, &java_dir, false)?;
    } else {
        let status = Command::new("tar")
            .args([
                "-xzf",
                &archive_path.to_string_lossy(),
                "-C",
                &java_dir.to_string_lossy(),
            ])
            .status()?;
        if !status.success() {
            return Err(format!("Java archive extraction failed with {status}").into());
        }
    }
    if let Err(error) = fs::remove_file(&archive_path) {
        eprintln!(
            "[!] Java was extracted, but the temporary archive could not be removed ({}): {}",
            archive_path.display(),
            error
        );
    }

    let found =
        find_java_bin(&java_dir, bin_name).ok_or("Java binary not found after extraction!")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&found, fs::Permissions::from_mode(0o755));
    }
    Ok(found.to_string_lossy().to_string())
}
