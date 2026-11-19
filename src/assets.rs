//! Asset-index synchronization and legacy asset-folder reconstruction.

use crate::config::{Paths, Result};
use crate::files::{fetch_missing, FileEntry, Kind};
use crate::http::{download_verified, file_sha1, needs_download};
use reqwest::Client;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub struct AssetSet {
    /// Content-addressed objects under `assets/objects/<xx>/<hash>`.
    pub objects: Vec<FileEntry>,
    /// Old versions read a plain resource tree; each entry is a copy of the
    /// object at the paired path.
    pub copies: Vec<(FileEntry, PathBuf)>,
    /// The directory passed to the game as `${game_assets}`.
    pub game_assets: PathBuf,
}

/// Fetch (or refresh) the asset index and list every file it describes.
pub async fn load_assets(
    client: &Client,
    paths: &Paths,
    blueprint: &Value,
    game_dir: Option<&Path>,
) -> Result<Option<AssetSet>> {
    let Some(index_info) = blueprint.get("assetIndex") else {
        return Ok(None);
    };
    let index_id = index_info["id"].as_str().unwrap_or("legacy");
    crate::config::validate_id(index_id)?;
    let index_url = index_info["url"].as_str().ok_or("Missing assetIndex url")?;

    let index_path = paths
        .assets
        .join("indexes")
        .join(format!("{index_id}.json"));
    // Mojang occasionally republishes an index under the same id. The file is
    // small, so re-check it against the version's checksum and refresh it.
    let index_sha1 = index_info["sha1"].as_str();
    if let Some(expected) = index_sha1 {
        if index_path.exists() && !file_sha1(&index_path)?.eq_ignore_ascii_case(expected) {
            fs::remove_file(&index_path)?;
        }
    }
    download_verified(
        client,
        index_url,
        &index_path,
        index_sha1,
        index_info["size"].as_u64(),
        true,
    )
    .await?;
    let index: Value = serde_json::from_str(&fs::read_to_string(&index_path)?)?;

    let map_to_resources = index["map_to_resources"].as_bool().unwrap_or(false);
    let is_virtual = index["virtual"].as_bool().unwrap_or(false)
        || blueprint["assets"].as_str() == Some("legacy");

    // Old Minecraft versions expect a normal resource tree instead of the
    // modern content-addressed `objects/<hash>` layout.
    let reconstruct_dir = match (map_to_resources, game_dir) {
        (true, Some(dir)) => Some(dir.join("resources")),
        _ if map_to_resources || is_virtual => Some(paths.assets.join("virtual").join(index_id)),
        _ => None,
    };

    let mut objects = Vec::new();
    let mut copies = Vec::new();
    for (key, value) in index["objects"].as_object().into_iter().flatten() {
        let hash = value["hash"].as_str().ok_or("Missing asset hash")?;
        if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("Invalid asset hash: {hash:?}").into());
        }
        // Resource names use slash-separated relative paths on every platform.
        for component in key.split('/') {
            crate::config::validate_id(component)?;
        }
        let sub = format!("{}/{}", &hash[..2], hash);
        let object = FileEntry {
            path: paths.assets.join("objects").join(&sub),
            url: format!("https://resources.download.minecraft.net/{sub}"),
            // Asset objects are content-addressed: the file name is its SHA-1.
            sha1: Some(hash.to_ascii_lowercase()),
            size: value["size"].as_u64(),
            name: key.clone(),
            kind: Kind::Asset,
        };
        if let Some(dir) = &reconstruct_dir {
            let copy = FileEntry {
                path: dir.join(key),
                url: String::new(),
                ..object.clone()
            };
            copies.push((copy, object.path.clone()));
        }
        objects.push(object);
    }

    Ok(Some(AssetSet {
        objects,
        copies,
        game_assets: reconstruct_dir.unwrap_or_else(|| paths.assets.clone()),
    }))
}

/// Make sure every asset is present with the right size; returns the
/// directory to pass as `${game_assets}`.
pub async fn sync_assets(
    client: &Client,
    paths: &Paths,
    blueprint: &Value,
    game_dir: Option<&Path>,
) -> Result<PathBuf> {
    let Some(set) = load_assets(client, paths, blueprint, game_dir).await? else {
        return Ok(paths.assets.clone());
    };
    fetch_missing(client, &set.objects, "assets").await?;

    let mut rebuilt = 0;
    for (copy, source) in &set.copies {
        if needs_download(&copy.path, copy.size) {
            if let Some(dir) = copy.path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::copy(source, &copy.path)?;
            rebuilt += 1;
        }
    }
    if rebuilt > 0 {
        println!(
            "Reconstructed {rebuilt} legacy asset files in {}",
            set.game_assets.display()
        );
    }
    Ok(set.game_assets)
}
