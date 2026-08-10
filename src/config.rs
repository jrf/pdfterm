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
    #[serde(alias = "invert")]
    dark_mode: bool,
    theme: Option<String>,
    theme_catalog: Option<String>,
    persistent_link_picker: bool,
    link_picker_split_percent: Option<u16>,
    link_picker_layout: LinkPickerLayout,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FitModeSetting {
    #[default]
    Page,
    Width,
    Height,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LinkPickerLayout {
    #[default]
    Auto,
    Vertical,
    Horizontal,
    Floating,
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

    pub fn dark_mode(&self) -> bool {
        self.dark_mode
    }

    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }

    pub fn theme_catalog(&self) -> Option<&str> {
        self.theme_catalog.as_deref()
    }

    pub fn persistent_link_picker(&self) -> bool {
        self.persistent_link_picker
    }

    pub fn link_picker_split_percent(&self) -> u16 {
        self.link_picker_split_percent.unwrap_or(50).clamp(20, 80)
    }

    pub fn link_picker_layout(&self) -> LinkPickerLayout {
        self.link_picker_layout
    }
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

pub(crate) fn config_root() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base)
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    Some(config_root()?.join("pdfterm"))
}

#[cfg(test)]
mod tests {
    use super::{Config, LinkPickerLayout};
    use crate::pdf::FitMode;

    #[test]
    fn empty_config_uses_defaults() {
        let config: Config = toml::from_str("").expect("empty config");
        assert_eq!(config.fit_mode(), FitMode::Page);
        assert!(!config.dark_mode());
        assert_eq!(config.theme(), None);
        assert_eq!(config.theme_catalog(), None);
        assert!(!config.persistent_link_picker());
        assert_eq!(config.link_picker_split_percent(), 50);
        assert_eq!(config.link_picker_layout(), LinkPickerLayout::Auto);
    }

    #[test]
    fn parses_fit_mode_and_dark_mode() {
        let config: Config =
            toml::from_str("fit_mode = \"width\"\ndark_mode = true\n").expect("config");
        assert_eq!(config.fit_mode(), FitMode::Width);
        assert!(config.dark_mode());
    }

    #[test]
    fn accepts_legacy_invert_name() {
        let config: Config = toml::from_str("invert = true\n").expect("config");
        assert!(config.dark_mode());
    }

    #[test]
    fn parses_persistent_link_picker() {
        let config: Config = toml::from_str("persistent_link_picker = true\n").expect("config");
        assert!(config.persistent_link_picker());
    }

    #[test]
    fn parses_and_bounds_link_picker_split_percent() {
        let config: Config = toml::from_str("link_picker_split_percent = 65\n").expect("config");
        assert_eq!(config.link_picker_split_percent(), 65);

        let config: Config = toml::from_str("link_picker_split_percent = 100\n").expect("config");
        assert_eq!(config.link_picker_split_percent(), 80);
    }

    #[test]
    fn parses_link_picker_layouts() {
        for (value, expected) in [
            ("auto", LinkPickerLayout::Auto),
            ("vertical", LinkPickerLayout::Vertical),
            ("horizontal", LinkPickerLayout::Horizontal),
            ("floating", LinkPickerLayout::Floating),
        ] {
            let config: Config =
                toml::from_str(&format!("link_picker_layout = \"{value}\"\n")).expect("config");
            assert_eq!(config.link_picker_layout(), expected);
        }
    }

    #[test]
    fn parses_explicit_theme_paths() {
        let config: Config = toml::from_str(
            "theme = \"~/.config/themes/synthetic.toml\"\ntheme_catalog = \"~/.config/themes/catalog.toml\"\n",
        )
        .expect("config");
        assert_eq!(config.theme(), Some("~/.config/themes/synthetic.toml"));
        assert_eq!(
            config.theme_catalog(),
            Some("~/.config/themes/catalog.toml")
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let config: Config =
            toml::from_str("future_option = 42\ndark_mode = true\n").expect("config");
        assert!(config.dark_mode());
    }
}
