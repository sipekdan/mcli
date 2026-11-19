//! Evaluation of the `rules` arrays used by Mojang version metadata for both
//! libraries and launch arguments.
//!
//! Semantics follow the official launcher: with no rules an entry applies;
//! otherwise it starts disallowed and every *matching* rule sets the outcome
//! to its `action`, so the last matching rule wins.

use crate::config::{MOJANG_ARCH, MOJANG_OS};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Every feature flag found in official metadata. Rules that mention any other
/// feature never match, as in the official launcher.
pub const KNOWN_FEATURES: &[&str] = &[
    "is_demo_user",
    "has_custom_resolution",
    "has_quick_plays_support",
    "is_quick_play_singleplayer",
    "is_quick_play_multiplayer",
    "is_quick_play_realms",
];

#[derive(Default, Clone)]
pub struct Features {
    enabled: HashSet<&'static str>,
}

impl Features {
    pub fn enable(&mut self, name: &'static str) {
        debug_assert!(KNOWN_FEATURES.contains(&name));
        self.enabled.insert(name);
    }

    fn get(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }
}

pub fn rules_allow(rules: Option<&Value>, features: &Features) -> bool {
    let Some(rules) = rules.and_then(Value::as_array) else {
        return true;
    };
    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule, features) {
            allowed = rule["action"].as_str() == Some("allow");
        }
    }
    allowed
}

fn rule_matches(rule: &Value, features: &Features) -> bool {
    if let Some(os) = rule.get("os") {
        if let Some(name) = os["name"].as_str() {
            if name != MOJANG_OS {
                return false;
            }
        }
        if let Some(arch) = os["arch"].as_str() {
            if arch != MOJANG_ARCH {
                return false;
            }
        }
        if let Some(pattern) = os["version"].as_str() {
            match regex_lite::Regex::new(pattern) {
                Ok(re) if re.is_match(os_version()) => {}
                _ => return false,
            }
        }
        if let Some(range) = os.get("versionRange") {
            let current = os_version();
            if let Some(min) = range["min"].as_str() {
                if compare_versions(current, min).is_lt() {
                    return false;
                }
            }
            if let Some(max) = range["max"].as_str() {
                if compare_versions(current, max).is_ge() {
                    return false;
                }
            }
        }
    }
    if let Some(wanted) = rule.get("features").and_then(Value::as_object) {
        for (name, value) in wanted {
            if features.get(name) != value.as_bool().unwrap_or(false) {
                return false;
            }
        }
    }
    true
}

/// Compare dotted numeric versions (`10.0.17134` vs `10.0.22631`).
/// Non-numeric parts compare as zero.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split(['.', '-', '_', '+'])
            .map(|p| {
                p.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let ord = a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0));
        if ord.is_ne() {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// The OS version as Java's `os.version` would report it. Resolved lazily
/// because only a handful of rules need it, and only on the matching OS.
fn os_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| detect_os_version().unwrap_or_default())
}

fn detect_os_version() -> Option<String> {
    use std::process::Command;
    let run = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if cfg!(windows) {
        // "Microsoft Windows [Version 10.0.22631.4317]"
        let text = run("cmd", &["/C", "ver"])?;
        let start = text.find("Version ")? + "Version ".len();
        let rest = &text[start..];
        Some(rest.trim_end_matches(']').trim().to_string())
    } else if cfg!(target_os = "macos") {
        run("sw_vers", &["-productVersion"])
    } else {
        run("uname", &["-r"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_rules_means_allowed() {
        assert!(rules_allow(None, &Features::default()));
    }

    #[test]
    fn last_matching_rule_wins() {
        let rules = json!([
            {"action": "allow"},
            {"action": "disallow", "os": {"name": MOJANG_OS}}
        ]);
        assert!(!rules_allow(Some(&rules), &Features::default()));

        let other_os = json!([
            {"action": "allow"},
            {"action": "disallow", "os": {"name": "some-other-os"}}
        ]);
        assert!(rules_allow(Some(&other_os), &Features::default()));
    }

    #[test]
    fn disallow_without_os_applies_everywhere() {
        let rules = json!([{"action": "disallow"}]);
        assert!(!rules_allow(Some(&rules), &Features::default()));
    }

    #[test]
    fn features_must_match() {
        let rules = json!([{"action": "allow", "features": {"has_custom_resolution": true}}]);
        let mut f = Features::default();
        assert!(!rules_allow(Some(&rules), &f));
        f.enable("has_custom_resolution");
        assert!(rules_allow(Some(&rules), &f));

        let unknown = json!([{"action": "allow", "features": {"some_future_flag": true}}]);
        assert!(!rules_allow(Some(&unknown), &f));
    }

    #[test]
    fn compares_dotted_versions() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("10.0.22631", "10.0.17134"), Greater);
        assert_eq!(compare_versions("10.0", "10.0.0"), Equal);
        assert_eq!(compare_versions("6.12.117+deb13-amd64", "6.2"), Greater);
    }
}
