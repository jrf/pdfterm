# pdfterm

`pdfterm` is a low-latency PDF viewer for Kitty. It renders where the command runs, compresses each page once, and sends the bitmap through Kitty's graphics protocol. A direct SSH session needs no local helper.

![pdfterm rendering an arXiv paper in dark mode inside Kitty](assets/pdfterm-dark-mode-arxiv.png)

The current page is fitted to the terminal and kept when the file reloads. Omit the path, or press `f`, to open another PDF in a new tab.

## Requirements

- Rust 1.85 or newer
- Kitty 0.20 or newer
- macOS arm64, Linux x86_64, or Linux aarch64

Run it in Kitty directly. tmux graphics passthrough is still to come.

## Install

```console
cargo install --git https://github.com/jrf/pdfterm.git
```

PDFium revision 7881 is checksummed and embedded, then extracted on first use to `$XDG_CACHE_HOME/pdfterm` or `~/.cache/pdfterm`. From a checkout, `cargo build --release`.

## Run

```console
pdfterm
```

## Keys

Press `?` for this list in the viewer.

| Key | Action |
| --- | --- |
| `j` `k` `↑` `↓` | Scroll vertically, then change page |
| `h` `l` `←` `→` | Scroll horizontally, then change page |
| `Space` `PageDown` / `Backspace` `PageUp` | Page the viewport |
| `g` / `G` | First / last page |
| `:` | Go to a page |
| `/` | Search text, or filter the open list |
| `n` / `N` | Next / previous page with a match |
| `m` | Cycle fit: page, width, height |
| `+` / `-` | Zoom by 25% (100–400%) |
| `0` | Reset zoom |
| `i` | Polaris-style dark mode |
| `p` | Render timings |
| `t` / `T` | Outline / theme for this session |
| `y` | Copy this page's text (OSC 52) |
| `Enter` / `L` | Link list / link list with highlights |
| `b` | Back from the last internal link |
| `f` | New tab |
| `Tab` / `Shift-Tab` | Next / previous tab |
| `Alt-1` … `Alt-9` | Jump to a tab |
| `q` | Leave link mode, or close the tab |
| `Esc` | Leave link mode, close a pane, clear a filter, or exit |

## Configuration

`$XDG_CONFIG_HOME/pdfterm/config.toml`, or `~/.config/pdfterm/config.toml`. A missing file uses the defaults. A bad file is reported once and skipped. `invert` is an alias for `dark_mode`.

```toml
fit_mode = "page"               # page, width, height
dark_mode = true
persistent_link_picker = true  # keep the link split open after a link
link_picker_split_percent = 50 # 20–80
link_picker_layout = "auto"    # auto, vertical, horizontal, floating
theme = "~/.config/themes/tokyo-night-moon.toml"
theme_catalog = "~/.config/themes/catalog.toml"
```
