use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pdf::FitMode;

/// Settings shared by the viewer and its bundled Neovim plugin.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    fit_mode: FitModeSetting,
    #[serde(alias = "invert")]
    dark_mode: bool,
    theme: Option<String>,
    theme_catalog: Option<String>,
    persistent_link_picker: bool,
    link_picker_split_percent: Option<u16>,
    link_picker_layout: LinkPickerLayout,
    synctex_enabled: Option<bool>,
    pub editor: crate::editor::Editor,
    forward_socket: Option<String>,
    pub viewer: ViewerSettings,
    pub nvim: NvimSettings,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum FitModeSetting {
    #[default]
    Page,
    Width,
    Height,
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LinkPickerLayout {
    #[default]
    Auto,
    Vertical,
    Horizontal,
    Floating,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = config_path().ok_or("HOME or XDG_CONFIG_HOME must be set")?;
        Self::load_path(&path).map_err(|error| format!("{}: {error}", path.display()).into())
    }

    fn load_path(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        fs::create_dir_all(path.parent().ok_or("config path has no parent")?)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => file.write_all(include_bytes!("../config.default.toml"))?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let mut config: Self = toml::from_str(&fs::read_to_string(path)?)?;
        let viewer = &config.viewer;
        for (name, value, low, high) in [
            ("scroll_frame_ms", viewer.scroll_frame_ms, 1, 1000),
            ("scroll_ease_divisor", viewer.scroll_ease_divisor, 1, 100),
            ("scroll_step_percent", viewer.scroll_step_percent, 1, 100),
            ("page_scroll_percent", viewer.page_scroll_percent, 1, 100),
            ("flash_duration_ms", viewer.flash_duration_ms, 1, 60000),
            ("source_context_lines", viewer.source_context_lines, 0, 100),
            ("prefetch_pages", viewer.prefetch_pages, 0, u32::MAX as u64),
        ] {
            if !(low..=high).contains(&value) {
                return Err(format!("viewer.{name} must be in {low}..={high}").into());
            }
        }
        let runtime = fs::canonicalize(path.parent().unwrap())?.join("run");
        config.editor.validate()?;
        for value in [config.editor.socket_mut(), config.forward_socket.as_mut()]
            .into_iter()
            .flatten()
        {
            if value.is_empty() {
                continue;
            }
            let configured = Path::new(value);
            let resolved = if configured.is_absolute() {
                configured.to_owned()
            } else {
                if configured.components().count() != 1
                    || !matches!(
                        configured.components().next(),
                        Some(std::path::Component::Normal(_))
                    )
                {
                    return Err("relative socket paths must be simple filenames".into());
                }
                runtime.join(configured)
            };
            // macOS sockaddr_un.sun_path has 104 bytes, including the NUL.
            if resolved.as_os_str().as_encoded_bytes().len() > 103 {
                return Err(
                    format!("socket path exceeds 103 bytes: {}", resolved.display()).into(),
                );
            }
            crate::ipc::private_dir(resolved.parent().ok_or("socket path has no parent")?)?;
            *value = resolved
                .into_os_string()
                .into_string()
                .map_err(|_| "socket path must be UTF-8")?;
        }
        if let crate::editor::Editor::Socket { path } = &config.editor
            && config.forward_socket.as_ref() == Some(path)
        {
            return Err("editor and forward sockets must have different paths".into());
        }
        Ok(config)
    }

    /// Select named endpoints, or disable viewer control without changing the editor.
    pub fn select_session(&mut self, name: Option<&str>) -> io::Result<()> {
        let Some(name) = name else {
            if let Some(socket) = self.forward_socket.as_mut() {
                socket.clear();
            }
            return Ok(());
        };
        if name.is_empty()
            || name.len() > 24
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(io::Error::other(
                "session must contain 1..24 ASCII letters, digits, '-' or '_'",
            ));
        }
        let mut endpoints = Vec::new();
        for value in [self.editor.socket_mut(), self.forward_socket.as_mut()]
            .into_iter()
            .flatten()
        {
            if value.is_empty() {
                continue;
            }
            let path = Path::new(value);
            let file = path
                .file_name()
                .ok_or_else(|| io::Error::other("socket path has no filename"))?;
            let selected = path.with_file_name(format!("{name}-{}", file.to_string_lossy()));
            if selected.as_os_str().as_encoded_bytes().len() > 103 {
                return Err(io::Error::other(
                    "named session socket path exceeds 103 bytes",
                ));
            }
            endpoints.push((value, selected));
        }
        for (value, selected) in endpoints {
            *value = selected
                .into_os_string()
                .into_string()
                .map_err(|_| io::Error::other("socket path is not UTF-8"))?;
        }
        Ok(())
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
        self.theme.as_deref().filter(|value| !value.is_empty())
    }

    pub fn theme_catalog(&self) -> Option<&str> {
        self.theme_catalog
            .as_deref()
            .filter(|value| !value.is_empty())
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

    pub fn synctex_enabled(&self) -> bool {
        self.synctex_enabled.unwrap_or(true)
    }

    pub fn forward_socket(&self) -> Option<&str> {
        self.forward_socket
            .as_deref()
            .filter(|value| !value.is_empty())
    }
}

pub fn config_path() -> Option<PathBuf> {
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

impl Default for Config {
    fn default() -> Self {
        Self {
            fit_mode: FitModeSetting::default(),
            dark_mode: false,
            theme: None,
            theme_catalog: None,
            persistent_link_picker: false,
            link_picker_split_percent: None,
            link_picker_layout: LinkPickerLayout::Auto,
            synctex_enabled: None,
            editor: crate::editor::Editor::Socket {
                path: "editor.sock".into(),
            },
            forward_socket: Some("forward.sock".into()),
            viewer: ViewerSettings::default(),
            nvim: NvimSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ViewerSettings {
    pub continuous_scroll: bool,
    pub prefetch_pages: u64,
    pub smooth_scroll: bool,
    pub scroll_frame_ms: u64,
    pub scroll_ease_divisor: u64,
    pub scroll_step_percent: u64,
    pub page_scroll_percent: u64,
    pub set_window_title: bool,
    pub center_forward_search: bool,
    pub flash_duration_ms: u64,
    pub flash_label_font: String,
    #[serde(with = "rgb_hex")]
    pub flash_label_foreground: [u8; 3],
    #[serde(with = "rgb_hex")]
    pub flash_label_background: [u8; 3],
    pub word_precision: bool,
    pub source_context_lines: u64,
}

impl Default for ViewerSettings {
    fn default() -> Self {
        Self {
            continuous_scroll: true,
            prefetch_pages: 5,
            smooth_scroll: false,
            scroll_frame_ms: 16,
            scroll_ease_divisor: 4,
            scroll_step_percent: 12,
            page_scroll_percent: 85,
            set_window_title: true,
            center_forward_search: true,
            flash_label_font: "monospace".into(),
            flash_label_foreground: [0xc0, 0xca, 0xf5],
            flash_label_background: [0x5b, 0x21, 0xb6],
            flash_duration_ms: 1000,
            word_precision: true,
            source_context_lines: 4,
        }
    }
}

mod rgb_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<[u8; 3], D::Error> {
        let value = String::deserialize(deserializer)?;
        crate::theme::parse_rgb("label color", &value).map_err(serde::de::Error::custom)
    }

    pub(super) fn serialize<S: Serializer>(
        rgb: &[u8; 3],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]))
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NvimSettings {
    pub compile: bool,
    pub focus_on_inverse: bool,
    pub focus_on_forward: bool,
    pub executable: String,
    pub attach_only: bool,
    pub keys: NvimKeys,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NvimKeys {
    pub forward: String,
    pub build: String,
    pub main_file: String,
    pub compile: String,
}

#[cfg(test)]
mod tests {
    use super::{Config, LinkPickerLayout};
    use crate::pdf::FitMode;
    use std::fs;

    #[test]
    fn empty_config_uses_defaults() {
        let config: Config = toml::from_str("").expect("empty config");
        assert_eq!(config.fit_mode(), FitMode::Page);
        assert!(!config.dark_mode());
        assert_eq!(config.theme(), None);
        assert_eq!(config.theme_catalog(), None);
        assert!(!config.persistent_link_picker());
        assert_eq!(config.link_picker_split_percent(), 50);
        assert!(!config.nvim.focus_on_forward);
        assert!(!config.nvim.focus_on_inverse);
        assert_eq!(config.link_picker_layout(), LinkPickerLayout::Auto);
    }

    #[test]
    fn parses_nvim_focus_overrides() {
        let config: Config =
            toml::from_str("[nvim]\nfocus_on_forward = true\nfocus_on_inverse = true\n")
                .expect("config");
        assert!(config.nvim.focus_on_forward);
        assert!(config.nvim.focus_on_inverse);
    }

    #[test]
    fn flash_label_font_is_configurable() {
        let default: Config = toml::from_str("").expect("default config");
        assert_eq!(default.viewer.flash_label_font, "monospace");
        let configured: Config =
            toml::from_str("[viewer]\nflash_label_font = \"Menlo\"\n").expect("config");
        assert_eq!(configured.viewer.flash_label_font, "Menlo");
    }

    #[test]
    fn label_colors_accept_hex_and_round_trip_as_strings() {
        let config: Config = toml::from_str(
            "[viewer]\nflash_label_foreground = '#Ab12EF'\nflash_label_background = '#102030'\n",
        )
        .unwrap();
        assert_eq!(config.viewer.flash_label_foreground, [0xab, 0x12, 0xef]);
        assert_eq!(config.viewer.flash_label_background, [0x10, 0x20, 0x30]);
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["viewer"]["flash_label_foreground"], "#ab12ef");
        assert_eq!(json["viewer"]["flash_label_background"], "#102030");
        let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.viewer.flash_label_foreground, [0xab, 0x12, 0xef]);
        assert_eq!(restored.viewer.flash_label_background, [0x10, 0x20, 0x30]);
    }

    #[test]
    fn label_colors_reject_invalid_values_without_panicking() {
        for field in ["flash_label_foreground", "flash_label_background"] {
            for value in [
                "'123456'",
                "'#123'",
                "'#gg1122'",
                "'#a€bc'",
                "'#+f0000'",
                "42",
                "[1,2,3]",
            ] {
                let error = toml::from_str::<Config>(&format!("[viewer]\n{field} = {value}\n"))
                    .expect_err("invalid label color");
                assert!(error.to_string().contains(field), "{error}");
            }
        }
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
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("future_option = 42").is_err());
    }

    #[test]
    fn creates_documented_config_without_overwriting_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pdfterm/config.toml");
        Config::load_path(&path).unwrap();
        let template = fs::read_to_string(&path).unwrap();
        assert_eq!(template, include_str!("../config.default.toml"));
        assert!(!Config::load_path(&path).unwrap().viewer.smooth_scroll);
        fs::write(&path, "[viewer]\nsmooth_scroll = true\n").unwrap();
        assert!(Config::load_path(&path).unwrap().viewer.smooth_scroll);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[viewer]\nsmooth_scroll = true\n"
        );
        fs::write(&path, "[viewer]\nscroll_frame_ms = 0\n").unwrap();
        assert!(Config::load_path(&path).is_err());
    }

    #[test]
    fn accepts_prefetch_counts_within_render_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for pages in [0, 1, 2, 8, 50, u32::MAX as u64] {
            fs::write(&path, format!("[viewer]\nprefetch_pages = {pages}\n")).unwrap();
            assert_eq!(
                Config::load_path(&path).unwrap().viewer.prefetch_pages,
                pages
            );
        }
        for pages in ["-1", "18446744073709551615", "notanumber"] {
            fs::write(&path, format!("[viewer]\nprefetch_pages = {pages}\n")).unwrap();
            assert!(Config::load_path(&path).is_err());
        }
    }

    #[test]
    fn unnamed_session_disables_control_but_preserves_inverse_search() {
        let mut config = Config::default();
        let editor = config.editor.socket_mut().unwrap().clone();
        config.select_session(None).unwrap();
        assert_eq!(config.forward_socket(), None);
        assert_eq!(config.editor.socket_mut().unwrap(), &editor);
    }

    #[test]
    fn named_session_preserves_explicitly_disabled_control() {
        let mut config = Config {
            forward_socket: Some(String::new()),
            ..Config::default()
        };
        config.select_session(Some("paper")).unwrap();
        assert_eq!(config.forward_socket(), None);
        assert_eq!(config.editor.socket_mut().unwrap(), "paper-editor.sock");
    }

    #[test]
    fn session_names_validate_before_changing_either_endpoint() {
        let mut config = Config {
            editor: crate::editor::Editor::Socket {
                path: "/private/editor.sock".into(),
            },
            forward_socket: Some(format!("/private/{}/forward.sock", "x".repeat(80))),
            ..Config::default()
        };
        assert!(config.select_session(Some("paper")).is_err());
        assert_eq!(config.editor.socket_mut().unwrap(), "/private/editor.sock");
        for invalid in [
            "",
            "../other",
            "with space",
            "é",
            "abcdefghijklmnopqrstuvwxy",
        ] {
            assert!(config.select_session(Some(invalid)).is_err());
        }
    }
}
