//! The `config` command: the effective configuration.

use std::path::PathBuf;
use std::process::ExitCode;

use herdr_sleep::config::{self, CONFIG_FILE_NAME, Config};
use serde_json::Value;

use super::{emit, fail};

/// The path of the config file, if a directory can be worked out.
fn config_path() -> Option<PathBuf> {
    config::config_dir(config::process_env).map(|dir| dir.join(CONFIG_FILE_NAME))
}

/// The configuration as TOML, or as JSON when `json` is set.
pub fn render(config: &Config, json: bool) -> Result<String, String> {
    if json {
        return serde_json::to_string_pretty(config).map_err(|error| error.to_string());
    }
    match serde_json::to_value(config).map_err(|error| error.to_string())? {
        Value::Object(table) => {
            let mut out = String::new();
            write_table(&mut out, &[], &table);
            Ok(out.trim_end().to_string())
        }
        _ => Err("the configuration is not a table".into()),
    }
}

fn key(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        // A JSON string is a valid TOML basic string.
        Value::String(name.to_string()).to_string()
    }
}

/// A value on one line. `null` has no TOML spelling and is skipped by the
/// caller, so inside an array it is dropped.
fn inline(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let items: Vec<_> = items
                .iter()
                .filter(|item| !item.is_null())
                .map(inline)
                .collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => {
            let pairs: Vec<_> = map
                .iter()
                .filter(|(_, item)| !item.is_null())
                .map(|(name, item)| format!("{} = {}", key(name), inline(item)))
                .collect();
            format!("{{ {} }}", pairs.join(", "))
        }
        scalar => scalar.to_string(),
    }
}

/// Write `table` under the header `path`: plain values first, then the
/// sub-tables, which is the order TOML requires.
fn write_table(out: &mut String, path: &[String], table: &serde_json::Map<String, Value>) {
    if !path.is_empty() {
        out.push_str(&format!("[{}]\n", path.join(".")));
    }
    for (name, value) in table {
        if !value.is_object() && !value.is_null() {
            out.push_str(&format!("{} = {}\n", key(name), inline(value)));
        }
    }
    out.push('\n');
    for (name, value) in table {
        if let Value::Object(sub) = value {
            let mut sub_path = path.to_vec();
            sub_path.push(key(name));
            write_table(out, &sub_path, sub);
        }
    }
}

/// Run `herdr-sleep config [--json]`.
pub fn run(json: bool) -> ExitCode {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => return fail(error),
    };
    match config_path() {
        Some(path) if path.is_file() => eprintln!("herdr-sleep: config file: {}", path.display()),
        Some(path) => eprintln!(
            "herdr-sleep: config file: {} (not found, defaults in use)",
            path.display()
        ),
        None => eprintln!("herdr-sleep: config file: none (no config directory, defaults in use)"),
    }
    match config::state_dir(config::process_env) {
        Some(dir) => eprintln!("herdr-sleep: state directory: {}", dir.display()),
        None => eprintln!("herdr-sleep: state directory: none"),
    }
    for warning in &config.warnings {
        eprintln!("herdr-sleep: warning: {warning}");
    }
    match render(&config, json) {
        Ok(text) => {
            emit(&text);
            ExitCode::SUCCESS
        }
        Err(error) => fail(format!("cannot print the configuration: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_output_reads_back_with_the_effective_values() {
        let config = Config::from_toml_str("sleep_after_minutes = 7\nsleep_icon = \"zz\"").unwrap();
        let text = render(&config, false).unwrap();
        let value: toml::Value = text.parse().unwrap();
        assert_eq!(value["sleep_after_minutes"].as_integer(), Some(7));
        assert_eq!(value["sleep_icon"].as_str(), Some("zz"));
        assert!(value["profiles"]["claude"]["bin"].is_str());
    }

    #[test]
    fn json_output_has_the_configuration_keys() {
        let text = render(&Config::default(), true).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        for key in [
            "sleep_after_minutes",
            "sleep_statuses",
            "dry_run",
            "sleep_icon",
            "sweep_interval_seconds",
            "profiles",
            "warnings",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert_eq!(value["profiles"]["claude"]["kind"], "claude");
    }
}
