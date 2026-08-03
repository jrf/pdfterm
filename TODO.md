# TODO

## Now

- [ ] Add Kitty graphics passthrough for tmux. #feature
- [ ] Measure render, compression, and transfer latency on direct SSH sessions. #experiment

## Next

- [ ] Package the release binary with PDFium for macOS arm64 and Linux x86_64/aarch64. #chore
- [ ] Add zoom and viewport panning without rerendering unchanged pages. #feature

## Later

- [ ] Add text search after page navigation meets the latency budget. #feature

## Scrapped

Pure-Rust PDF rendering: Hayro states that its renderer has not received performance work yet, which conflicts with the latency requirement.

## Done

- [x] Render fitted pages through PDFium on a background worker. #feature
- [x] Send zlib-compressed RGBA data through Kitty's chunked graphics protocol. #feature
- [x] Cache adjacent pages and prioritize foreground render requests. #improvement

