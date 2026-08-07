use std::env;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::pdf::FitMode;

/// User settings loaded from `$XDG_CONFIG_HOME/pdfterm/config.toml`
/// (falling back to `~/.config/pdfterm/config.toml`). Every field is optional;
/// a missing or unreadable file yields defaults, and a malformed file is
/// reported once and then ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    fit_mode: FitModeSetting,
    invert: bool,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FitModeSetting {
    #[default]
    Page,
    Width,
    Height,
}

impl Config {
    /// Loads the configuration, returning defaults when no usable file exists.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return Self::default(),
        };
        match toml::from_str(&text) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("pdfterm: ignoring {}: {error}", path.display());
                Self::default()
            }
        }
    }

    pub fn fit_mode(&self) -> FitMode {
        match self.fit_mode {
            FitModeSetting::Page => FitMode::Page,
            FitModeSetting::Width => FitMode::Width,
            FitModeSetting::Height => FitMode::Height,
        }
    }

    pub fn invert(&self) -> bool {
        self.invert
    }
}

fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("pdfterm").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::pdf::FitMode;

    #[test]
    fn empty_config_uses_defaults() {
        let config: Config = toml::from_str("").expect("empty config");
        assert_eq!(config.fit_mode(), FitMode::Page);
        assert!(!config.invert());
    }

    #[test]
    fn parses_fit_mode_and_invert() {
        let config: Config =
            toml::from_str("fit_mode = \"width\"\ninvert = true\n").expect("config");
        assert_eq!(config.fit_mode(), FitMode::Width);
        assert!(config.invert());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: Config =
            toml::from_str("future_option = 42\ninvert = true\n").expect("config");
        assert!(config.invert());
    }
}
