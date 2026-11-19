//! Launch-argument assembly from version metadata.
//!
//! Both metadata formats are handled by one code path:
//! - 1.13+ (`arguments.jvm` / `arguments.game`): structured lists with rules.
//! - pre-1.13 (`minecraftArguments`): one whitespace-separated string.
//!
//! Placeholder expansion is strict. Any `${name}` that survives substitution
//! aborts the launch with a clear message instead of handing Minecraft a
//! literal `${version_type}` and hoping for the best.

use crate::config::{Result, CLASSPATH_SEP, LAUNCHER_NAME, LAUNCHER_VERSION};
use crate::rules::{rules_allow, Features};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

/// Every placeholder that occurs in Mojang's official version metadata
/// (all 925 versions as of 26.x), plus those Forge/NeoForge installers add.
/// `tests::every_known_placeholder_has_a_value` guarantees each one is
/// actually supplied when its feature is active.
pub const KNOWN_PLACEHOLDERS: &[&str] = &[
    // Game arguments
    "auth_player_name",
    "auth_session",
    "auth_uuid",
    "auth_access_token",
    "auth_xuid",
    "clientid",
    "user_type",
    "user_properties",
    "version_name",
    "version_type",
    "game_directory",
    "assets_root",
    "game_assets",
    "assets_index_name",
    "resolution_width",
    "resolution_height",
    "quickPlayPath",
    "quickPlaySingleplayer",
    "quickPlayMultiplayer",
    "quickPlayRealms",
    // JVM arguments
    "natives_directory",
    "launcher_name",
    "launcher_version",
    "classpath",
    // Mod loaders (Forge, NeoForge)
    "classpath_separator",
    "library_directory",
];

#[derive(Clone, Debug)]
pub enum QuickPlay {
    Singleplayer(String),
    Multiplayer(String),
}

/// Everything the launcher knows about this particular launch.
#[derive(Clone, Debug)]
pub struct LaunchInputs {
    pub username: String,
    pub uuid: String,
    pub access_token: String,
    pub user_type: String,
    pub xuid: String,
    pub client_id: String,
    pub version_name: String,
    pub version_type: String,
    pub game_dir: PathBuf,
    pub assets_root: PathBuf,
    /// Directory holding reconstructed legacy assets (equals `assets_root`
    /// for modern versions).
    pub game_assets: PathBuf,
    pub asset_index: String,
    pub natives_dir: PathBuf,
    pub library_dir: PathBuf,
    pub classpath: String,
    pub resolution: Option<(String, String)>,
    pub quick_play: Option<QuickPlay>,
    pub demo: bool,
}

impl LaunchInputs {
    pub fn features(&self) -> Features {
        let mut f = Features::default();
        if self.resolution.is_some() {
            f.enable("has_custom_resolution");
        }
        if self.demo {
            f.enable("is_demo_user");
        }
        match self.quick_play {
            Some(QuickPlay::Singleplayer(_)) => f.enable("is_quick_play_singleplayer"),
            Some(QuickPlay::Multiplayer(_)) => f.enable("is_quick_play_multiplayer"),
            None => {}
        }
        f
    }

    pub fn placeholder_values(&self) -> HashMap<&'static str, String> {
        let path = |p: &PathBuf| p.to_string_lossy().into_owned();
        let mut v = HashMap::from([
            ("auth_player_name", self.username.clone()),
            ("auth_uuid", self.uuid.clone()),
            ("auth_access_token", self.access_token.clone()),
            // Pre-1.6 metadata expects the old `token:<access>:<uuid>` session.
            (
                "auth_session",
                format!("token:{}:{}", self.access_token, self.uuid),
            ),
            ("auth_xuid", self.xuid.clone()),
            ("clientid", self.client_id.clone()),
            ("user_type", self.user_type.clone()),
            ("user_properties", "{}".to_string()),
            ("version_name", self.version_name.clone()),
            ("version_type", self.version_type.clone()),
            ("game_directory", path(&self.game_dir)),
            ("assets_root", path(&self.assets_root)),
            ("game_assets", path(&self.game_assets)),
            ("assets_index_name", self.asset_index.clone()),
            ("natives_directory", path(&self.natives_dir)),
            ("launcher_name", LAUNCHER_NAME.to_string()),
            ("launcher_version", LAUNCHER_VERSION.to_string()),
            ("classpath", self.classpath.clone()),
            ("classpath_separator", CLASSPATH_SEP.to_string()),
            ("library_directory", path(&self.library_dir)),
        ]);
        if let Some((w, h)) = &self.resolution {
            v.insert("resolution_width", w.clone());
            v.insert("resolution_height", h.clone());
        }
        match &self.quick_play {
            Some(QuickPlay::Singleplayer(world)) => {
                v.insert("quickPlaySingleplayer", world.clone());
            }
            Some(QuickPlay::Multiplayer(addr)) => {
                v.insert("quickPlayMultiplayer", addr.clone());
            }
            None => {}
        }
        v
    }
}

pub struct LaunchArgs {
    pub jvm: Vec<String>,
    pub game: Vec<String>,
}

/// JVM arguments the official launcher used before metadata carried them.
fn legacy_default_jvm() -> Value {
    json!([
        {"rules": [{"action": "allow", "os": {"name": "osx"}}], "value": ["-XstartOnFirstThread"]},
        {"rules": [{"action": "allow", "os": {"name": "windows"}}],
         "value": "-XX:HeapDumpPath=MojangTricksIntelDriversForPerformance_javaw.exe_minecraft.exe.heapdump"},
        {"rules": [{"action": "allow", "os": {"arch": "x86"}}], "value": "-Xss1M"},
        "-Djava.library.path=${natives_directory}",
        "-Dminecraft.launcher.brand=${launcher_name}",
        "-Dminecraft.launcher.version=${launcher_version}",
        "-cp",
        "${classpath}"
    ])
}

/// Whether this version's own arguments can express a feature (newer
/// versions use `--quickPlay*`/`--width`; older ones need manual flags).
pub fn declares_feature(blueprint: &Value, feature: &str) -> bool {
    blueprint["arguments"]["game"]
        .as_array()
        .is_some_and(|args| {
            args.iter().any(|a| {
                a["rules"]
                    .as_array()
                    .is_some_and(|rules| rules.iter().any(|r| r["features"].get(feature).is_some()))
            })
        })
}

pub fn build(blueprint: &Value, inputs: &LaunchInputs) -> Result<LaunchArgs> {
    let values = inputs.placeholder_values();
    let features = inputs.features();
    let mut missing = BTreeSet::new();

    let jvm = match blueprint["arguments"].get("jvm") {
        Some(section) => collect(section, &values, &features, &mut missing),
        None => collect(&legacy_default_jvm(), &values, &features, &mut missing),
    };

    let game = if let Some(section) = blueprint["arguments"].get("game") {
        collect(section, &values, &features, &mut missing)
    } else if let Some(line) = blueprint["minecraftArguments"].as_str() {
        line.split_whitespace()
            .map(|arg| substitute(arg, &values, &mut missing))
            .collect()
    } else {
        return Err("Version metadata has neither 'arguments' nor 'minecraftArguments'".into());
    };

    check_missing(&missing)?;
    Ok(LaunchArgs { jvm, game })
}

/// Flatten one argument list (strings, or `{rules, value}` objects whose value
/// is a string or array of strings), dropping entries whose rules fail.
pub fn collect(
    section: &Value,
    values: &HashMap<&'static str, String>,
    features: &Features,
    missing: &mut BTreeSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for entry in section.as_array().into_iter().flatten() {
        let items: Vec<&str> = match entry {
            Value::String(s) => vec![s.as_str()],
            Value::Object(obj) => {
                if !rules_allow(obj.get("rules"), features) {
                    continue;
                }
                match obj.get("value") {
                    Some(Value::String(s)) => vec![s.as_str()],
                    Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
                    _ => continue,
                }
            }
            _ => continue,
        };
        out.extend(items.into_iter().map(|s| substitute(s, values, missing)));
    }
    out
}

/// Replace every `${name}` in `template`. Unknown names are recorded in
/// `missing` and left in place so the error can show them.
pub fn substitute(
    template: &str,
    values: &HashMap<&'static str, String>,
    missing: &mut BTreeSet<String>,
) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match values.get(name) {
            Some(value) => out.push_str(value),
            None => {
                missing.insert(name.to_string());
                out.push_str(&rest[start..start + 2 + end + 1]);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

pub fn check_missing(missing: &BTreeSet<String>) -> Result<()> {
    if missing.is_empty() {
        return Ok(());
    }
    let (known, unknown): (Vec<&String>, Vec<&String>) = missing
        .iter()
        .partition(|m| KNOWN_PLACEHOLDERS.contains(&m.as_str()));
    let fmt = |v: &[&String]| {
        v.iter()
            .map(|s| format!("${{{s}}}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut msg = String::from("Refusing to launch with unresolved arguments:");
    if !known.is_empty() {
        msg += &format!(
            "\n  mcli did not supply a value for: {} (this is an mcli bug)",
            fmt(&known)
        );
    }
    if !unknown.is_empty() {
        msg += &format!(
            "\n  version metadata uses placeholders mcli does not know: {}",
            fmt(&unknown)
        );
    }
    Err(msg.into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn full_inputs() -> LaunchInputs {
        LaunchInputs {
            username: "Alex".into(),
            uuid: "uuid".into(),
            access_token: "token".into(),
            user_type: "msa".into(),
            xuid: "0".into(),
            client_id: "0".into(),
            version_name: "1.0".into(),
            version_type: "release".into(),
            game_dir: "game".into(),
            assets_root: "assets".into(),
            game_assets: "assets/virtual/legacy".into(),
            asset_index: "1.8".into(),
            natives_dir: "natives".into(),
            library_dir: "libraries".into(),
            classpath: "a.jar:b.jar".into(),
            resolution: Some(("854".into(), "480".into())),
            quick_play: Some(QuickPlay::Multiplayer("example.org:25565".into())),
            demo: true,
        }
    }

    #[test]
    fn every_known_placeholder_has_a_value() {
        // Quick play modes are mutually exclusive, so check each in turn.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for qp in [
            QuickPlay::Singleplayer("w".into()),
            QuickPlay::Multiplayer("m".into()),
        ] {
            let inputs = LaunchInputs {
                quick_play: Some(qp),
                ..full_inputs()
            };
            seen.extend(inputs.placeholder_values().keys().copied());
        }
        // These belong to features mcli never enables: `has_quick_plays_support`
        // (a quick-play log file) and `is_quick_play_realms`.
        let never_enabled = ["quickPlayPath", "quickPlayRealms"];
        let unsupplied: Vec<_> = KNOWN_PLACEHOLDERS
            .iter()
            .filter(|p| !seen.contains(**p) && !never_enabled.contains(p))
            .collect();
        assert!(unsupplied.is_empty(), "no value for {unsupplied:?}");
    }

    #[test]
    fn expands_legacy_minecraft_arguments() {
        let bp = json!({
            "minecraftArguments": "--username ${auth_player_name} --version ${version_name} --gameDir ${game_directory} --assetsDir ${assets_root} --assetIndex ${assets_index_name} --uuid ${auth_uuid} --accessToken ${auth_access_token} --userProperties ${user_properties} --userType ${user_type} --versionType ${version_type}"
        });
        let args = build(&bp, &full_inputs()).unwrap();
        assert_eq!(
            args.game,
            [
                "--username",
                "Alex",
                "--version",
                "1.0",
                "--gameDir",
                "game",
                "--assetsDir",
                "assets",
                "--assetIndex",
                "1.8",
                "--uuid",
                "uuid",
                "--accessToken",
                "token",
                "--userProperties",
                "{}",
                "--userType",
                "msa",
                "--versionType",
                "release"
            ]
        );
        assert!(args.jvm.contains(&"-cp".to_string()));
        assert!(args
            .jvm
            .contains(&"-Djava.library.path=natives".to_string()));
    }

    #[test]
    fn modern_arguments_respect_feature_rules() {
        let bp = json!({"arguments": {
            "game": [
                "--version", "${version_name}",
                {"rules": [{"action": "allow", "features": {"has_custom_resolution": true}}],
                 "value": ["--width", "${resolution_width}", "--height", "${resolution_height}"]},
                {"rules": [{"action": "allow", "features": {"is_quick_play_singleplayer": true}}],
                 "value": ["--quickPlaySingleplayer", "${quickPlaySingleplayer}"]}
            ],
            "jvm": ["-cp", "${classpath}"]
        }});
        let args = build(&bp, &full_inputs()).unwrap();
        assert_eq!(
            args.game,
            ["--version", "1.0", "--width", "854", "--height", "480"]
        );
        assert_eq!(args.jvm, ["-cp", "a.jar:b.jar"]);
    }

    #[test]
    fn unknown_placeholder_is_an_error() {
        let bp = json!({"minecraftArguments": "--foo ${not_a_real_thing}"});
        let err = build(&bp, &full_inputs()).err().unwrap().to_string();
        assert!(err.contains("${not_a_real_thing}"), "{err}");
    }

    fn check_blueprint(id: &str, bp: &Value, failures: &mut Vec<String>) {
        for quick_play in [
            None,
            Some(QuickPlay::Singleplayer("w".into())),
            Some(QuickPlay::Multiplayer("m".into())),
        ] {
            let inputs = LaunchInputs {
                quick_play,
                ..full_inputs()
            };
            match build(bp, &inputs) {
                Ok(args) => {
                    if !args.jvm.iter().any(|a| a == "-cp") {
                        failures.push(format!("{id}: no -cp in JVM arguments"));
                    }
                }
                Err(e) => failures.push(format!("{id}: {e}")),
            }
            if let Some(defaults) = bp["arguments"].get("default-user-jvm") {
                let mut missing = BTreeSet::new();
                collect(
                    defaults,
                    &inputs.placeholder_values(),
                    &inputs.features(),
                    &mut missing,
                );
                if let Err(e) = check_missing(&missing) {
                    failures.push(format!("{id} (default-user-jvm): {e}"));
                }
            }
        }
    }

    /// Validates the argument builder against Mojang's own metadata for every
    /// version ever released, plus Fabric/Quilt on the latest release.
    /// Run with: cargo test -- --ignored
    #[tokio::test]
    #[ignore = "downloads every official version JSON (~50 MB)"]
    async fn every_official_version_resolves_all_placeholders() {
        use crate::http::{build_http_client, get_json};
        use futures::stream::{self, StreamExt};

        let client = build_http_client().unwrap();
        let manifest = get_json(&client, crate::config::MANIFEST_URL, None)
            .await
            .unwrap();
        let entries: Vec<(String, String)> = manifest["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["id"].as_str().unwrap().to_string(),
                    v["url"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let fetched: Vec<(String, Value)> = stream::iter(entries)
            .map(|(id, url)| {
                let client = client.clone();
                async move { (id, get_json(&client, &url, None).await.unwrap()) }
            })
            .buffer_unordered(32)
            .collect()
            .await;

        let mut failures = Vec::new();
        for (id, bp) in &fetched {
            check_blueprint(id, bp, &mut failures);
        }

        let latest = manifest["latest"]["release"].as_str().unwrap();
        let vanilla = &fetched.iter().find(|(id, _)| id == latest).unwrap().1;
        for meta in [
            "https://meta.fabricmc.net/v2",
            "https://meta.quiltmc.org/v3",
        ] {
            let loaders = get_json(&client, &format!("{meta}/versions/loader/{latest}"), None)
                .await
                .unwrap();
            let Some(loader) = loaders[0]["loader"]["version"].as_str() else {
                continue;
            };
            let profile = get_json(
                &client,
                &format!("{meta}/versions/loader/{latest}/{loader}/profile/json"),
                None,
            )
            .await
            .unwrap();
            let mut merged = vanilla.clone();
            crate::version::merge_child(&mut merged, profile);
            check_blueprint(&format!("{meta} {loader}"), &merged, &mut failures);
        }

        println!("checked {} versions", fetched.len());
        assert!(
            failures.is_empty(),
            "{} failures:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn forge_module_path_expands() {
        let bp = json!({"arguments": {"game": [], "jvm": [
            "-p", "${library_directory}/a.jar${classpath_separator}${library_directory}/b.jar",
            "-DignoreList=forge-,${version_name}.jar"
        ]}});
        let args = build(&bp, &full_inputs()).unwrap();
        assert_eq!(
            args.jvm,
            [
                "-p".to_string(),
                format!("libraries/a.jar{CLASSPATH_SEP}libraries/b.jar"),
                "-DignoreList=forge-,1.0.jar".to_string()
            ]
        );
    }
}
