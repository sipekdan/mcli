//! Java runtime discovery and portable Adoptium runtime installation.

use crate::config::{Paths, Result, ADOPTIUM_ARCH, ADOPTIUM_OS};
use crate::http::{download_verified, extract_zip, file_sha256, get_json};
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
    let archive_path = java_dir.join(format!("jre{}.{}", major, ext));

    // Adoptium does not publish every combination: there is no Java 16 JRE
    // (Minecraft 1.17) and no Java 8 build for Apple Silicon. Fall back to a
    // JDK, then to x64 (which macOS runs through Rosetta).
    let mut candidates = vec![(ADOPTIUM_ARCH, "jre"), (ADOPTIUM_ARCH, "jdk")];
    if cfg!(target_os = "macos") && ADOPTIUM_ARCH == "aarch64" {
        candidates.extend([("x64", "jre"), ("x64", "jdk")]);
    }
    let mut chosen = None;
    let mut last_error: Option<String> = None;
    for (arch, image) in candidates {
        let url = format!(
            "https://api.adoptium.net/v3/assets/latest/{major}/hotspot?architecture={arch}&image_type={image}&os={ADOPTIUM_OS}&vendor=eclipse"
        );
        match get_json(client, &url, None).await {
            // An empty list means Adoptium has no such build.
            Ok(list) => {
                if let Some(package) = list[0]["binary"].get("package") {
                    chosen = Some(package.clone());
                    break;
                }
            }
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    let Some(package) = chosen else {
        return Err(format!(
            "No Java {major} runtime available from Adoptium for {ADOPTIUM_OS}/{ADOPTIUM_ARCH}{}. \
             Install one yourself and pass --java-bin /path/to/java.",
            last_error.map(|e| format!(" ({e})")).unwrap_or_default()
        )
        .into());
    };
    let link = package["link"]
        .as_str()
        .ok_or("Adoptium package has no link")?;
    download_verified(
        client,
        link,
        &archive_path,
        None,
        package["size"].as_u64(),
        false,
    )
    .await?;
    // Adoptium publishes a SHA-256 for every package.
    if let Some(expected) = package["checksum"].as_str() {
        let actual = file_sha256(&archive_path)?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&archive_path);
            return Err(format!(
                "Java {major} download is corrupt (SHA-256 {actual}, expected {expected}). Try again."
            )
            .into());
        }
    }
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
