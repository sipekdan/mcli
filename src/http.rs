//! Small HTTP and archive helpers shared by downloads and Microsoft login.

use crate::config::{Result, NATIVE_EXTS};
use reqwest::Client;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

pub fn build_http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("mcli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?)
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
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !quiet {
        if let Some(name) = path.file_name() {
            println!("Downloading {}...", name.to_string_lossy());
        }
    }
    let resp = client.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;

    // Write to a temporary file first so interrupted downloads never leave a
    // partial file that would later be mistaken for a completed download.
    let mut part_name = path.as_os_str().to_os_string();
    part_name.push(".part");
    let part_path = std::path::PathBuf::from(part_name);
    {
        let mut file = File::create(&part_path)?;
        file.write_all(&bytes)?;
    }
    fs::rename(&part_path, path)?;
    Ok(())
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
            let out_path = target_dir.join(Path::new(&name).file_name().unwrap());
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
