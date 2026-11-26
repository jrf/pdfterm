use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crossterm::style::Color;
use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentPalette {
    pub background: [u8; 3],
    pub foreground: [u8; 3],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Palette {
    pub bg: Color,
    pub bg_dark: Color,
    pub bg_dark1: Color,
    pub bg_highlight: Color,
    pub blue: Color,
    pub blue1: Color,
    pub blue7: Color,
    pub comment: Color,
    pub cyan: Color,
    pub dark3: Color,
    pub fg: Color,
    pub fg_dark: Color,
    pub green: Color,
    pub magenta: Color,
    pub yellow: Color,
    pub document: DocumentPalette,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}

pub const TOKYO_NIGHT_MOON: Palette = Palette {
    bg: rgb(0x22, 0x24, 0x36),
    bg_dark: rgb(0x1e, 0x20, 0x30),
    bg_dark1: rgb(0x19, 0x1b, 0x29),
    bg_highlight: rgb(0x2f, 0x33, 0x4d),
    blue: rgb(0x82, 0xaa, 0xff),
    blue1: rgb(0x65, 0xbc, 0xff),
    blue7: rgb(0x39, 0x4b, 0x70),
    comment: rgb(0x63, 0x6d, 0xa6),
    cyan: rgb(0x86, 0xe1, 0xfc),
    dark3: rgb(0x54, 0x5c, 0x7e),
    fg: rgb(0xc8, 0xd3, 0xf5),
    fg_dark: rgb(0x82, 0x8b, 0xb8),
    green: rgb(0xc3, 0xe8, 0x8d),
    magenta: rgb(0xc0, 0x99, 0xff),
    yellow: rgb(0xff, 0xc7, 0x77),
    document: DocumentPalette {
        background: [0x1e, 0x20, 0x30],
        foreground: [0xc8, 0xd3, 0xf5],
    },
};

#[derive(Debug, Deserialize)]
struct PaletteFile {
    bg: String,
    bg_dark: String,
    bg_dark1: String,
    bg_highlight: String,
    blue: String,
    blue1: String,
    blue7: String,
    comment: String,
    cyan: String,
    dark3: String,
    fg: String,
    fg_dark: String,
    green: String,
    magenta: String,
    yellow: String,
    document: Option<DocumentPaletteFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentPaletteFile {
    background: String,
    foreground: String,
}

#[derive(Debug, Deserialize)]
struct SharedPaletteFile {
    colors: BTreeMap<String, String>,
    #[serde(default)]
    ui: BTreeMap<String, String>,
    document: Option<DocumentPaletteFile>,
}

#[derive(Debug, Error)]
pub enum ThemeError {
    #[error("could not read theme {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse theme {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("theme color {field} must be in #RRGGBB format, got {value:?}")]
    InvalidColor { field: String, value: String },
}

pub(crate) struct ResolvedThemes {
    pub active: Palette,
    pub themes: Vec<(String, Palette)>,
    pub index: usize,
    pub warning: Option<String>,
}

pub(crate) fn resolve(catalog_path: Option<&str>, selected_path: Option<&str>) -> ResolvedThemes {
    resolve_with_home(
        catalog_path,
        selected_path,
        crate::config::home_dir().as_deref(),
    )
}

pub fn load(configured_path: &str) -> Result<Palette, ThemeError> {
    let path = expand_home(crate::config::home_dir().as_deref(), configured_path);
    load_theme_file(&path)
}

fn load_theme_file(path: &Path) -> Result<Palette, ThemeError> {
    let text = fs::read_to_string(path).map_err(|source| ThemeError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse_theme(&text).map_err(|error| match error {
        ThemeError::Parse { source, .. } => ThemeError::Parse {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

fn resolve_with_home(
    catalog_path: Option<&str>,
    selected_path: Option<&str>,
    home: Option<&Path>,
) -> ResolvedThemes {
    let mut themes = Vec::new();
    let mut catalog_loaded = Vec::new();
    if let Some(catalog_path) = catalog_path {
        for path in load_catalog_paths(&expand_home(home, catalog_path)) {
            let expanded = expand_home(home, &path);
            if let Ok(theme) = load_theme_file(&expanded) {
                insert_theme_entry(&mut themes, theme_name(&expanded), theme);
                catalog_loaded.push((expanded, theme));
            }
        }
    }
    let mut active = TOKYO_NIGHT_MOON;
    let mut warning = None;
    let mut selected_name = None;
    if let Some(selected_path) = selected_path {
        let expanded = expand_home(home, selected_path);
        let loaded = catalog_loaded
            .iter()
            .find_map(|(path, theme)| (path == &expanded).then_some(*theme))
            .map_or_else(|| load_theme_file(&expanded), Ok);
        match loaded {
            Ok(theme) => {
                let name = theme_name(&expanded);
                insert_theme_entry(&mut themes, name.clone(), theme);
                active = theme;
                selected_name = Some(name);
            }
            Err(error) => {
                warning = Some(format!("{error}; using the internal fallback palette"));
            }
        }
    }
    if selected_name.is_none() && !themes.is_empty() {
        insert_theme_entry(&mut themes, "tokyo night moon".into(), TOKYO_NIGHT_MOON);
    }
    themes.sort_by(|left, right| left.0.cmp(&right.0));
    let index = selected_name
        .and_then(|name| themes.iter().position(|(existing, _)| existing == &name))
        .or_else(|| themes.iter().position(|(_, theme)| *theme == active))
        .unwrap_or(0);
    ResolvedThemes {
        active,
        themes,
        index,
        warning,
    }
}

fn load_catalog_paths(catalog_path: &Path) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(catalog_path) else {
        return Vec::new();
    };
    let Ok(catalog) = toml::from_str::<toml::Value>(&contents) else {
        return Vec::new();
    };
    catalog
        .get("themes")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_string)
        .collect()
}

fn insert_theme_entry(themes: &mut Vec<(String, Palette)>, name: String, theme: Palette) {
    themes.retain(|(existing, _)| existing != &name);
    themes.push((name, theme));
}

fn expand_home(home: Option<&Path>, configured_path: &str) -> PathBuf {
    configured_path
        .strip_prefix("~/")
        .zip(home.filter(|path| !path.as_os_str().is_empty()))
        .map(|(rest, home)| home.join(rest))
        .unwrap_or_else(|| PathBuf::from(configured_path))
}

fn theme_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("theme")
        .replace('-', " ")
}

fn parse_theme(text: &str) -> Result<Palette, ThemeError> {
    let value: toml::Value = toml::from_str(text).map_err(|source| ThemeError::Parse {
        path: PathBuf::from("<theme>"),
        source,
    })?;
    if value.get("colors").is_some() {
        let file: SharedPaletteFile = value.try_into().map_err(|source| ThemeError::Parse {
            path: PathBuf::from("<theme>"),
            source,
        })?;
        return parse_shared_theme(&file);
    }
    let file: PaletteFile = value.try_into().map_err(|source| ThemeError::Parse {
        path: PathBuf::from("<theme>"),
        source,
    })?;
    parse_legacy_theme(&file)
}

fn parse_legacy_theme(file: &PaletteFile) -> Result<Palette, ThemeError> {
    let document = if let Some(document) = &file.document {
        DocumentPalette {
            background: parse_rgb("document.background", &document.background)?,
            foreground: parse_rgb("document.foreground", &document.foreground)?,
        }
    } else {
        DocumentPalette {
            background: parse_rgb("bg_dark", &file.bg_dark)?,
            foreground: parse_rgb("fg", &file.fg)?,
        }
    };
    macro_rules! color {
        ($field:ident) => {
            parse_color(stringify!($field), &file.$field)?
        };
    }
    let palette = Palette {
        bg: color!(bg),
        bg_dark: color!(bg_dark),
        bg_dark1: color!(bg_dark1),
        bg_highlight: color!(bg_highlight),
        blue: color!(blue),
        blue1: color!(blue1),
        blue7: color!(blue7),
        comment: color!(comment),
        cyan: color!(cyan),
        dark3: color!(dark3),
        fg: color!(fg),
        fg_dark: color!(fg_dark),
        green: color!(green),
        magenta: color!(magenta),
        yellow: color!(yellow),
        document,
    };
    Ok(palette)
}

fn parse_shared_theme(file: &SharedPaletteFile) -> Result<Palette, ThemeError> {
    let bg = shared_color(
        file,
        "bg",
        &["background"],
        &["bg", "base"],
        TOKYO_NIGHT_MOON.bg,
    )?;
    let bg_dark = shared_color(
        file,
        "bg_dark",
        &["background_dark", "background"],
        &["bg_dark", "mantle", "bg"],
        bg,
    )?;
    let bg_dark1 = shared_color(
        file,
        "bg_dark1",
        &["background_deep", "background_dark"],
        &["bg_dark1", "crust", "bg_dark", "mantle"],
        bg_dark,
    )?;
    let bg_highlight = shared_color(
        file,
        "bg_highlight",
        &["cursor_bg", "selection"],
        &["bg_highlight", "surface0", "bg"],
        bg,
    )?;
    let fg = shared_color(file, "fg", &["text"], &["fg", "text"], TOKYO_NIGHT_MOON.fg)?;
    let fg_dark = shared_color(
        file,
        "fg_dark",
        &["text_dim"],
        &["fg_dark", "fg_dim", "subtext0", "comment"],
        fg,
    )?;
    let fg_gutter = shared_color(
        file,
        "fg_gutter",
        &["border", "text_muted"],
        &["fg_gutter", "fg_muted", "surface1", "overlay0"],
        fg_dark,
    )?;
    let blue = shared_color(
        file,
        "blue",
        &["selection", "heading"],
        &["blue", "lavender", "fg_bright", "fg"],
        fg,
    )?;
    let blue1 = shared_color(
        file,
        "blue1",
        &["key"],
        &["blue1", "sky", "cyan", "blue"],
        blue,
    )?;
    let blue7 = shared_color(
        file,
        "blue7",
        &["border"],
        &["blue7", "overlay0", "fg_muted"],
        fg_gutter,
    )?;
    let comment = shared_color(
        file,
        "comment",
        &["text_dim", "text_muted"],
        &["comment", "overlay0", "fg_dim"],
        fg_dark,
    )?;
    let cyan = shared_color(
        file,
        "cyan",
        &["key"],
        &["cyan", "sky", "aqua", "blue1"],
        blue1,
    )?;
    let dark3 = shared_color(
        file,
        "dark3",
        &[],
        &["dark3", "overlay1", "fg_muted"],
        fg_gutter,
    )?;
    let green = shared_color(
        file,
        "green",
        &[],
        &["green", "teal", "lime", "fg_bright"],
        fg,
    )?;
    let magenta = shared_color(
        file,
        "magenta",
        &["accent"],
        &["magenta", "mauve", "pink", "fg_bright"],
        fg,
    )?;
    let yellow = shared_color(
        file,
        "yellow",
        &["heading"],
        &["yellow", "peach", "fg_bright"],
        fg,
    )?;

    let document = if let Some(document) = &file.document {
        DocumentPalette {
            background: parse_rgb("document.background", &document.background)?,
            foreground: parse_rgb("document.foreground", &document.foreground)?,
        }
    } else {
        DocumentPalette {
            background: rgb_channels(bg_dark),
            foreground: rgb_channels(fg),
        }
    };
    Ok(Palette {
        bg,
        bg_dark,
        bg_dark1,
        bg_highlight,
        blue,
        blue1,
        blue7,
        comment,
        cyan,
        dark3,
        fg,
        fg_dark,
        green,
        magenta,
        yellow,
        document,
    })
}

fn shared_color(
    file: &SharedPaletteFile,
    field: &str,
    ui_roles: &[&str],
    color_names: &[&str],
    fallback: Color,
) -> Result<Color, ThemeError> {
    for role in ui_roles {
        if let Some(reference) = file.ui.get(*role) {
            if reference.starts_with('#') {
                return parse_color(field, reference);
            }
            if let Some(value) = file.colors.get(reference) {
                return parse_color(field, value);
            }
        }
    }
    for name in color_names {
        if let Some(value) = file.colors.get(*name) {
            return parse_color(field, value);
        }
    }
    Ok(fallback)
}

fn rgb_channels(color: Color) -> [u8; 3] {
    match color {
        Color::Rgb { r, g, b } => [r, g, b],
        _ => [0, 0, 0],
    }
}

fn parse_color(field: &str, value: &str) -> Result<Color, ThemeError> {
    let [red, green, blue] = parse_rgb(field, value)?;
    Ok(rgb(red, green, blue))
}

pub(crate) fn parse_rgb(field: &str, value: &str) -> Result<[u8; 3], ThemeError> {
    let Some(hex) = value
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    else {
        return Err(ThemeError::InvalidColor {
            field: field.to_string(),
            value: value.to_string(),
        });
    };
    let channel = |i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
    Ok([channel(0), channel(2), channel(4)])
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKYO_NIGHT_MOON_TOML: &str = r##"
bg = "#222436"
bg_dark = "#1e2030"
bg_dark1 = "#191B29"
bg_highlight = "#2f334d"
blue = "#82aaff"
blue0 = "#3e68d7"
blue1 = "#65bcff"
blue2 = "#0db9d7"
blue5 = "#89ddff"
blue6 = "#b4f9f8"
blue7 = "#394b70"
comment = "#636da6"
cyan = "#86e1fc"
dark3 = "#545c7e"
dark5 = "#737aa2"
fg = "#c8d3f5"
fg_dark = "#828bb8"
fg_gutter = "#3b4261"
green = "#c3e88d"
green1 = "#4fd6be"
green2 = "#41a6b5"
magenta = "#c099ff"
magenta2 = "#ff007c"
orange = "#ff966c"
purple = "#fca7ea"
red = "#ff757f"
red1 = "#c53b53"
teal = "#4fd6be"
terminal_black = "#444a73"
yellow = "#ffc777"

[git]
add = "#b8db87"
change = "#7ca1f2"
delete = "#e26a75"
"##;

    #[test]
    fn palette_keeps_requested_visible_colors() {
        assert_eq!(TOKYO_NIGHT_MOON.bg, rgb(0x22, 0x24, 0x36));
        assert_eq!(TOKYO_NIGHT_MOON.bg_dark, rgb(0x1e, 0x20, 0x30));
        assert_eq!(TOKYO_NIGHT_MOON.fg, rgb(0xc8, 0xd3, 0xf5));
        assert_eq!(TOKYO_NIGHT_MOON.blue, rgb(0x82, 0xaa, 0xff));
        assert_eq!(TOKYO_NIGHT_MOON.magenta, rgb(0xc0, 0x99, 0xff));
        assert_eq!(TOKYO_NIGHT_MOON.green, rgb(0xc3, 0xe8, 0x8d));
        assert_eq!(TOKYO_NIGHT_MOON.comment, rgb(0x63, 0x6d, 0xa6));
        assert_eq!(
            TOKYO_NIGHT_MOON.document,
            DocumentPalette {
                background: [0x1e, 0x20, 0x30],
                foreground: [0xc8, 0xd3, 0xf5],
            }
        );
    }

    #[test]
    fn parses_complete_theme_file() {
        assert_eq!(
            parse_theme(TOKYO_NIGHT_MOON_TOML).expect("theme"),
            TOKYO_NIGHT_MOON
        );
    }

    #[test]
    fn parses_document_palette_override() {
        let text = TOKYO_NIGHT_MOON_TOML.replace(
            "[git]",
            "[document]\nbackground = \"#101820\"\nforeground = \"#d8e0e8\"\n\n[git]",
        );
        assert_eq!(
            parse_theme(&text).expect("theme").document,
            DocumentPalette {
                background: [0x10, 0x18, 0x20],
                foreground: [0xd8, 0xe0, 0xe8],
            }
        );
    }

    #[test]
    fn parses_shared_catppuccin_palette() {
        let theme = parse_theme(
            r##"
[colors]
base = "#1e1e2e"
mantle = "#181825"
crust = "#11111b"
surface0 = "#313244"
surface1 = "#45475a"
overlay0 = "#6c7086"
overlay1 = "#7f849c"
overlay2 = "#9399b2"
text = "#cdd6f4"
subtext0 = "#a6adc8"
red = "#f38ba8"
maroon = "#eba0ac"
peach = "#fab387"
yellow = "#f9e2af"
green = "#a6e3a1"
teal = "#94e2d5"
sky = "#89dceb"
blue = "#89b4fa"
mauve = "#cba6f7"
pink = "#f5c2e7"

[ui]
background = "base"
background_dark = "mantle"
background_deep = "crust"
border = "surface1"
accent = "mauve"
selection = "blue"
key = "sky"
text = "text"
text_dim = "subtext0"
error = "red"
cursor_bg = "surface0"
"##,
        )
        .expect("shared theme");

        assert_eq!(theme.bg, rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(theme.bg_dark, rgb(0x18, 0x18, 0x25));
        assert_eq!(theme.fg, rgb(0xcd, 0xd6, 0xf4));
        assert_eq!(theme.blue, rgb(0x89, 0xb4, 0xfa));
        assert_eq!(theme.magenta, rgb(0xcb, 0xa6, 0xf7));
    }

    #[test]
    fn expands_home_in_explicit_theme_path() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            expand_home(Some(root.path()), "~/.config/themes/synthetic.toml"),
            root.path().join(".config/themes/synthetic.toml")
        );
        assert_eq!(
            expand_home(Some(Path::new("")), "~/.config/themes/synthetic.toml"),
            PathBuf::from("~/.config/themes/synthetic.toml")
        );
        assert_eq!(
            expand_home(None, "~/.config/themes/synthetic.toml"),
            PathBuf::from("~/.config/themes/synthetic.toml")
        );
    }

    #[test]
    fn catalog_loads_only_explicit_theme_paths() {
        let root = tempfile::tempdir().unwrap();
        let themes_dir = root.path().join("themes");
        std::fs::create_dir_all(&themes_dir).unwrap();
        std::fs::write(
            themes_dir.join("synthetic-theme.toml"),
            TOKYO_NIGHT_MOON_TOML.replace("#222436", "#101010"),
        )
        .unwrap();
        std::fs::write(themes_dir.join("unlisted.toml"), TOKYO_NIGHT_MOON_TOML).unwrap();
        let catalog = root.path().join("catalog.toml");
        std::fs::write(
            &catalog,
            format!(
                "themes = [\"{}\"]\n",
                themes_dir.join("synthetic-theme.toml").display()
            ),
        )
        .unwrap();

        let resolved = resolve_with_home(
            Some(catalog.to_str().unwrap()),
            Some(themes_dir.join("synthetic-theme.toml").to_str().unwrap()),
            Some(root.path()),
        );

        assert_eq!(resolved.themes.len(), 1);
        assert_eq!(resolved.themes[0].0, "synthetic theme");
        assert_eq!(resolved.themes[0].1.bg, rgb(0x10, 0x10, 0x10));
        assert_eq!(resolved.active.bg, rgb(0x10, 0x10, 0x10));

        let fallback = resolve_with_home(Some(catalog.to_str().unwrap()), None, Some(root.path()));
        assert_eq!(fallback.active, TOKYO_NIGHT_MOON);
        assert_eq!(fallback.themes[fallback.index].1, TOKYO_NIGHT_MOON);
    }

    #[test]
    fn themes_ignore_unknown_and_retired_fields() {
        let minimal = TOKYO_NIGHT_MOON_TOML
            .replace("teal = \"#4fd6be\"\n", "")
            .replace("orange = \"#ff966c\"", "orange = \"not-a-color\"")
            .replace("delete = \"#e26a75\"", "delete = \"not-a-color\"");
        assert_eq!(parse_theme(&minimal).unwrap(), TOKYO_NIGHT_MOON);
        assert!(
            parse_theme("[colors]\nsapphire = \"not-a-color\"\n[git]\ndelete = \"nope\"\n").is_ok()
        );
    }

    #[test]
    fn rejects_invalid_color() {
        let error = parse_color("bg", "222436").expect_err("missing hash must fail");
        assert!(error.to_string().contains("#RRGGBB"));
    }
}
