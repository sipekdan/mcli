//! Mod-loader installation: Fabric, Quilt, Forge and NeoForge.
//!
//! Every loader ends up as an ordinary version JSON in `versions/<id>/` that
//! `inheritsFrom` the vanilla version, so launching a modded version uses
//! exactly the same pipeline as vanilla.
//!
//! - Fabric / Quilt publish a ready-made version JSON through their meta APIs.
//! - Legacy Forge installers (<= 1.12.2 early builds) embed a version JSON and
//!   the universal jar, which mcli unpacks itself.
//! - Modern Forge / NeoForge must patch the game jar, which only their
//!   installer can do; mcli runs it headlessly with `--installClient`.

use crate::config::{validate_id, Paths, Result};
use crate::http::{download_file, get_json, get_text, read_zip_entry};
use crate::rules::compare_versions;
use crate::version::{maven_to_path, read_local_version, resolve_blueprint, version_json_path};
use clap::ValueEnum;
use reqwest::Client;
use serde_json::Value;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::process::Command;

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loader {
    Vanilla,
    Fabric,
    Quilt,
    Forge,
    #[value(alias = "neo")]
    Neoforge,
}

impl fmt::Display for Loader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Loader::Vanilla => "Vanilla",
            Loader::Fabric => "Fabric",
            Loader::Quilt => "Quilt",
            Loader::Forge => "Forge",
            Loader::Neoforge => "NeoForge",
        })
    }
}

#[derive(Debug, Clone)]
pub struct LoaderVersion {
    pub version: String,
    pub stable: bool,
    pub recommended: bool,
}

const FABRIC_META: &str = "https://meta.fabricmc.net/v2";
const QUILT_META: &str = "https://meta.quiltmc.org/v3";
const FORGE_MAVEN: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge";
const FORGE_PROMOS: &str =
    "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
const NEOFORGE_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";
const NEOFORGE_VERSIONS: &str =
    "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge";

/// Available loader versions for a game version, newest first.
pub async fn list_versions(
    client: &Client,
    loader: Loader,
    mc: &str,
) -> Result<Vec<LoaderVersion>> {
    let list = match loader {
        Loader::Vanilla => return Err("Vanilla has no loader versions".into()),
        Loader::Fabric | Loader::Quilt => {
            let base = if loader == Loader::Fabric {
                FABRIC_META
            } else {
                QUILT_META
            };
            let data = get_json(client, &format!("{base}/versions/loader/{mc}"), None).await?;
            data.as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let l = &entry["loader"];
                    let version = l["version"].as_str()?.to_string();
                    // Quilt has no `stable` flag; it marks pre-releases in the name.
                    let stable = l["stable"]
                        .as_bool()
                        .unwrap_or_else(|| !is_prerelease(&version));
                    Some(LoaderVersion {
                        version,
                        stable,
                        recommended: false,
                    })
                })
                .collect()
        }
        Loader::Forge => {
            let xml = get_text(client, &format!("{FORGE_MAVEN}/maven-metadata.xml")).await?;
            let promos = get_json(client, FORGE_PROMOS, None)
                .await
                .unwrap_or(Value::Null);
            let recommended = promos["promos"][format!("{mc}-recommended")].as_str();
            let prefix = format!("{mc}-");
            let mut list: Vec<LoaderVersion> = xml_versions(&xml)
                .filter_map(|full| full.strip_prefix(&prefix))
                // Old builds repeat the game version as a suffix: 10.13.4.1614-1.7.10
                .map(|v| v.split('-').next().unwrap_or(v).to_string())
                .map(|version| LoaderVersion {
                    stable: true,
                    recommended: Some(version.as_str()) == recommended,
                    version,
                })
                .collect();
            list.sort_by(|a, b| compare_versions(&b.version, &a.version));
            list.dedup_by(|a, b| a.version == b.version);
            list
        }
        Loader::Neoforge => {
            let data = get_json(client, NEOFORGE_VERSIONS, None).await?;
            let prefix = neoforge_prefix(mc);
            let mut list: Vec<LoaderVersion> = data["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|v| prefix.as_deref().is_some_and(|p| v.starts_with(p)))
                .map(|v| LoaderVersion {
                    version: v.to_string(),
                    stable: !is_prerelease(v),
                    recommended: false,
                })
                .collect();
            list.sort_by(|a, b| compare_versions(&b.version, &a.version));
            list
        }
    };
    if list.is_empty() {
        let hint = if loader == Loader::Neoforge && mc == "1.20.1" {
            " (NeoForge for 1.20.1 is Forge-compatible; use --loader forge)"
        } else {
            ""
        };
        return Err(format!("{loader} has no builds for Minecraft {mc}{hint}").into());
    }
    Ok(list)
}

fn is_prerelease(v: &str) -> bool {
    let v = v.to_ascii_lowercase();
    v.contains("beta") || v.contains("alpha") || v.contains("-rc") || v.contains("pre")
}

fn xml_versions(xml: &str) -> impl Iterator<Item = &str> {
    xml.split("<version>")
        .skip(1)
        .filter_map(|chunk| chunk.split_once("</version>").map(|(v, _)| v.trim()))
}

/// NeoForge versions encode the game version: 1.21.1 -> 21.1.x,
/// 1.21 -> 21.0.x, and since the 2026 scheme 26.1 -> 26.1.0.x, 26.1.1 -> 26.1.1.x.
fn neoforge_prefix(mc: &str) -> Option<String> {
    let parts: Vec<&str> = mc.split('.').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    match parts.as_slice() {
        ["1", minor] => Some(format!("{minor}.0.")),
        ["1", minor, patch] => Some(format!("{minor}.{patch}.")),
        [year, drop] => Some(format!("{year}.{drop}.0.")),
        [year, drop, patch] => Some(format!("{year}.{drop}.{patch}.")),
        _ => None,
    }
}

fn pick(
    list: &[LoaderVersion],
    requested: Option<&str>,
    loader: Loader,
    mc: &str,
) -> Result<String> {
    if let Some(req) = requested {
        return list
            .iter()
            .find(|v| v.version == req)
            .map(|v| v.version.clone())
            .ok_or_else(|| {
                format!("{loader} {req} is not available for Minecraft {mc}. See: mcli version loaders {} {mc}", loader.to_string().to_lowercase()).into()
            });
    }
    Ok(list
        .iter()
        .find(|v| v.recommended)
        .or_else(|| list.iter().find(|v| v.stable))
        .unwrap_or(&list[0])
        .version
        .clone())
}

/// Does `id` (an installed version) belong to `loader` on game version `mc`?
fn is_loader_version(paths: &Paths, id: &str, loader: Loader, mc: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    let matches_name = match loader {
        Loader::Vanilla => return false,
        Loader::Fabric => lower.starts_with("fabric-loader-"),
        Loader::Quilt => lower.starts_with("quilt-loader-"),
        Loader::Forge => lower.contains("forge") && !lower.starts_with("neoforge"),
        Loader::Neoforge => lower.starts_with("neoforge-"),
    };
    matches_name
        && read_local_version(paths, id).is_some_and(|v| v["inheritsFrom"].as_str() == Some(mc))
}

/// Newest locally installed version of `loader` for `mc`, if any.
pub fn find_installed(paths: &Paths, loader: Loader, mc: &str) -> Option<String> {
    let mut found: Vec<String> = fs::read_dir(&paths.versions)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|id| is_loader_version(paths, id, loader, mc))
        .collect();
    found.sort_by(|a, b| compare_versions(b, a));
    found.into_iter().next()
}

/// Use an installed loader if one matches (no network), otherwise install.
pub async fn ensure(
    client: &Client,
    paths: &Paths,
    loader: Loader,
    mc: &str,
    requested: Option<&str>,
) -> Result<String> {
    if loader == Loader::Vanilla {
        return Ok(mc.to_string());
    }
    if requested.is_none() {
        if let Some(id) = find_installed(paths, loader, mc) {
            return Ok(id);
        }
    }
    install(client, paths, loader, mc, requested).await
}

/// Install a loader version and return the resulting version id.
pub async fn install(
    client: &Client,
    paths: &Paths,
    loader: Loader,
    mc: &str,
    requested: Option<&str>,
) -> Result<String> {
    validate_id(mc)?;
    let list = list_versions(client, loader, mc).await?;
    let version = pick(&list, requested, loader, mc)?;
    println!("=== Installing {loader} {version} for Minecraft {mc} ===");
    match loader {
        Loader::Vanilla => Ok(mc.to_string()),
        Loader::Fabric | Loader::Quilt => {
            let base = if loader == Loader::Fabric {
                FABRIC_META
            } else {
                QUILT_META
            };
            let url = format!("{base}/versions/loader/{mc}/{version}/profile/json");
            let profile = get_json(client, &url, None).await?;
            save_version_json(paths, profile)
        }
        Loader::Forge => {
            // The Maven artifact may carry a game-version suffix on old builds.
            let xml = get_text(client, &format!("{FORGE_MAVEN}/maven-metadata.xml")).await?;
            let exact = format!("{mc}-{version}");
            let full = xml_versions(&xml)
                .find(|v| *v == exact || v.starts_with(&format!("{exact}-")))
                .unwrap_or(&exact)
                .to_string();
            println!(
                "[i] Forge is funded by ads on its download page. If you use it a lot, \
                 consider supporting it: https://www.patreon.com/LexManos/"
            );
            let url = format!("{FORGE_MAVEN}/{full}/forge-{full}-installer.jar");
            install_from_installer(client, paths, &url, mc).await
        }
        Loader::Neoforge => {
            let url = format!("{NEOFORGE_MAVEN}/{version}/neoforge-{version}-installer.jar");
            install_from_installer(client, paths, &url, mc).await
        }
    }
}

fn save_version_json(paths: &Paths, json: Value) -> Result<String> {
    let id = json["id"]
        .as_str()
        .ok_or("Loader profile has no id")?
        .to_string();
    validate_id(&id)?;
    let path = version_json_path(paths, &id);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    Ok(id)
}

async fn install_from_installer(
    client: &Client,
    paths: &Paths,
    url: &str,
    mc: &str,
) -> Result<String> {
    fs::create_dir_all(&paths.cache)?;
    let file_name = url.rsplit('/').next().unwrap_or("installer.jar");
    let jar = paths.cache.join(file_name);
    download_file(client, url, &jar, false).await?;
    let result = run_installer(client, paths, &jar, mc).await;
    let _ = fs::remove_file(&jar);
    let _ = fs::remove_file(paths.cache.join(format!("{file_name}.log")));
    result
}

async fn run_installer(
    client: &Client,
    paths: &Paths,
    jar: &std::path::Path,
    mc: &str,
) -> Result<String> {
    let profile: Value = serde_json::from_str(
        &read_zip_entry(jar, "install_profile.json")?
            .ok_or("Installer has no install_profile.json")?,
    )?;

    // Legacy format: the full version JSON and the universal jar are embedded.
    if let Some(info) = profile.get("versionInfo") {
        let install = &profile["install"];
        let lib_coord = install["path"]
            .as_str()
            .ok_or("Legacy installer has no install.path")?;
        let embedded = install["filePath"]
            .as_str()
            .ok_or("Legacy installer has no install.filePath")?;
        let rel = maven_to_path(lib_coord).ok_or("Invalid Forge library coordinate")?;
        crate::config::validate_relative_path(&rel)?;
        let dest = paths.libraries.join(rel);
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut archive = zip::ZipArchive::new(File::open(jar)?)?;
        let mut entry = archive.by_name(embedded)?;
        io::copy(&mut entry, &mut File::create(&dest)?)?;
        return save_version_json(paths, info.clone());
    }

    // Modern format: the installer must run to patch the game jar.
    let version_json: Value = serde_json::from_str(
        &read_zip_entry(jar, "version.json")?.ok_or("Installer has no version.json")?,
    )?;
    let id = version_json["id"]
        .as_str()
        .ok_or("Installer version.json has no id")?
        .to_string();
    validate_id(&id)?;
    if version_json_path(paths, &id).exists() {
        println!("[!] {id} is already installed.");
        return Ok(id);
    }

    // The installer reuses our vanilla files and needs a Java runtime.
    let vanilla = resolve_blueprint(client, paths, mc).await?;
    crate::launcher::download_game_files(client, paths, &vanilla, None).await?;
    let java = crate::runtime::ensure_java(client, paths, &vanilla, None).await?;

    // Installers refuse to run unless the target looks like an official
    // launcher directory. Provide a throwaway profile file for the duration.
    crate::profile::migrate_legacy_profiles(paths)?;
    let stub = &paths.legacy_profiles_file;
    let created_stub = !stub.exists();
    if created_stub {
        fs::write(stub, r#"{"profiles":{}}"#)?;
    }
    println!("[!] Running installer (this patches the game and can take a minute)...");
    let status = Command::new(&java)
        .arg("-jar")
        .arg(jar)
        .arg("--installClient")
        .arg(&paths.base)
        .current_dir(&paths.cache)
        .status();
    if created_stub {
        let _ = fs::remove_file(stub);
    }
    let status = status?;
    if !status.success() || !version_json_path(paths, &id).exists() {
        return Err(format!("Installer failed ({status}). See the output above.").into());
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_game_versions_to_neoforge_prefixes() {
        assert_eq!(neoforge_prefix("1.21.1").as_deref(), Some("21.1."));
        assert_eq!(neoforge_prefix("1.21").as_deref(), Some("21.0."));
        assert_eq!(neoforge_prefix("26.1").as_deref(), Some("26.1.0."));
        assert_eq!(neoforge_prefix("26.1.1").as_deref(), Some("26.1.1."));
        assert_eq!(neoforge_prefix("26.4-snapshot-1"), None);
    }

    #[test]
    fn parses_maven_metadata_versions() {
        let xml = "<metadata><versioning><versions><version>1.21.1-52.1.20</version>\n<version>1.7.10-10.13.4.1614-1.7.10</version></versions></versioning></metadata>";
        assert_eq!(
            xml_versions(xml).collect::<Vec<_>>(),
            ["1.21.1-52.1.20", "1.7.10-10.13.4.1614-1.7.10"]
        );
    }

    #[test]
    fn picks_recommended_then_stable() {
        let v = |version: &str, stable, recommended| LoaderVersion {
            version: version.into(),
            stable,
            recommended,
        };
        let list = [
            v("3-beta", false, false),
            v("2", true, false),
            v("1", true, true),
        ];
        assert_eq!(pick(&list, None, Loader::Forge, "x").unwrap(), "1");
        assert_eq!(pick(&list[..2], None, Loader::Forge, "x").unwrap(), "2");
        assert_eq!(
            pick(&list, Some("3-beta"), Loader::Forge, "x").unwrap(),
            "3-beta"
        );
        assert!(pick(&list, Some("9"), Loader::Forge, "x").is_err());
    }
}
