use std::fs;
use std::io;
use std::path::PathBuf;

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
    git: GitPaletteFile,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitPaletteFile {
    add: String,
    change: String,
    delete: String,
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

pub fn load(name: &str) -> Result<Palette, ThemeError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(ThemeError::InvalidName(name.to_string()));
    }
    let directory = crate::config::config_dir().ok_or(ThemeError::MissingConfigDirectory)?;
    let path = directory.join("themes").join(format!("{name}.toml"));
    let text = fs::read_to_string(&path).map_err(|source| ThemeError::Read {
        path: path.clone(),
        source,
    })?;
    parse_theme(&text).map_err(|error| match error {
        ThemeError::Parse { source, .. } => ThemeError::Parse { path, source },
        other => other,
    })
}

fn parse_theme(text: &str) -> Result<Palette, ThemeError> {
    let file: PaletteFile = toml::from_str(text).map_err(|source| ThemeError::Parse {
        path: PathBuf::from("<theme>"),
        source,
    })?;
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
        git: GitPalette {
            add: parse_color("git.add", &file.git.add)?,
            change: parse_color("git.change", &file.git.change)?,
            delete: parse_color("git.delete", &file.git.delete)?,
        },
    })
}

fn parse_color(field: &str, value: &str) -> Result<Color, ThemeError> {
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
    Ok(rgb(red, green, blue))
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
    }

    #[test]
    fn parses_complete_theme_file() {
        assert_eq!(
            parse_theme(TOKYO_NIGHT_MOON_TOML).expect("theme"),
            TOKYO_NIGHT_MOON
        );
    }

    #[test]
    fn rejects_invalid_color() {
        let error = parse_color("bg", "222436").expect_err("missing hash must fail");
        assert!(error.to_string().contains("#RRGGBB"));
    }
}
