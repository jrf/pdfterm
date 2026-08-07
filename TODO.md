# TODO

## Now

- [ ] Add Kitty graphics passthrough for tmux. #feature
- [ ] Measure render, compression, and transfer latency on direct SSH sessions. #experiment

## Next

- [ ] Publish release archives for macOS arm64 and Linux x86_64/aarch64. #chore
- [ ] Add arbitrary zoom levels beyond the fit modes. #feature
- [ ] Transmit each page once and re-place crops on scroll instead of re-transmitting. #improvement

## Later

- [ ] Add text search after page navigation meets the latency budget. #feature
- [ ] Follow internal links and named destinations. #feature

## Scrapped

Pure-Rust PDF rendering: Hayro states that its renderer has not received performance work yet, which conflicts with the latency requirement.

## Done

- [x] Add a fuzzy PDF picker for startup and in-viewer file changes. #feature
- [x] Reload changed PDFs without interrupting navigation or displaying partial writes. #feature
- [x] Embed checksummed PDFium builds for one-command Cargo installation. #chore
- [x] Render fitted pages through PDFium on a background worker. #feature
- [x] Send zlib-compressed RGBA data through Kitty's chunked graphics protocol. #feature
- [x] Cache adjacent pages and prioritize foreground render requests. #improvement
- [x] Add fit-width/fit-height modes with page scrolling and viewport panning. #feature
- [x] Add invert (dark-mode) rendering. #feature
- [x] Add an outline / table-of-contents overlay. #feature
- [x] Add a go-to-page prompt. #feature
- [x] Copy the current page's text to the clipboard over SSH (OSC 52). #feature
- [x] List recently opened documents first in the picker. #improvement
- [x] Load fit mode and invert defaults from a config file. #feature
