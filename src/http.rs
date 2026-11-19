//! Small HTTP and archive helpers shared by downloads and Microsoft login.

use crate::config::{Result, NATIVE_EXTS};
use reqwest::Client;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DOWNLOAD_ATTEMPTS: u32 = 3;

pub fn build_http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("mcli/", env!("CARGO_PKG_VERSION")))
        // A total request timeout would abort large downloads (a JRE is ~50MB)
        // on slower connections. Only fail when the connection stalls.
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()?)
}

pub async fn get_text(client: &Client, url: &str) -> Result<String> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

pub async fn get_json(client: &Client, url: &str, bearer_token: Option<&str>) -> Result<Value> {
    let mut req = client.get(url);
    if let Some(token) = bearer_token {
        req = req.header("Authorization", format!("Bearer {}", token));
    }
    let text = req.send().await?.error_for_status()?.text().await?;
    Ok(serde_json::from_str(&text)?)
}

pub async fn post_json(client: &Client, url: &str, payload: &Value) -> Result<Value> {
    let text = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(serde_json::to_string(payload)?)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(serde_json::from_str(&text)?)
}

pub async fn post_form(client: &Client, url: &str, params: &[(&str, &str)]) -> Result<Value> {
    let response = client
        .post(url)
        .header("Accept", "application/json")
        .form(params)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    let value: Value = serde_json::from_str(&text)?;
    if !status.is_success() && value.get("error").is_none() {
        return Err(format!("Request failed with {status}: {text}").into());
    }
    Ok(value)
}

pub async fn download_file(client: &Client, url: &str, path: &Path, quiet: bool) -> Result<()> {
    download_verified(client, url, path, None, None, quiet).await
}

/// Whether `path` must be (re)downloaded: it is missing, or its size differs
/// from the published one. A size check costs one `stat`, which an existence
/// check needs anyway, and catches truncated or emptied files.
pub fn needs_download(path: &Path, size: Option<u64>) -> bool {
    match fs::metadata(path) {
        Ok(meta) => size.is_some_and(|expected| meta.len() != expected),
        Err(_) => true,
    }
}

/// Download `url` to `path` unless a file of the right size is already there.
///
/// When `sha1` is known the received bytes are hashed while streaming, so
/// verification costs no extra disk read. Transient failures are retried; a
/// checksum mismatch is retried as well because it is usually a truncated or
/// corrupted transfer.
pub async fn download_verified(
    client: &Client,
    url: &str,
    path: &Path,
    sha1: Option<&str>,
    size: Option<u64>,
    quiet: bool,
) -> Result<()> {
    if !needs_download(path, size) {
        return Ok(());
    }
    if path.exists() {
        if !quiet {
            println!("[!] {} has the wrong size; re-downloading.", path.display());
        }
        fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !quiet {
        if let Some(name) = path.file_name() {
            println!("Downloading {}...", name.to_string_lossy());
        }
    }

    let mut last_error = None;
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        match try_download(client, url, path, sha1, size).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt < DOWNLOAD_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
                }
            }
        }
    }
    Err(format!(
        "Failed to download {url} after {DOWNLOAD_ATTEMPTS} attempts: {}",
        last_error.map(|e| e.to_string()).unwrap_or_default()
    )
    .into())
}

async fn try_download(
    client: &Client,
    url: &str,
    path: &Path,
    sha1: Option<&str>,
    size: Option<u64>,
) -> Result<()> {
    let mut resp = client.get(url).send().await?.error_for_status()?;

    // Write to a temporary file first so interrupted downloads never leave a
    // partial file that would later be mistaken for a completed download.
    let part_path = part_path(path);
    let mut hasher = sha1_smol::Sha1::new();
    let mut written: u64 = 0;
    {
        let mut file = io::BufWriter::new(File::create(&part_path)?);
        while let Some(chunk) = resp.chunk().await? {
            hasher.update(&chunk);
            written += chunk.len() as u64;
            file.write_all(&chunk)?;
        }
        file.flush()?;
    }

    if let Some(expected) = size {
        if written != expected {
            let _ = fs::remove_file(&part_path);
            return Err(format!("size mismatch (expected {expected} bytes, got {written})").into());
        }
    }

    if let Some(expected) = sha1 {
        let actual = hasher.digest().to_string();
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&part_path);
            return Err(format!("checksum mismatch (expected {expected}, got {actual})").into());
        }
    }
    fs::rename(&part_path, path)?;
    Ok(())
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

fn hash_file(path: &Path, mut update: impl FnMut(&[u8])) -> Result<()> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        update(&buf[..n]);
    }
}

pub fn file_sha1(path: &Path) -> Result<String> {
    let mut hasher = sha1_smol::Sha1::new();
    hash_file(path, |b| hasher.update(b))?;
    Ok(hasher.digest().to_string())
}

pub fn file_sha256(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hash_file(path, |b| hasher.update(b))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub fn extract_zip(archive_path: &Path, target_dir: &Path, only_natives: bool) -> Result<()> {
    fs::create_dir_all(target_dir)?;
    let mut archive = zip::ZipArchive::new(File::open(archive_path)?)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if only_natives {
            // Native archives are untrusted input. Keeping only the file name
            // prevents an archive entry from writing outside `target_dir`.
            if !NATIVE_EXTS.iter().any(|ext| name.ends_with(ext)) || name.starts_with("META-INF") {
                continue;
            }
            let Some(file_name) = Path::new(&name).file_name() else {
                continue;
            };
            let out_path = target_dir.join(file_name);
            io::copy(&mut entry, &mut File::create(&out_path)?)?;
        } else {
            let out_path = match entry.enclosed_name() {
                Some(p) => target_dir.join(p),
                None => continue,
            };
            if entry.is_dir() {
                fs::create_dir_all(&out_path)?;
            } else {
                if let Some(p) = out_path.parent() {
                    fs::create_dir_all(p)?;
                }
                io::copy(&mut entry, &mut File::create(&out_path)?)?;
            }
        }
    }
    Ok(())
}

/// Read one file from a zip archive (e.g. `version.json` inside an installer).
pub fn read_zip_entry(archive_path: &Path, entry_name: &str) -> Result<Option<String>> {
    let mut archive = zip::ZipArchive::new(File::open(archive_path)?)?;
    let Ok(mut entry) = archive.by_name(entry_name) else {
        return Ok(None);
    };
    let mut text = String::new();
    entry.read_to_string(&mut text)?;
    Ok(Some(text))
}
