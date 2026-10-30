//! Shared configuration, platform constants, and the launcher's data layout.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Identifiers used as directory names must remain a single portable component.
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.ends_with(['.', ' '])
        || id
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
    {
        return Err(format!("Invalid profile or version identifier: {id:?}").into());
    }
    Ok(())
}

pub fn validate_relative_path(path: &str) -> Result<()> {
    for component in path.split('/') {
        validate_id(component)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub const MOJANG_OS: &str = "osx";
#[cfg(target_os = "macos")]
pub const ADOPTIUM_OS: &str = "mac";
#[cfg(target_os = "macos")]
pub const CLASSPATH_SEP: &str = ":";
#[cfg(target_os = "macos")]
pub const NATIVE_EXTS: &[&str] = &[".dylib", ".jnilib"];

#[cfg(target_os = "linux")]
pub const MOJANG_OS: &str = "linux";
#[cfg(target_os = "linux")]
pub const ADOPTIUM_OS: &str = "linux";
#[cfg(target_os = "linux")]
pub const CLASSPATH_SEP: &str = ":";
#[cfg(target_os = "linux")]
pub const NATIVE_EXTS: &[&str] = &[".so"];

#[cfg(target_os = "windows")]
pub const MOJANG_OS: &str = "windows";
#[cfg(target_os = "windows")]
pub const ADOPTIUM_OS: &str = "windows";
#[cfg(target_os = "windows")]
pub const CLASSPATH_SEP: &str = ";";
#[cfg(target_os = "windows")]
pub const NATIVE_EXTS: &[&str] = &[".dll"];

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
pub const ADOPTIUM_ARCH: &str = "aarch64";
#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
pub const ADOPTIUM_ARCH: &str = "x64";

pub const MANIFEST_URL: &str = "https://launchermeta.mojang.com/mc/game/version_manifest_v2.json";
// Preserve the application identity used by existing installations and tokens.
const DEFAULT_MSA_CLIENT_ID: &str = "00000000441cc96b";

fn resolve_msa_client_id(override_id: Option<&str>) -> &str {
    override_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(DEFAULT_MSA_CLIENT_ID)
}

pub fn msa_client_id() -> String {
    let override_id = std::env::var("MCLI_CLIENT_ID").ok();
    resolve_msa_client_id(override_id.as_deref()).to_string()
}
pub const MSA_SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL";

pub const DEFAULT_JVM_ARGS: &str = "-Xmx2G -XX:+UnlockExperimentalVMOptions -XX:+UseG1GC \
    -XX:G1NewSizePercent=20 -XX:G1ReservePercent=20 -XX:MaxGCPauseMillis=50 \
    -XX:G1HeapRegionSize=32M -Dfile.encoding=UTF-8 -Dsun.stdout.encoding=UTF-8 -Dsun.stderr.encoding=UTF-8";

pub struct Paths {
    /// Rooted at `~/.minecraft-cli`; keeps launcher data out of the official
    /// Minecraft directory.
    pub base: PathBuf,
    pub versions: PathBuf,
    pub libraries: PathBuf,
    pub assets: PathBuf,
    pub profiles: PathBuf,
    pub java: PathBuf,
    pub profiles_file: PathBuf,
    pub auth_file: PathBuf,
}

impl Paths {
    pub fn new() -> Self {
        // Keep all managed files under one directory so profiles, caches, and
        // downloaded runtimes can be located consistently on every platform.
        let base = dirs::home_dir()
            .expect("Could not find home directory")
            .join(".minecraft-cli");
        Self {
            versions: base.join("versions"),
            libraries: base.join("libraries"),
            assets: base.join("assets"),
            profiles: base.join("profiles"),
            java: base.join("runtimes"),
            profiles_file: base.join("launcher_profiles.json"),
            auth_file: base.join("auth.json"),
            base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_msa_client_id, validate_id, validate_relative_path};

    #[test]
    fn preserves_original_authentication_identity_unless_overridden() {
        assert_eq!(resolve_msa_client_id(None), "00000000441cc96b");
        assert_eq!(resolve_msa_client_id(Some("  ")), "00000000441cc96b");
        assert_eq!(resolve_msa_client_id(Some(" custom-app ")), "custom-app");
    }

    #[test]
    fn rejects_paths_that_escape_managed_directories() {
        for id in [
            "",
            ".",
            "..",
            "../saves",
            "a/b",
            "a\\b",
            "C:drive",
            "/absolute",
            "a.",
        ] {
            assert!(validate_id(id).is_err(), "accepted {id:?}");
        }
        assert!(validate_id("1.21.1-forge-1.0").is_ok());
        assert!(validate_id("My Survival").is_ok());
    }

    #[test]
    fn validates_nested_download_paths() {
        assert!(validate_relative_path("org/example/library.jar").is_ok());
        for path in [
            "../outside.jar",
            "org/../../outside.jar",
            "/absolute.jar",
            "C:/outside.jar",
            "org\\..\\outside.jar",
        ] {
            assert!(validate_relative_path(path).is_err(), "accepted {path:?}");
        }
    }
}
