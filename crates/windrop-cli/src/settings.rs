//! Reading and writing individual settings.
//!
//! `windrop config get` and `set` work on dotted paths into the same JSON shape
//! the configuration file uses — `performance_mode`, `dxvk_settings.hud`,
//! `shared_folders` — rather than on a hand-written list of accessors. That way
//! a field added to [`Config`] is reachable immediately, and the two cannot
//! drift apart.
//!
//! Setting a value round-trips through `serde`: the JSON is edited, then
//! converted back into a [`Config`]. A type that does not fit the field is
//! therefore a clean error rather than a mangled configuration file, and the
//! same validation the file goes through applies here too.

use serde_json::Value;
use windrop_core::config::Config;
use windrop_core::{Error, Result};

/// One settable key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    /// The dotted path used on the command line.
    pub path: &'static str,
    pub description: &'static str,
}

/// Everything `windrop config keys` prints.
pub const KEYS: &[Setting] = &[
    Setting {
        path: "wine_variant",
        description: "stable | staging | system | a named build",
    },
    Setting {
        path: "dxvk",
        description: "translate Direct3D 9/10/11 to Vulkan",
    },
    Setting {
        path: "vkd3d_proton",
        description: "translate Direct3D 12 to Vulkan",
    },
    Setting {
        path: "esync",
        description: "eventfd-based synchronisation",
    },
    Setting {
        path: "fsync",
        description: "futex-based synchronisation (needs a patched kernel)",
    },
    Setting {
        path: "sandbox",
        description: "strict (bubblewrap) | off",
    },
    Setting {
        path: "shared_folders",
        description: "host directories visible inside the sandbox",
    },
    Setting {
        path: "allow_remote_registry",
        description: "allow profile lookups over the network",
    },
    Setting {
        path: "auto_profile_updates",
        description: "check for newer profiles in the background",
    },
    Setting {
        path: "registry_url",
        description: "the profile registry endpoint",
    },
    Setting {
        path: "telemetry",
        description: "anonymous diagnostics (off by default)",
    },
    Setting {
        path: "data_dir",
        description: "where applications and runtimes live",
    },
    Setting {
        path: "install_timeout_secs",
        description: "how long an installer may run",
    },
    Setting {
        path: "log_level",
        description: "trace | debug | info | warn | error",
    },
    Setting {
        path: "prompt_for_main_exe",
        description: "always ask which program to launch",
    },
    Setting {
        path: "install_silently",
        description: "drive installers with their silent flags",
    },
    Setting {
        path: "performance_mode",
        description: "balanced | performance | compatibility",
    },
    Setting {
        path: "dxvk_settings.hud",
        description: "DXVK HUD contents, e.g. fps,devinfo",
    },
    Setting {
        path: "dxvk_settings.max_frame_rate",
        description: "frame-rate cap (0 = uncapped)",
    },
    Setting {
        path: "dxvk_settings.max_frame_latency",
        description: "frames queued (0 = app decides)",
    },
    Setting {
        path: "dxvk_settings.compiler_threads",
        description: "shader compiler threads (0 = auto)",
    },
    Setting {
        path: "dxvk_settings.sync_interval",
        description: "vsync (-1 = app decides)",
    },
    Setting {
        path: "dxvk_settings.graphics_pipeline_library",
        description: "fast first-run shader compilation",
    },
];

/// Read a setting.
pub fn get(config: &Config, key: &str) -> Result<Value> {
    let value = serde_json::to_value(config)?;
    lookup(&value, key).cloned()
}

/// Change a setting, returning its new value.
pub fn set(config: &mut Config, key: &str, raw: &str) -> Result<Value> {
    let mut value = serde_json::to_value(&*config)?;
    let parsed = parse_value(raw);
    assign(&mut value, key, parsed)?;

    // Converting back is what enforces the field's type: `config set dxvk  maybe`
    // is refused here rather than producing a configuration file that fails to
    // load on the next run.
    let updated: Config = serde_json::from_value(value).map_err(|e| Error::Config {
        field: key.to_string(),
        reason: e.to_string(),
    })?;
    updated.validate()?;
    *config = updated;
    get(config, key)
}

/// How a value is shown to the user.
///
/// A string is printed bare, because a user setting `staging` expects to read
/// back `staging` rather than `"staging"`. Everything else is printed as JSON.
pub fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// A command-line value as JSON: JSON when it parses as JSON, text otherwise.
///
/// This makes the common case pleasant (`true`, `3`, `staging`, `/tmp/x`) while
/// still allowing a list (`["/home/me/Shared"]`) or an empty string.
fn parse_value(raw: &str) -> Value {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) => value,
        Err(_) => Value::String(raw.to_string()),
    }
}

fn lookup<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    let mut current = value;
    for segment in key.split('.') {
        current = current.get(segment).ok_or_else(|| unknown_key(key))?;
    }
    Ok(current)
}

fn assign(value: &mut Value, key: &str, new_value: Value) -> Result<()> {
    let (parent_path, leaf) = match key.rsplit_once('.') {
        Some((parent, leaf)) => (Some(parent), leaf),
        None => (None, key),
    };

    let target = match parent_path {
        Some(path) => {
            let mut current = value;
            for segment in path.split('.') {
                current = current.get_mut(segment).ok_or_else(|| unknown_key(key))?;
            }
            current.as_object_mut().ok_or_else(|| unknown_key(key))?
        }
        None => value.as_object_mut().ok_or_else(|| unknown_key(key))?,
    };

    // Refuse to invent a key: silently accepting one would look like it worked
    // and change nothing.
    let slot = target.get_mut(leaf).ok_or_else(|| unknown_key(key))?;
    *slot = new_value;
    Ok(())
}

fn unknown_key(key: &str) -> Error {
    let known = KEYS.iter().map(|k| k.path).collect::<Vec<_>>().join(", ");
    Error::Config {
        field: "key".into(),
        reason: format!("there is no setting called '{key}'. Known settings: {known}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windrop_core::config::{PerformanceMode, SandboxMode};

    #[test]
    fn every_advertised_key_can_be_read() {
        // A key that is listed but missing from `Config` would be a broken
        // promise, and this is the only place that can catch it.
        let config = Config::default();
        for setting in KEYS {
            get(&config, setting.path)
                .unwrap_or_else(|e| panic!("{} is advertised but unreadable: {e}", setting.path));
        }
    }

    #[test]
    fn every_advertised_key_exists_in_the_serialised_config() {
        let value = serde_json::to_value(Config::default()).unwrap();
        for setting in KEYS {
            assert!(
                lookup(&value, setting.path).is_ok(),
                "{} is advertised but is not a field of Config",
                setting.path
            );
        }
    }

    #[test]
    fn booleans_are_read_and_written() {
        let mut config = Config::default();
        assert_eq!(get(&config, "dxvk").unwrap(), Value::Bool(true));

        set(&mut config, "dxvk", "false").unwrap();
        assert!(!config.dxvk);
        assert_eq!(get(&config, "dxvk").unwrap(), Value::Bool(false));

        // A bare word that is not JSON is text, not a boolean, so it is refused
        // rather than silently coerced.
        assert!(set(&mut config, "dxvk", "maybe").is_err());
        assert!(!config.dxvk, "a refused write must not change anything");
    }

    #[test]
    fn numbers_are_read_and_written() {
        let mut config = Config::default();
        set(&mut config, "install_timeout_secs", "900").unwrap();
        assert_eq!(config.install_timeout_secs, 900);

        // Out-of-range values are still the domain of validation.
        assert!(set(&mut config, "install_timeout_secs", "0").is_err());
        assert_eq!(config.install_timeout_secs, 900);
        assert!(set(&mut config, "install_timeout_secs", "-5").is_err());
    }

    #[test]
    fn nested_dxvk_settings_are_reachable() {
        let mut config = Config::default();
        set(&mut config, "dxvk_settings.max_frame_rate", "144").unwrap();
        assert_eq!(config.dxvk_settings.max_frame_rate, 144);

        // The nested path is also readable, and it is the *stored* value rather
        // than the effective one, so the user can see what they chose.
        assert_eq!(
            get(&config, "dxvk_settings.hud").unwrap(),
            Value::String(String::new())
        );
        assert!(set(&mut config, "dxvk_settings.hud", "fps,devinfo").is_ok());
        assert_eq!(config.dxvk_settings.hud, "fps,devinfo");
    }

    #[test]
    fn enums_are_written_by_their_wire_name() {
        let mut config = Config::default();
        set(&mut config, "sandbox", "off").unwrap();
        assert_eq!(config.sandbox, SandboxMode::Off);

        set(&mut config, "performance_mode", "compatibility").unwrap();
        assert_eq!(config.performance_mode, PerformanceMode::Compatibility);

        set(&mut config, "wine_variant", "staging").unwrap();
        assert_eq!(
            config.wine_variant,
            windrop_core::config::WineVariant::Staging
        );

        assert!(set(&mut config, "wine_variant", "nonesuch").is_err());
        assert!(set(&mut config, "sandbox", "sometimes").is_err());
    }

    #[test]
    fn a_known_wrong_type_is_refused_with_a_useful_message() {
        let mut config = Config::default();
        match set(&mut config, "log_level", "3") {
            Err(Error::Config { field, .. }) => assert_eq!(field, "log_level"),
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_key_lists_the_real_ones() {
        let mut config = Config::default();
        match set(&mut config, "dxxvk", "true") {
            Err(Error::Config { reason, .. }) => {
                assert!(reason.contains("no setting called 'dxxvk'"), "{reason}");
                assert!(
                    reason.contains("performance_mode"),
                    "the message should list the keys"
                );
            }
            other => panic!("expected a config error, got {other:?}"),
        }
        assert!(get(&config, "nope.nope").is_err());
    }

    #[test]
    fn lists_are_written_as_json() {
        let mut config = Config::default();
        set(&mut config, "shared_folders", r#"["/home/me/Documents"]"#).unwrap();
        assert_eq!(
            config.shared_folders,
            vec![std::path::PathBuf::from("/home/me/Documents")]
        );
        // A bare string where a list belongs is refused, not silently accepted.
        assert!(set(&mut config, "shared_folders", "/home/me/Documents").is_err());
    }

    #[test]
    fn strings_are_rendered_without_quotes_and_everything_else_as_json() {
        assert_eq!(render(&Value::String("staging".into())), "staging");
        assert_eq!(render(&Value::Bool(true)), "true");
        assert_eq!(render(&Value::from(144)), "144");
        assert_eq!(
            render(&serde_json::json!(["/tmp/shared"])),
            r#"["/tmp/shared"]"#
        );
    }

    #[test]
    fn text_that_is_not_json_is_taken_literally() {
        // A registry URL must not have to be quoted, and a Windows-looking path
        // must survive untouched.
        let mut config = Config::default();
        set(&mut config, "registry_url", "https://example.com/p.json").unwrap();
        assert_eq!(config.registry_url, "https://example.com/p.json");

        assert_eq!(parse_value("staging"), Value::String("staging".into()));
        assert_eq!(parse_value("null"), Value::Null);
        assert_eq!(parse_value("true"), Value::Bool(true));
    }

    #[test]
    fn setting_a_value_reports_the_new_value() {
        let mut config = Config::default();
        let reported = set(&mut config, "log_level", "debug").unwrap();
        assert_eq!(render(&reported), "debug");
    }
}
