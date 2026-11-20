//! Persistent launch profiles and the `mcli profile` subcommands.

use crate::cli::{ProfileAction, ProfileOverrides};
use crate::config::{Paths, Result, DEFAULT_JVM_ARGS};
use crate::loader;
use crate::version::{base_game_version, resolve_alias};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Profile {
    pub version: String,
    pub game_dir: String,
    pub width: String,
    pub height: String,
    pub fullscreen: bool,
    pub java_bin: String,
    pub jvm_args: String,
    pub username: String,
    pub uuid: String,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: "1.20.4".to_string(),
            game_dir: String::new(),
            width: "auto".to_string(),
            height: "auto".to_string(),
            fullscreen: false,
            java_bin: "bundled".to_string(),
            jvm_args: DEFAULT_JVM_ARGS.to_string(),
            username: "Player".to_string(),
            uuid: "0".to_string(),
        }
    }
}

impl Profile {
    pub fn new(paths: &Paths, name: &str, version: Option<&str>) -> Self {
        Self {
            version: version.unwrap_or(name).to_string(),
            game_dir: paths.profiles.join(name).to_string_lossy().to_string(),
            ..Default::default()
        }
    }
}

/// Move profiles saved by mcli < 1.1 out of `launcher_profiles.json`, a name
/// that mod-loader installers expect to belong to the official launcher.
pub fn migrate_legacy_profiles(paths: &Paths) -> Result<()> {
    let legacy = &paths.legacy_profiles_file;
    if paths.profiles_file.exists() || !legacy.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(legacy)?;
    let value: Value = serde_json::from_str(&content)?;
    // An official-launcher file has `profiles: { id: { lastVersionId, ... } }`;
    // mcli's own entries always carry a `version` field.
    let official = value
        .get("profiles")
        .and_then(Value::as_object)
        .is_some_and(|p| p.values().all(|v| v.get("version").is_none()));
    if official {
        return Ok(());
    }
    fs::write(&paths.profiles_file, &content)?;
    fs::remove_file(legacy)?;
    Ok(())
}

pub fn load_profiles(paths: &Paths) -> HashMap<String, Profile> {
    if let Err(error) = migrate_legacy_profiles(paths) {
        eprintln!("[!] Could not migrate old profile file: {error}");
    }
    // Invalid or absent configuration behaves like an empty profile list so a
    // user can still launch a version and recreate their settings.
    if paths.profiles_file.exists() {
        if let Ok(content) = fs::read_to_string(&paths.profiles_file) {
            if let Ok(mut map) = serde_json::from_str::<HashMap<String, Profile>>(&content) {
                for (name, prof) in map.iter_mut() {
                    if prof.game_dir.is_empty() {
                        prof.game_dir = paths.profiles.join(name).to_string_lossy().to_string();
                    }
                }
                return map;
            }
        }
    }
    HashMap::new()
}

pub fn save_profiles(paths: &Paths, profiles: &HashMap<String, Profile>) -> Result<()> {
    if let Some(p) = paths.profiles_file.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(
        &paths.profiles_file,
        serde_json::to_string_pretty(profiles)?,
    )?;
    Ok(())
}

pub fn apply_overrides(prof: &mut Profile, ov: &ProfileOverrides) {
    // Used both for saved profile edits and one-off `run` overrides.
    if let Some(v) = &ov.version {
        prof.version = v.clone();
    }
    if let Some(g) = &ov.game_dir {
        prof.game_dir = g.clone();
    }
    if let Some(w) = &ov.width {
        prof.width = w.clone();
    }
    if let Some(h) = &ov.height {
        prof.height = h.clone();
    }
    if ov.fullscreen {
        prof.fullscreen = true;
    }
    if ov.no_fullscreen {
        prof.fullscreen = false;
    }
    if let Some(j) = &ov.java_bin {
        prof.java_bin = j.clone();
    }
    if let Some(ja) = &ov.jvm_args {
        prof.jvm_args = ja.clone();
    }
    if let Some(u) = &ov.username {
        prof.username = u.clone();
    }
    if let Some(id) = &ov.uuid {
        prof.uuid = id.clone();
    }
}

/// One `profile list` row: name, version, max heap, window, directory.
fn list_row(paths: &Paths, name: &str, p: &Profile) -> [String; 5] {
    let memory = if p.jvm_args.trim() == DEFAULT_JVM_ARGS.trim() {
        "default".to_string()
    } else {
        p.jvm_args
            .split_whitespace()
            .filter_map(|a| a.strip_prefix("-Xmx"))
            .next_back()
            .unwrap_or("java default")
            .to_string()
    };
    let window = if p.fullscreen {
        "fullscreen".to_string()
    } else if p.width == "auto" || p.height == "auto" {
        "auto".to_string()
    } else {
        format!("{}x{}", p.width, p.height)
    };
    let dir = PathBuf::from(&p.game_dir);
    let directory = if dir == paths.profiles.join(name) {
        // The managed location; no need to repeat the long path.
        "(managed)".to_string()
    } else {
        match dirs::home_dir().and_then(|home| dir.strip_prefix(home).ok().map(PathBuf::from)) {
            Some(rel) => format!("~/{}", rel.display()),
            None => p.game_dir.clone(),
        }
    };
    [
        name.to_string(),
        p.version.clone(),
        memory,
        window,
        directory,
    ]
}

/// Print rows as columns sized to their widest cell.
fn print_table<const N: usize>(header: [&str; N], rows: &[[String; N]]) {
    let mut widths = header.map(|h| h.chars().count());
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let parts: Vec<String> = cells
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", parts.join("  ").trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

/// Apply `--loader` / `--loader-version`, installing the loader if needed.
/// The profile then simply stores the resulting version id.
pub async fn apply_loader(
    client: &Client,
    paths: &Paths,
    prof: &mut Profile,
    ov: &ProfileOverrides,
) -> Result<()> {
    let Some(loader) = ov.loader else {
        if ov.loader_version.is_some() {
            return Err("--loader-version requires --loader".into());
        }
        return Ok(());
    };
    let mc = resolve_alias(client, paths, &base_game_version(paths, &prof.version)).await?;
    prof.version = loader::ensure(client, paths, loader, &mc, ov.loader_version.as_deref()).await?;
    Ok(())
}

/// When a renamed profile uses its managed directory (`<root>/<old>`), move
/// that directory to `<root>/<new>` and point the profile at it. Custom game
/// directories are left alone. Returns `(from, to)` if files were moved.
fn move_managed_dir(
    root: &Path,
    old: &str,
    new: &str,
    prof: &mut Profile,
    others: &HashMap<String, Profile>,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let old_dir = root.join(old);
    if Path::new(&prof.game_dir) != old_dir {
        return Ok(None);
    }
    let new_dir = root.join(new);
    for (other, p) in others {
        let dir = Path::new(&p.game_dir);
        if dir == old_dir {
            return Err(format!(
                "Profile '{other}' also uses {}; not moving it",
                old_dir.display()
            )
            .into());
        }
        if dir == new_dir {
            return Err(format!("Profile '{other}' already uses {}", new_dir.display()).into());
        }
    }
    if new_dir.exists() {
        return Err(format!(
            "{} already exists. Move or remove it first.",
            new_dir.display()
        )
        .into());
    }
    prof.game_dir = new_dir.to_string_lossy().to_string();
    if !old_dir.exists() {
        // Never launched: nothing on disk to move.
        return Ok(None);
    }
    fs::rename(&old_dir, &new_dir)?;
    Ok(Some((old_dir, new_dir)))
}

pub async fn handle_profile_cmd(
    action: ProfileAction,
    client: &Client,
    paths: &Paths,
) -> Result<()> {
    let mut profiles = load_profiles(paths);
    match action {
        ProfileAction::List => {
            println!("=== Saved Profiles ===");
            if profiles.is_empty() {
                println!("No profiles created yet.");
            }
            let sorted: BTreeMap<_, _> = profiles.iter().collect();
            let rows: Vec<[String; 5]> = sorted
                .into_iter()
                .map(|(name, p)| list_row(paths, name, p))
                .collect();
            print_table(["NAME", "VERSION", "MEMORY", "WINDOW", "DIRECTORY"], &rows);
        }
        ProfileAction::Create { name, .. } if profiles.contains_key(&name) => {
            return Err(format!(
                "Profile '{name}' already exists. Use 'mcli profile edit {name} ...' to change it."
            )
            .into());
        }
        ProfileAction::Edit { name, .. } if !profiles.contains_key(&name) => {
            return Err(format!(
                "Profile '{name}' does not exist. Create it with 'mcli profile create'."
            )
            .into());
        }
        ProfileAction::Create { name, overrides } | ProfileAction::Edit { name, overrides } => {
            crate::config::validate_id(&name)?;
            let mut entry = profiles
                .get(&name)
                .cloned()
                .unwrap_or_else(|| Profile::new(paths, &name, overrides.version.as_deref()));
            apply_overrides(&mut entry, &overrides);
            apply_loader(client, paths, &mut entry, &overrides).await?;
            let version = entry.version.clone();
            profiles.insert(name.clone(), entry);
            save_profiles(paths, &profiles)?;
            println!(
                "[+] Profile '{}' saved successfully (version {version}).",
                name
            );
        }
        ProfileAction::Rename { name, new_name } => {
            crate::config::validate_id(&new_name)?;
            if profiles.contains_key(&new_name) {
                return Err(format!("Profile '{new_name}' already exists").into());
            }
            if let Some(mut prof) = profiles.remove(&name) {
                let moved =
                    move_managed_dir(&paths.profiles, &name, &new_name, &mut prof, &profiles)?;
                profiles.insert(new_name.clone(), prof);
                if let Err(error) = save_profiles(paths, &profiles) {
                    // Keep files and configuration in agreement.
                    if let Some((from, to)) = &moved {
                        let _ = fs::rename(to, from);
                    }
                    return Err(error);
                }
                println!("[+] Renamed profile '{}' -> '{}'.", name, new_name);
                if let Some((_, to)) = moved {
                    println!("[+] Moved game directory to {}", to.display());
                }
            } else {
                println!("[!] Profile '{}' does not exist.", name);
            }
        }
        ProfileAction::Remove { name, purge } => {
            // Validate before changing configuration or deleting any files.
            if purge {
                if let Some(prof) = profiles.get(&name) {
                    let gdir = PathBuf::from(&prof.game_dir);
                    if gdir.exists() {
                        let root = fs::canonicalize(&paths.profiles)?;
                        let target = fs::canonicalize(&gdir)?;
                        if target == root || !target.starts_with(&root) {
                            return Err("Refusing to purge a directory outside the managed profiles folder. Remove the profile without --purge and manage custom files manually.".into());
                        }
                        for (other, profile) in &profiles {
                            if other != &name {
                                if let Ok(other_dir) = fs::canonicalize(&profile.game_dir) {
                                    if other_dir.starts_with(&target) {
                                        return Err(
                                            "Cannot purge a directory used by another profile"
                                                .into(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(prof) = profiles.remove(&name) {
                save_profiles(paths, &profiles)?;
                println!("[+] Removed profile '{}' from configuration.", name);
                if purge {
                    let gdir = PathBuf::from(&prof.game_dir);
                    if gdir.exists() && gdir != paths.base {
                        fs::remove_dir_all(&gdir)?;
                        println!("[+] Purged game directory: {}", gdir.display());
                    }
                }
            } else {
                println!("[!] Profile '{}' does not exist.", name);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{move_managed_dir, Profile};
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mcli-rename-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn managed(root: &std::path::Path, name: &str) -> Profile {
        Profile {
            game_dir: root.join(name).to_string_lossy().to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn moves_managed_directory() {
        let root = temp_root("move");
        fs::create_dir_all(root.join("Old/mods")).unwrap();
        let mut prof = managed(&root, "Old");
        let moved = move_managed_dir(&root, "Old", "New", &mut prof, &HashMap::new()).unwrap();
        assert_eq!(moved, Some((root.join("Old"), root.join("New"))));
        assert!(root.join("New/mods").is_dir() && !root.join("Old").exists());
        assert_eq!(PathBuf::from(&prof.game_dir), root.join("New"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn updates_path_of_never_launched_profile() {
        let root = temp_root("unlaunched");
        let mut prof = managed(&root, "Old");
        assert_eq!(
            move_managed_dir(&root, "Old", "New", &mut prof, &HashMap::new()).unwrap(),
            None
        );
        assert_eq!(PathBuf::from(&prof.game_dir), root.join("New"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_custom_directory_alone() {
        let root = temp_root("custom");
        let mut prof = Profile {
            game_dir: "/somewhere/else".into(),
            ..Default::default()
        };
        assert_eq!(
            move_managed_dir(&root, "Old", "New", &mut prof, &HashMap::new()).unwrap(),
            None
        );
        assert_eq!(prof.game_dir, "/somewhere/else");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_existing_target_or_shared_directory() {
        let root = temp_root("refuse");
        fs::create_dir_all(root.join("Old")).unwrap();
        fs::create_dir_all(root.join("New")).unwrap();
        let mut prof = managed(&root, "Old");
        assert!(move_managed_dir(&root, "Old", "New", &mut prof, &HashMap::new()).is_err());
        assert_eq!(PathBuf::from(&prof.game_dir), root.join("Old"));

        fs::remove_dir(root.join("New")).unwrap();
        let others = HashMap::from([("Twin".to_string(), managed(&root, "Old"))]);
        assert!(move_managed_dir(&root, "Old", "New", &mut prof, &others).is_err());
        assert!(root.join("Old").is_dir() && !root.join("New").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
