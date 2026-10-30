//! Persistent launch profiles and the `mcli profile` subcommands.

use crate::cli::{ProfileAction, ProfileOverrides};
use crate::config::{Paths, Result, DEFAULT_JVM_ARGS};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Profile {
    pub version: String,
    pub game_dir: String,
    pub width: String,
    pub height: String,
    pub fullscreen: bool,
    pub fast_launch: bool,
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
            fast_launch: false,
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

pub fn load_profiles(paths: &Paths) -> HashMap<String, Profile> {
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
    if ov.fast {
        prof.fast_launch = true;
    }
    if let Some(j) = &ov.java_bin {
        prof.java_bin = j.clone();
    }
    if let Some(ja) = &ov.jvm_args {
        prof.jvm_args = ja.clone();
    }
    if let Some(ram) = &ov.ram {
        let filtered: Vec<&str> = prof
            .jvm_args
            .split_whitespace()
            .filter(|t| !t.starts_with("-Xmx") && !t.starts_with("-Xms"))
            .collect();
        prof.jvm_args = format!("-Xms{} -Xmx{} {}", ram, ram, filtered.join(" "));
    }
    if let Some(u) = &ov.username {
        prof.username = u.clone();
    }
    if let Some(id) = &ov.uuid {
        prof.uuid = id.clone();
    }
}

pub fn handle_profile_cmd(action: ProfileAction, paths: &Paths) -> Result<()> {
    let mut profiles = load_profiles(paths);
    match action {
        ProfileAction::List => {
            println!("=== Saved Profiles ===");
            if profiles.is_empty() {
                println!("No profiles created yet.");
            }
            for (name, p) in &profiles {
                let res = if p.width == "auto" || p.height == "auto" {
                    "auto".to_string()
                } else {
                    format!("{}x{}", p.width, p.height)
                };
                println!(
                    "- {:15} | Ver: {:25} | Res: {:10} | Dir: {}",
                    name, p.version, res, p.game_dir
                );
            }
        }
        ProfileAction::Create { name, overrides } | ProfileAction::Edit { name, overrides } => {
            crate::config::validate_id(&name)?;
            let entry = profiles
                .entry(name.clone())
                .or_insert_with(|| Profile::new(paths, &name, overrides.version.as_deref()));
            apply_overrides(entry, &overrides);
            save_profiles(paths, &profiles)?;
            println!("[+] Profile '{}' saved successfully.", name);
        }
        ProfileAction::Rename { name, new_name } => {
            crate::config::validate_id(&new_name)?;
            if profiles.contains_key(&new_name) {
                return Err(format!("Profile '{new_name}' already exists").into());
            }
            if let Some(prof) = profiles.remove(&name) {
                profiles.insert(new_name.clone(), prof);
                save_profiles(paths, &profiles)?;
                println!("[+] Renamed profile '{}' -> '{}'.", name, new_name);
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
