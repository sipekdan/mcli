//! Asset-index synchronization and legacy asset-folder reconstruction.

use crate::config::{Paths, Result};
use crate::http::download_file;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub async fn sync_assets(
    client: &Client,
    paths: &Paths,
    blueprint: &Value,
    game_dir: Option<&Path>,
    fast: bool,
) -> Result<PathBuf> {
    let Some(index_info) = blueprint.get("assetIndex") else {
        return Ok(paths.assets.clone());
    };
    let index_id = index_info["id"].as_str().unwrap_or("legacy");
    crate::config::validate_id(index_id)?;
    let index_url = index_info["url"].as_str().ok_or("Missing assetIndex url")?;

    let index_path = paths
        .assets
        .join("indexes")
        .join(format!("{}.json", index_id));
    download_file(client, index_url, &index_path, true).await?;

    let index_data: Value = serde_json::from_str(&fs::read_to_string(&index_path)?)?;

    let map_to_resources = index_data["map_to_resources"].as_bool().unwrap_or(false);
    let is_virtual = index_data["virtual"].as_bool().unwrap_or(false)
        || blueprint["assets"].as_str() == Some("legacy")
        || index_id.contains("virtual");

    // Old Minecraft versions expect a normal resource tree instead of the
    // modern content-addressed `objects/<hash>` layout.
    let target_reconstruct_dir = if map_to_resources {
        if let Some(gdir) = game_dir {
            gdir.join("resources")
        } else {
            paths.assets.join("virtual").join(index_id)
        }
    } else {
        paths.assets.join("virtual").join(index_id)
    };

    if fast
        && paths.assets.join("objects").exists()
        && (!(is_virtual || map_to_resources) || target_reconstruct_dir.exists())
    {
        println!("[!] Fast launch enabled. Skipping asset verification.");
        return Ok(if is_virtual || map_to_resources {
            target_reconstruct_dir
        } else {
            paths.assets.clone()
        });
    }

    let Some(objects) = index_data["objects"].as_object() else {
        return Ok(paths.assets.clone());
    };
    for (key, value) in objects {
        let hash = value["hash"].as_str().ok_or("Missing asset hash")?;
        if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("Invalid asset hash: {hash:?}").into());
        }
        // Resource names use slash-separated relative paths on every platform.
        for component in key.split('/') {
            crate::config::validate_id(component)?;
        }
    }
    // Multiple resource names can reference the same object hash. Download
    // each object once; otherwise concurrent tasks would race on one `.part` file.
    let mut seen_hashes = HashSet::new();
    let tasks: Vec<(String, PathBuf)> = objects
        .values()
        .filter_map(|val| {
            let hash = val["hash"].as_str()?;
            if hash.len() < 2 || !seen_hashes.insert(hash) {
                return None;
            }
            let sub = format!("{}/{}", &hash[..2], hash);
            let path = paths.assets.join("objects").join(&sub);
            (!path.exists()).then(|| {
                (
                    format!("https://resources.download.minecraft.net/{}", sub),
                    path,
                )
            })
        })
        .collect();

    if tasks.is_empty() {
        println!("Assets are up to date.");
    } else {
        println!(
            "Downloading {} asset{}...",
            tasks.len(),
            if tasks.len() == 1 { "" } else { "s" }
        );
    }
    let pb = ProgressBar::new(tasks.len() as u64);
    pb.set_style(ProgressStyle::with_template(
        "  {spinner:.cyan} {wide_bar:.cyan/blue} {pos}/{len} • {eta}",
    )?);
    pb.enable_steady_tick(std::time::Duration::from_millis(100));

    // Asset files are independent, so bounded concurrency improves a first
    // install without overwhelming Mojang's CDN or the local connection.
    let mut stream = stream::iter(tasks)
        .map(|(url, path)| {
            let c = client.clone();
            async move { download_file(&c, &url, &path, true).await }
        })
        .buffer_unordered(16);

    while let Some(res) = stream.next().await {
        res?;
        pb.inc(1);
    }
    pb.finish_with_message("assets ready");

    if is_virtual || map_to_resources {
        println!(
            "Reconstructing legacy assets into {}...",
            target_reconstruct_dir.display()
        );
        for (key, val) in objects {
            if let Some(hash) = val["hash"].as_str() {
                let obj_path = paths.assets.join("objects").join(&hash[..2]).join(hash);
                let out_path = target_reconstruct_dir.join(key);
                if !out_path.exists() && obj_path.exists() {
                    if let Some(p) = out_path.parent() {
                        fs::create_dir_all(p)?;
                    }
                    fs::copy(&obj_path, &out_path)?;
                }
            }
        }
        return Ok(target_reconstruct_dir);
    }

    Ok(paths.assets.clone())
}
