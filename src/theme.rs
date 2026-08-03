use crossterm::style::Color;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
