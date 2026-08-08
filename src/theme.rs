use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crossterm::style::Color;
use serde::Deserialize;
use thiserror::Error;

pub const DEFAULT_THEME_NAME: &str = "tokyo-night-moon";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GitPalette {
    pub add: Color,
    pub change: Color,
    pub delete: Color,
}

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
    pub blue0: Color,
    pub blue1: Color,
    pub blue2: Color,
    pub blue5: Color,
    pub blue6: Color,
    pub blue7: Color,
    pub comment: Color,
    pub cyan: Color,
    pub dark3: Color,
    pub dark5: Color,
    pub fg: Color,
    pub fg_dark: Color,
    pub fg_gutter: Color,
    pub green: Color,
    pub green1: Color,
    pub green2: Color,
    pub magenta: Color,
    pub magenta2: Color,
    pub orange: Color,
    pub purple: Color,
    pub red: Color,
    pub red1: Color,
    pub teal: Color,
    pub terminal_black: Color,
    pub yellow: Color,
    pub document: DocumentPalette,
    pub git: GitPalette,
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
    blue0: rgb(0x3e, 0x68, 0xd7),
    blue1: rgb(0x65, 0xbc, 0xff),
    blue2: rgb(0x0d, 0xb9, 0xd7),
    blue5: rgb(0x89, 0xdd, 0xff),
    blue6: rgb(0xb4, 0xf9, 0xf8),
    blue7: rgb(0x39, 0x4b, 0x70),
    comment: rgb(0x63, 0x6d, 0xa6),
    cyan: rgb(0x86, 0xe1, 0xfc),
    dark3: rgb(0x54, 0x5c, 0x7e),
    dark5: rgb(0x73, 0x7a, 0xa2),
    fg: rgb(0xc8, 0xd3, 0xf5),
    fg_dark: rgb(0x82, 0x8b, 0xb8),
    fg_gutter: rgb(0x3b, 0x42, 0x61),
    green: rgb(0xc3, 0xe8, 0x8d),
    green1: rgb(0x4f, 0xd6, 0xbe),
    green2: rgb(0x41, 0xa6, 0xb5),
    magenta: rgb(0xc0, 0x99, 0xff),
    magenta2: rgb(0xff, 0x00, 0x7c),
    orange: rgb(0xff, 0x96, 0x6c),
    purple: rgb(0xfc, 0xa7, 0xea),
    red: rgb(0xff, 0x75, 0x7f),
    red1: rgb(0xc5, 0x3b, 0x53),
    teal: rgb(0x4f, 0xd6, 0xbe),
    terminal_black: rgb(0x44, 0x4a, 0x73),
    yellow: rgb(0xff, 0xc7, 0x77),
    document: DocumentPalette {
        background: [0x1e, 0x20, 0x30],
        foreground: [0xc8, 0xd3, 0xf5],
    },
    git: GitPalette {
        add: rgb(0xb8, 0xdb, 0x87),
        change: rgb(0x7c, 0xa1, 0xf2),
        delete: rgb(0xe2, 0x6a, 0x75),
    },
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaletteFile {
    bg: String,
    bg_dark: String,
    bg_dark1: String,
    bg_highlight: String,
    blue: String,
    blue0: String,
    blue1: String,
    blue2: String,
    blue5: String,
    blue6: String,
    blue7: String,
    comment: String,
    cyan: String,
    dark3: String,
    dark5: String,
    fg: String,
    fg_dark: String,
    fg_gutter: String,
    green: String,
    green1: String,
    green2: String,
    magenta: String,
    magenta2: String,
    orange: String,
    purple: String,
    red: String,
    red1: String,
    teal: String,
    terminal_black: String,
    yellow: String,
    document: Option<DocumentPaletteFile>,
    git: GitPaletteFile,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentPaletteFile {
    background: String,
    foreground: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitPaletteFile {
    add: String,
    change: String,
    delete: String,
}

#[derive(Debug, Deserialize)]
struct SharedPaletteFile {
    colors: BTreeMap<String, String>,
    #[serde(default)]
    ui: BTreeMap<String, String>,
    document: Option<DocumentPaletteFile>,
    git: Option<GitPaletteFile>,
}

#[derive(Debug, Error)]
pub enum ThemeError {
    #[error("invalid theme name {0:?}; use letters, numbers, dashes, or underscores")]
    InvalidName(String),
    #[error("could not locate the pdfterm configuration directory")]
    MissingConfigDirectory,
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

pub fn load_or_default(name: &str) -> Palette {
    match load(name) {
        Ok(theme) => theme,
        Err(ThemeError::Read { source, .. })
            if name == DEFAULT_THEME_NAME && source.kind() == io::ErrorKind::NotFound =>
        {
            TOKYO_NIGHT_MOON
        }
        Err(error) => {
            eprintln!("pdfterm: {error}; using the built-in {DEFAULT_THEME_NAME} theme");
            TOKYO_NIGHT_MOON
        }
    }
}

pub fn available_themes() -> Vec<(String, Palette)> {
    let Some(config_root) = crate::config::config_root() else {
        return vec![(DEFAULT_THEME_NAME.to_string(), TOKYO_NIGHT_MOON)];
    };
    discover_themes(&config_root)
}

pub fn load(name: &str) -> Result<Palette, ThemeError> {
    if !valid_theme_name(name) {
        return Err(ThemeError::InvalidName(name.to_string()));
    }
    let config_root = crate::config::config_root().ok_or(ThemeError::MissingConfigDirectory)?;
    let path = theme_path(&config_root, name);
    load_theme_file(&path)
}

fn valid_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
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

fn discover_themes(config_root: &Path) -> Vec<(String, Palette)> {
    let mut themes = BTreeMap::from([(DEFAULT_THEME_NAME.to_string(), TOKYO_NIGHT_MOON)]);
    overlay_theme_directory(&mut themes, &config_root.join("themes"));
    overlay_theme_directory(&mut themes, &config_root.join("pdfterm/themes"));
    themes.into_iter().collect()
}

fn overlay_theme_directory(themes: &mut BTreeMap<String, Palette>, directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if !valid_theme_name(name) {
            continue;
        }
        if let Ok(theme) = load_theme_file(&path) {
            themes.insert(name.to_string(), theme);
        }
    }
}

fn theme_path(config_root: &Path, name: &str) -> PathBuf {
    let app_path = config_root
        .join("pdfterm")
        .join("themes")
        .join(format!("{name}.toml"));
    let shared_path = config_root.join("themes").join(format!("{name}.toml"));
    if app_path.is_file() || !shared_path.is_file() {
        app_path
    } else {
        shared_path
    }
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
    Ok(Palette {
        bg: color!(bg),
        bg_dark: color!(bg_dark),
        bg_dark1: color!(bg_dark1),
        bg_highlight: color!(bg_highlight),
        blue: color!(blue),
        blue0: color!(blue0),
        blue1: color!(blue1),
        blue2: color!(blue2),
        blue5: color!(blue5),
        blue6: color!(blue6),
        blue7: color!(blue7),
        comment: color!(comment),
        cyan: color!(cyan),
        dark3: color!(dark3),
        dark5: color!(dark5),
        fg: color!(fg),
        fg_dark: color!(fg_dark),
        fg_gutter: color!(fg_gutter),
        green: color!(green),
        green1: color!(green1),
        green2: color!(green2),
        magenta: color!(magenta),
        magenta2: color!(magenta2),
        orange: color!(orange),
        purple: color!(purple),
        red: color!(red),
        red1: color!(red1),
        teal: color!(teal),
        terminal_black: color!(terminal_black),
        yellow: color!(yellow),
        document,
        git: GitPalette {
            add: parse_color("git.add", &file.git.add)?,
            change: parse_color("git.change", &file.git.change)?,
            delete: parse_color("git.delete", &file.git.delete)?,
        },
    })
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
    let blue0 = shared_color(file, "blue0", &[], &["blue0", "sapphire", "blue"], blue)?;
    let blue1 = shared_color(
        file,
        "blue1",
        &["key"],
        &["blue1", "sky", "cyan", "blue"],
        blue,
    )?;
    let blue2 = shared_color(
        file,
        "blue2",
        &[],
        &["blue2", "teal", "cyan", "blue"],
        blue1,
    )?;
    let blue5 = shared_color(file, "blue5", &[], &["blue5", "sky", "cyan", "blue"], blue1)?;
    let blue6 = shared_color(
        file,
        "blue6",
        &[],
        &["blue6", "lavender", "cyan", "blue"],
        blue5,
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
    let dark5 = shared_color(
        file,
        "dark5",
        &[],
        &["dark5", "overlay2", "fg_dim"],
        fg_dark,
    )?;
    let green = shared_color(
        file,
        "green",
        &[],
        &["green", "teal", "lime", "fg_bright"],
        fg,
    )?;
    let green1 = shared_color(file, "green1", &[], &["green1", "teal", "green"], green)?;
    let green2 = shared_color(file, "green2", &[], &["green2", "teal", "green"], green1)?;
    let magenta = shared_color(
        file,
        "magenta",
        &["accent"],
        &["magenta", "mauve", "pink", "fg_bright"],
        fg,
    )?;
    let magenta2 = shared_color(
        file,
        "magenta2",
        &[],
        &["magenta2", "pink", "mauve"],
        magenta,
    )?;
    let orange = shared_color(file, "orange", &[], &["orange", "peach", "yellow"], magenta)?;
    let purple = shared_color(
        file,
        "purple",
        &[],
        &["purple", "mauve", "pink", "magenta"],
        magenta,
    )?;
    let red = shared_color(file, "red", &["error"], &["red", "maroon"], magenta)?;
    let red1 = shared_color(file, "red1", &[], &["red1", "maroon", "red"], red)?;
    let teal = shared_color(file, "teal", &[], &["teal", "cyan", "green"], cyan)?;
    let terminal_black = shared_color(
        file,
        "terminal_black",
        &["background_deep"],
        &["terminal_black", "crust", "bg_dark1"],
        bg_dark1,
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
    let git = if let Some(git) = &file.git {
        GitPalette {
            add: parse_color("git.add", &git.add)?,
            change: parse_color("git.change", &git.change)?,
            delete: parse_color("git.delete", &git.delete)?,
        }
    } else {
        GitPalette {
            add: green,
            change: blue,
            delete: red,
        }
    };

    Ok(Palette {
        bg,
        bg_dark,
        bg_dark1,
        bg_highlight,
        blue,
        blue0,
        blue1,
        blue2,
        blue5,
        blue6,
        blue7,
        comment,
        cyan,
        dark3,
        dark5,
        fg,
        fg_dark,
        fg_gutter,
        green,
        green1,
        green2,
        magenta,
        magenta2,
        orange,
        purple,
        red,
        red1,
        teal,
        terminal_black,
        yellow,
        document,
        git,
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

fn parse_rgb(field: &str, value: &str) -> Result<[u8; 3], ThemeError> {
    let Some(hex) = value.strip_prefix('#').filter(|hex| hex.len() == 6) else {
        return Err(ThemeError::InvalidColor {
            field: field.to_string(),
            value: value.to_string(),
        });
    };
    let parse_channel = |range| u8::from_str_radix(&hex[range], 16).ok();
    let Some((red, (green, blue))) =
        parse_channel(0..2).zip(parse_channel(2..4).zip(parse_channel(4..6)))
    else {
        return Err(ThemeError::InvalidColor {
            field: field.to_string(),
            value: value.to_string(),
        });
    };
    Ok([red, green, blue])
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
        assert_eq!(theme.git.add, rgb(0xa6, 0xe3, 0xa1));
    }

    #[test]
    fn app_theme_overrides_shared_theme_path() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("themes/moon.toml");
        let app = root.path().join("pdfterm/themes/moon.toml");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(&shared, "shared").unwrap();

        assert_eq!(theme_path(root.path(), "moon"), shared);

        std::fs::write(&app, "app").unwrap();
        assert_eq!(theme_path(root.path(), "moon"), app);
    }

    #[test]
    fn discovers_shared_themes_with_app_specific_overrides() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("themes/custom.toml");
        let app = root.path().join("pdfterm/themes/custom.toml");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(&shared, TOKYO_NIGHT_MOON_TOML.replace("#222436", "#101010")).unwrap();
        std::fs::write(&app, TOKYO_NIGHT_MOON_TOML.replace("#222436", "#202020")).unwrap();
        std::fs::write(root.path().join("themes/invalid.toml"), "not toml").unwrap();

        let themes = discover_themes(root.path());
        let custom = themes
            .iter()
            .find(|(name, _)| name == "custom")
            .expect("custom theme");

        assert_eq!(custom.1.bg, rgb(0x20, 0x20, 0x20));
        assert!(themes.iter().any(|(name, _)| name == DEFAULT_THEME_NAME));
        assert!(!themes.iter().any(|(name, _)| name == "invalid"));
    }

    #[test]
    fn rejects_invalid_color() {
        let error = parse_color("bg", "222436").expect_err("missing hash must fail");
        assert!(error.to_string().contains("#RRGGBB"));
    }
}
