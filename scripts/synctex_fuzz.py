#!/usr/bin/env python3
"""Fuzz forward and inverse SyncTeX mappings for any TeX-built PDF.

Forward probes compare pdfterm's page with raw SyncTeX results. Inverse probes
enumerate painted PDF words with PyMuPDF, then send all selected word centers
through pdfterm's PDFium + inverse resolver batch diagnostic. File/line identity
is checked even for coarse results; fragile frames use the clicked sheet's
original source-map box, never the final reused .vrb contents. Precision is
checked against literal printed source spans; unsupported math remains separate.
The report is one JSON object with exact probe failures and deterministic
coverage totals. Any failed probe exits nonzero.

Usage: synctex_fuzz.py PDF [--source TEX] [--lines N] [--page N ...]
       [--pages N] [--words N] [--seed N] [--allow-stale]

By default every eligible painted word on every page is probed. --pages and
--words provide seed-deterministic quick caps; --pages 0 disables inverse
probes and --words 0 means all words on each selected page. Repeat --page to
probe exact one-based pages (for example, --page 27 --page 28).
"""

import argparse
import gzip
import json
import random
import re
import subprocess
import sys
import unicodedata
from collections.abc import Iterator
from pathlib import Path

WordPoint = tuple[str, tuple[float, float, float, float], float]
PDFTERM_BINARY = Path(__file__).parent.parent / "target" / "release" / "pdfterm"


def raw_pages(pdf: Path, source: Path, line: int, column: int = 1) -> set[int]:
    result = subprocess.run(
        ["synctex", "view", "-i", f"{line}:{column}:{source}", "-o", str(pdf)],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"synctex view: {result.stderr.strip()[:200]}")
    return {int(p) for p in re.findall(r"^Page:(\d+)$", result.stdout, re.MULTILINE)}


def raw_inverse(pdf: Path, x: float, y: float, page: int) -> tuple[int, str] | None:
    result = subprocess.run(
        ["synctex", "edit", "-o", f"{page}:{x:.2f}:{y:.2f}:{pdf}"],
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"synctex edit: {result.stderr.strip()[:200]}")
    matches = re.findall(r"^Input:(.*)$\n^Line:(\d+)", result.stdout, re.MULTILINE)
    return (int(matches[-1][1]), matches[-1][0]) if matches else None


def source_line_text(text: str) -> str:
    escaped = False
    for index, char in enumerate(text):
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == "%":
            return text[:index]
    return text


def sample_source_positions(
    source: str, rng: random.Random, count: int
) -> list[tuple[int, int]]:
    candidates = []
    for line, row in enumerate(source.splitlines(), 1):
        if not row.strip() or row.lstrip().startswith("%"):
            continue
        row = source_line_text(row)
        words = [
            match.start() + 1
            for match in re.finditer(r"[A-Za-z]{6,}", row)
            if match.start() == 0 or row[match.start() - 1] != "\\"
        ]
        candidates.append((line, rng.choice(words) if words else 1))
    return sorted(rng.sample(candidates, min(count, len(candidates))))


def normalized_words(value: str) -> list[str]:
    folded = "".join(
        char
        for char in unicodedata.normalize("NFKD", value).casefold()
        if not unicodedata.combining(char)
    )
    return re.findall(r"\w+", folded)


def context_score(tokens: list[str], hint: dict) -> int | None:
    source = [normalized_words(word) for word in hint["words"]]
    if any(len(word) != 1 for word in source):
        return None
    source = [word[0] for word in source]
    selected = hint["selected"]
    best = None
    for index, word in enumerate(tokens):
        if word != source[selected]:
            continue
        score = 0
        for distance in range(1, 4):
            for direction in (-1, 1):
                neighbor = index + direction * distance
                source_neighbor = selected + direction * distance
                if (
                    0 <= neighbor < len(tokens)
                    and 0 <= source_neighbor < len(source)
                    and tokens[neighbor] == source[source_neighbor]
                ):
                    score += 4 - distance
        best = max(score, best or 0)
    return best


def pdf_visible_words(pdf: Path) -> list[list[str]]:
    result = subprocess.run(
        ["pdftotext", "-layout", str(pdf), "-"],
        capture_output=True,
        text=True,
        errors="replace",
        timeout=120,
        check=False,
    )
    if result.returncode:
        raise SystemExit(f"pdftotext: {result.stderr.strip()[:200]}")
    pages = result.stdout.split("\f")
    if pages[-1].strip() == "":
        pages.pop()
    return [normalized_words(page) for page in pages]


def forward(
    pdf: Path,
    source: Path,
    positions: list[tuple[int, int]],
    page_count: int,
    visible_words: list[list[str]] | None = None,
) -> Iterator[dict]:
    for line, column in positions:
        try:
            expected = raw_pages(pdf, source, line, column)
        except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
            yield {
                "direction": "forward",
                "line": line,
                "ok": False,
                "error": str(error),
            }
            continue
        try:
            result = subprocess.run(
                [
                    str(pdfterm_bin()),
                    str(pdf),
                    "--synctex-view",
                    str(source),
                    "--line",
                    str(line),
                    "--column",
                    str(column),
                ],
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            yield {
                "direction": "forward",
                "line": line,
                "column": column,
                "ok": False,
                "error": str(error),
            }
            continue
        if result.returncode != 0:
            yield {
                "direction": "forward",
                "line": line,
                "ok": not expected and "no complete match" in result.stderr,
                "error": result.stderr.strip()[:200],
                "raw_pages": sorted(expected),
            }
            continue
        try:
            got = json.loads(result.stdout)
            page = got["page"]
            valid = got["pdf"] == str(pdf) and type(page) is int and 1 <= page <= page_count
        except (ValueError, KeyError, TypeError) as error:
            yield {
                "direction": "forward",
                "line": line,
                "ok": False,
                "error": str(error),
            }
            continue
        entry = {
            "direction": "forward",
            "line": line,
            "column": column,
            "ok": valid,
            "resolved_page": page,
            "raw_pages": sorted(expected),
            "raw_page_match": page in expected,
            "raw_unmapped": not expected,
        }
        if (
            valid
            and visible_words is not None
            and (not expected or len(expected) > 1)
            and got.get("word")
        ):
            try:
                hint = got["word"]
                chosen_score = context_score(visible_words[page - 1], hint)
                if not expected:
                    elsewhere = chosen_score is None and any(
                        context_score(words, hint) is not None
                        for words in visible_words
                    )
                    entry["visible_checked"] = chosen_score is not None or elsewhere
                    if elsewhere:
                        entry["ok"] = False
                        entry["error"] = "raw SyncTeX has no page and selected word is absent from chosen PDF page"
                elif page in expected:
                    entry["visible_checked"] = True
                    better = [
                        candidate
                        for candidate in expected
                        if candidate != page
                        and 1 <= candidate <= page_count
                        and (context_score(visible_words[candidate - 1], hint) or 0) > 0
                    ]
                    if chosen_score is None and better:
                        entry["ok"] = False
                        entry["error"] = (
                            "selected word absent from chosen page but present with source context on another SyncTeX page"
                        )
                        entry["visible_alternatives"] = sorted(better)
            except (IndexError, KeyError, TypeError, ValueError) as error:
                entry["ok"] = False
                entry["error"] = f"invalid forward word or PDF page: {error}"
        yield entry


def read_source_map(pdf: Path) -> dict:
    companion = next((path for path in (pdf.with_suffix(".synctex.gz"), pdf.with_suffix(".synctex")) if path.is_file()), None)
    if companion is None:
        raise RuntimeError("missing SyncTeX source map")
    opener = gzip.open if companion.suffix == ".gz" else open
    inputs, origins, page_inputs = {}, {}, {}
    page = None
    with opener(companion, "rt", encoding="utf-8") as stream:
        size = 0
        for row in stream:
            size += len(row.encode("utf-8"))
            if size > 128 * 1024 * 1024:
                raise RuntimeError("SyncTeX source map exceeds 128 MiB")
            if row.startswith("Input:"):
                tag, _, name = row[len("Input:"):].rstrip("\r\n").partition(":")
                path = Path(name)
                inputs[int(tag)] = (path if path.is_absolute() else pdf.parent / path).resolve()
            elif re.fullmatch(r"\{\d+\s*", row):
                page = int(row[1:])
                page_inputs[page] = set()
            elif page is not None and row.startswith("}"):
                if int(row[1:]) != page:
                    raise RuntimeError("SyncTeX source map has mismatched sheet bounds")
                page = None
            elif page is not None:
                node = re.match(r"([\[(vhkgx$r])(\d+),(\d+)(?:,\d+)?:", row)
                if not origins.get(page):
                    if not node or node[1] not in "[(":
                        raise RuntimeError("SyncTeX sheet has no enclosing source box")
                    origins[page] = (int(node[2]), int(node[3]))
                if node:
                    page_inputs[page].add(int(node[2]))
    if page is not None or 1 not in inputs:
        raise RuntimeError("SyncTeX source map is incomplete or has no main input")
    return {"inputs": inputs, "origins": origins, "page_inputs": page_inputs}


def tex_group_end(text: str, start: int, opening: str = "{") -> int | None:
    closing = "}" if opening == "{" else "]"
    start = tex_space_end(text, start)
    if start >= len(text) or text[start] != opening:
        return None
    depth, at = 1, start + 1
    while at < len(text):
        char = text[at]
        if char == "\\":
            at += 2
            continue
        if char == "%":
            end = text.find("\n", at)
            at = len(text) if end < 0 else end
            continue
        if opening == "[" and char == "{":
            end = tex_group_end(text, at)
            if end is None:
                return None
            at = end
            continue
        depth += (char == opening) - (char == closing)
        at += 1
        if depth == 0:
            return at
    return None


def tex_space_end(text: str, at: int) -> int:
    while at < len(text):
        if text[at].isspace():
            at += 1
        elif text[at] == "%":
            end = text.find("\n", at)
            at = len(text) if end < 0 else end
        else:
            break
    return at


def nonprinting_ranges(text: str) -> list[tuple[int, int]]:
    definitions = {
        "newcommand", "renewcommand", "providecommand", "DeclareRobustCommand",
        "NewDocumentCommand", "RenewDocumentCommand", "ProvideDocumentCommand",
        "DeclareDocumentCommand", "newenvironment", "renewenvironment",
        "def", "gdef", "edef", "xdef",
    }
    keys = {
        "label", "ref", "eqref", "pageref", "autoref", "cref", "Cref", "cite",
        "citep", "citet", "parencite", "textcite", "autocite", "footcite",
        "nocite", "href", "includegraphics", "input", "include", "bibliography",
        "addbibresource",
    }
    hidden, at = [], 0
    while at < len(text):
        if text[at] == "%":
            at = tex_space_end(text, at)
            continue
        if text[at] != "\\":
            at += 1
            continue
        match = re.match(r"\\([A-Za-z@]+)\*?", text[at:])
        if match is None:
            at += 2
            continue
        start, command = at, match[1]
        at += match.end()
        if command in definitions:
            if command in {"def", "gdef", "edef", "xdef"}:
                while at < len(text) and text[at] != "{":
                    if text[at] == "\\":
                        at += 2
                    elif text[at] == "%":
                        at = tex_space_end(text, at)
                    else:
                        at += 1
            else:
                at = tex_space_end(text, at)
                end = tex_group_end(text, at)
                if end is not None:
                    at = end
                else:
                    name = re.match(r"\\[A-Za-z@]+", text[at:])
                    if name:
                        at += name.end()
                while (end := tex_group_end(text, at, "[")) is not None:
                    at = end
                if "Document" in command:
                    end = tex_group_end(text, at)
                    if end is None:
                        hidden.append((start, len(text)))
                        break
                    at = end
            end = tex_group_end(text, at)
            if end is None:
                hidden.append((start, len(text)))
                break
            at = end
            if command.endswith("environment"):
                at = tex_group_end(text, at) or at
            hidden.append((start, at))
        elif command in keys:
            while (end := tex_group_end(text, at, "[")) is not None:
                at = end
            body = tex_space_end(text, at)
            end = tex_group_end(text, body)
            if end is not None:
                hidden.append((start if command == "includegraphics" else body, end))
                at = end
    return hidden


def input_path(name: str, pdf: Path, source: Path) -> Path:
    path = Path(name)
    if path.is_absolute():
        return path.resolve()
    for root in (pdf.parent, source.parent):
        candidate = (root / path).resolve()
        if candidate.is_file():
            return candidate
    return (pdf.parent / path).resolve()


def line_has_word(line: str, word: str) -> bool:
    text = source_line_text(line)
    hidden = nonprinting_ranges(text)
    text = "".join(" " if any(start <= at < end for start, end in hidden) else char for at, char in enumerate(text))
    text = re.sub(r"\\[A-Za-z@]+\*?", " ", text)
    needle = normalized_words(word)
    return len(needle) == 1 and needle[0] in normalized_words(text)


def metadata_line(line: str) -> bool:
    return bool(re.search(r"\\(?:title|subtitle|author|date|institute)\b", source_line_text(line)))


def frame_bounds(lines: list[str], line_number: int) -> tuple[int, int] | None:
    index = line_number - 1
    if not 0 <= index < len(lines):
        return None
    start = None
    for cursor in range(index, -1, -1):
        if re.search(r"\\begin\s*\{frame\}", source_line_text(lines[cursor])):
            start = cursor
            break
        if cursor == index:
            continue
        if re.search(r"\\end\s*\{frame\}", source_line_text(lines[cursor])):
            return None
    if start is None:
        return None
    for end in range(index, len(lines)):
        if re.search(r"\\end\s*\{frame\}", source_line_text(lines[end])):
            return start, end
    return None


def inverse(
    pdf: Path,
    source: Path,
    word_points: dict[int, list[WordPoint]],
) -> Iterator[dict]:
    points = []
    for page, words in word_points.items():
        for word, box, page_height in words:
            x = (box[0] + box[2]) / 2
            y = (box[1] + box[3]) / 2
            points.append({"page": page, "x": x, "y": y, "word": word, "box": box, "page_height": page_height})
    if not points:
        return
    result = subprocess.run(
        [str(pdfterm_bin()), str(pdf), "--synctex-edit-batch"],
        input="".join(json.dumps({key: point[key] for key in ("page", "x", "y")}) + "\n" for point in points),
        capture_output=True,
        text=True,
        timeout=max(30, len(points) * 2),
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"pdfterm --synctex-edit-batch exited {result.returncode}: {result.stderr.strip()[:500]}")
    rows = []
    for line in result.stdout.splitlines():
        try:
            rows.append(json.loads(line))
        except json.JSONDecodeError as error:
            raise RuntimeError(f"invalid batch JSON: {error}: {line[:200]}") from error
    if len(rows) != len(points):
        raise RuntimeError(f"batch returned {len(rows)} results for {len(points)} points")
    source_map = read_source_map(pdf)
    known_inputs = set(source_map["inputs"].values())
    sources = {}

    def source_info(path: Path) -> tuple[str, list[str], list[tuple[int, int]], list[int]]:
        if path not in sources:
            text = path.read_text()
            starts, at = [], 0
            for row in text.splitlines(keepends=True):
                starts.append(at)
                at += len(row)
            sources[path] = (text, text.splitlines(), nonprinting_ranges(text), starts)
        return sources[path]

    for point, batch in zip(points, rows):
        page, word = point["page"], point["word"]
        entry = {
            "direction": "inverse",
            "page": page,
            "word": word,
            "word_box": point["box"],
            "page_height": point["page_height"],
            "raw_anchor": None,
            "location": batch.get("location"),
            "warning": batch.get("warning"),
            "ok": False,
        }
        # Warnings/coarse results do not exempt file, line or remap identity.
        try:
            raw = raw_inverse(pdf, point["x"], point["y"], page)
        except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
            entry["error"] = f"raw SyncTeX edit failed: {error}"
            yield entry
            continue
        if raw is None:
            entry["error"] = "raw SyncTeX edit returned no match"
            yield entry
            continue
        entry["raw_anchor"] = {"line": raw[0], "file": raw[1]}
        raw_file = input_path(raw[1], pdf, source)
        if not batch.get("ok"):
            error = batch.get("error", "batch returned no result")
            if "no text near point" in error or "hit-test failed" in error:
                entry.update(abstained=True, reason="pdfium_hit_unsupported", error=error)
            else:
                entry["error"] = error
            yield entry
            continue
        expected_file, expected_anchor = raw_file, raw[0]
        bounds = None
        remapped = False
        generated = raw_file.suffix == ".vrb" and raw_file.stem == pdf.stem
        if generated:
            origin = source_map["origins"].get(page)
            generated_on_page = any(
                source_map["inputs"].get(tag) == raw_file
                for tag in source_map["page_inputs"].get(page, set())
            )
            if origin and generated_on_page and origin[0] in source_map["inputs"]:
                original = source_map["inputs"][origin[0]]
                original_lines = source_info(original)[1]
                original_bounds = frame_bounds(original_lines, origin[1])
                if (
                    original_bounds
                    and origin[1] == original_bounds[1] + 1
                    and re.search(
                        r"\\begin\s*\{frame\}\s*(?:<[^>]*>\s*)?\[[^\]]*\bfragile\b",
                        "\n".join(source_line_text(row) for row in original_lines[original_bounds[0]:original_bounds[1] + 1]),
                    )
                ):
                    expected_file, expected_anchor = original, origin[1]
                    bounds, remapped = original_bounds, True
        else:
            bounds = frame_bounds(source_info(raw_file)[1], raw[0])
        entry["expected_anchor"] = {"file": str(expected_file), "line": expected_anchor, "remapped": remapped}
        location = batch.get("location")
        if not (generated and not remapped) and not 1 <= expected_anchor <= len(source_info(expected_file)[1]):
            entry["error"] = "page-proven source anchor is outside its source file"
            yield entry
            continue
        if not isinstance(location, dict):
            entry["error"] = "successful batch has no source location"
            yield entry
            continue
        resolved_file = Path(location["file"]).resolve()
        resolved_line = int(location["line"])
        precise = location["precise"]
        if precise is False:
            if resolved_file != expected_file or resolved_line != expected_anchor:
                entry["error"] = "coarse inverse result changed the source file or page-proven anchor"
                yield entry
                continue
            entry["mapping_ok"] = True
            entry.update(abstained=True, reason="resolver_warning" if batch.get("warning") else "coarse_source_refinement")
            yield entry
            continue
        if generated and not remapped:
            entry["error"] = "precise fragile result has no original frame provenance"
            yield entry
            continue
        if resolved_file not in known_inputs:
            entry["error"] = "inverse result selected a file absent from the source map"
            yield entry
            continue
        _, resolved_lines, hidden, starts = source_info(resolved_file)
        if not 1 <= resolved_line <= len(resolved_lines):
            entry["error"] = "SyncTeX source line is invalid"
            yield entry
            continue
        resolved_row = resolved_lines[resolved_line - 1]
        byte = int(location["byte_column"])
        try:
            if byte < 0 or byte > len(resolved_row.encode("utf-8")):
                raise ValueError("source byte column is outside its line")
            prefix = resolved_row.encode("utf-8")[:byte].decode("utf-8")
            if location["column"] != len(prefix.encode("utf-16-le")) // 2 + 1 or location["column_char"] != len(prefix) + 1:
                raise ValueError("source Unicode columns disagree")
        except (UnicodeError, ValueError) as error:
            entry["error"] = str(error)
            yield entry
            continue
        absolute_column = starts[resolved_line - 1] + len(prefix)
        if any(start <= absolute_column < end for start, end in hidden):
            entry["error"] = "precise inverse result selected a known nonprinting argument or definition"
            yield entry
            continue
        pdf_word = batch.get("pdf_word")
        entry["pdfium_word"] = pdf_word
        selected_word = pdf_word or word
        metadata = metadata_line(resolved_row) and line_has_word(resolved_row, selected_word)
        if resolved_file != expected_file and not metadata:
            entry["error"] = "inverse result changed the page-proven source file"
            yield entry
            continue
        if bounds and not metadata and not bounds[0] + 1 <= resolved_line <= bounds[1] + 1:
            entry["error"] = "inverse result escaped the page-proven original frame"
            yield entry
            continue
        entry["mapping_ok"] = True
        if batch.get("warning"):
            entry.update(abstained=True, reason="resolver_warning")
        elif not pdf_word:
            entry.update(abstained=True, reason="pdfium_selected_no_word")
        elif normalized_words(pdf_word) != normalized_words(word):
            entry.update(abstained=True, reason="pdfium_word_mismatch")
        elif line_has_word(resolved_row, pdf_word):
            literal = []
            for char in resolved_row[len(prefix):]:
                if char.isalnum() or literal and unicodedata.combining(char):
                    literal.append(char)
                else:
                    break
            if normalized_words("".join(literal)) != normalized_words(pdf_word):
                entry["error"] = "precise inverse column does not select the PDF word"
            else:
                entry.update(ok=True, match="document_metadata" if metadata else "literal_frame_word" if bounds else "literal_source")
        else:
            expected_lines = source_info(expected_file)[1]
            eligible = expected_lines[bounds[0]:bounds[1] + 1] if bounds else expected_lines
            if any(line_has_word(row, pdf_word) for row in eligible):
                entry["error"] = "inverse refinement missed a literal printed word in its original source scope"
            else:
                entry.update(abstained=True, reason="precision_not_literal_math_or_expansion")
        yield entry


def painted_word_points(
    pdf: Path, page_count: int, page_limit: int | None, word_limit: int,
    rng: random.Random, specific_pages: list[int] | None,
) -> tuple[dict[int, list[WordPoint]], dict]:
    """Return every on-page word made from visible painted text characters."""
    if page_limit == 0 and specific_pages is None:
        return {}, {
            "eligible_pages": None,
            "pages_without_eligible_words": None,
            "selected_pages": [],
            "total": None,
            "selected": 0,
            "raw_extracted_words": None,
            "eligibility_filtered_words": None,
            "page_cap_unselected": None,
            "word_cap_unselected": 0,
            "eligible_words_by_selected_page": {},
            "selected_pages_without_eligible_words": [],
        }
    try:
        import pymupdf
    except ImportError as error:
        raise SystemExit("inverse word probes require PyMuPDF in the Python environment") from error
    selected = {}
    eligible_by_page = {}
    with pymupdf.open(pdf) as document:
        if document.page_count != page_count:
            raise SystemExit("PyMuPDF page count differs from pdfinfo")
        raw_extracted = 0
        for page_number, page in enumerate(document, 1):
            words = []
            painted = [
                (chr(codepoint), pymupdf.Rect(bounds))
                for span in page.get_texttrace()
                if span["type"] in (0, 1) and span["opacity"] > 0
                for codepoint, _, _, bounds in span["chars"]
                if 0 < codepoint <= 0x10FFFF
            ]
            text_words = page.get_text("words")
            raw_extracted += len(text_words)
            for x0, y0, x1, y1, word, *_ in text_words:
                rect = pymupdf.Rect(x0, y0, x1, y1)
                if rect.width <= 0 or rect.height <= 0:
                    continue
                if not page.rect.contains(rect.tl + (rect.br - rect.tl) / 2):
                    continue
                painted_word = "".join(
                    char for char, glyph in painted
                    if rect.contains(glyph.tl + (glyph.br - glyph.tl) / 2)
                )
                if normalized_words(word) and normalized_words(word) == normalized_words(painted_word):
                    words.append((word, tuple(rect), page.rect.height))
            if words:
                eligible_by_page[page_number] = words
    candidates = sorted(eligible_by_page)
    if specific_pages is not None:
        chosen = sorted(specific_pages)
    else:
        chosen = candidates if page_limit is None else sorted(rng.sample(candidates, min(page_limit, len(candidates))))
    for page in chosen:
        words = eligible_by_page.get(page, [])
        selected[page] = rng.sample(words, min(word_limit, len(words))) if word_limit else words
    eligible_total = sum(map(len, eligible_by_page.values()))
    coverage = {
        "eligible_pages": len(eligible_by_page),
        "pages_without_eligible_words": page_count - len(eligible_by_page),
        "selected_pages": chosen,
        "total": eligible_total,
        "raw_extracted_words": raw_extracted,
        "eligibility_filtered_words": raw_extracted - eligible_total,
        "selected": sum(map(len, selected.values())),
        "page_cap_unselected": sum(len(eligible_by_page[page]) for page in set(candidates) - set(chosen)),
        "word_cap_unselected": sum(len(eligible_by_page.get(page, [])) - len(selected[page]) for page in chosen),
        "eligible_words_by_selected_page": {page: len(eligible_by_page.get(page, [])) for page in chosen},
        "selected_pages_without_eligible_words": [page for page in chosen if not eligible_by_page.get(page)],
    }
    return selected, coverage


def pdf_page_count(pdf: Path) -> int:
    result = subprocess.run(
        ["pdfinfo", str(pdf)], capture_output=True, text=True, check=False
    )
    if result.returncode:
        raise SystemExit(f"pdfinfo: {result.stderr.strip()[:200]}")
    match = re.search(r"^Pages:\s+(\d+)$", result.stdout, re.MULTILINE)
    if not match:
        raise SystemExit("pdfinfo returned no page count")
    return int(match.group(1))


def pdfterm_bin() -> Path:
    if PDFTERM_BINARY.is_file():
        return PDFTERM_BINARY
    raise SystemExit(f"missing {PDFTERM_BINARY}; build pdfterm or pass --binary PATH")


def main() -> None:
    global PDFTERM_BINARY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pdf", type=Path)
    parser.add_argument("--binary", type=Path, default=PDFTERM_BINARY, help="pdfterm executable to probe")
    parser.add_argument(
        "--source",
        type=Path,
        help="source file to sample (default: main input recorded in SyncTeX)",
    )
    parser.add_argument("--page", type=int, action="append", help="probe this one-based page; repeatable")
    parser.add_argument("--lines", type=int, default=100, help="source lines to sample")
    parser.add_argument("--pages", type=int, help="seed-selected inverse page cap; default probes all pages")
    parser.add_argument("--words", type=int, default=0, help="max words per selected page; 0 means all")
    parser.add_argument("--seed", type=int, default=20260922)
    parser.add_argument("--allow-stale", action="store_true", help="accept an older PDF/SyncTeX pair")
    args = parser.parse_args()
    PDFTERM_BINARY = args.binary.resolve()
    if (
        min(args.lines, args.words) < 0
        or (args.pages is not None and args.pages < 0)
        or (args.page is not None and (args.pages is not None or min(args.page) < 1 or len(set(args.page)) != len(args.page)))
    ):
        parser.error("counts must be nonnegative; --page values must be unique and cannot combine with --pages")

    pdf = args.pdf.resolve()
    companion = next(
        (
            path
            for path in (pdf.with_suffix(".synctex.gz"), pdf.with_suffix(".synctex"))
            if path.exists()
        ),
        None,
    )
    if not pdf.is_file() or companion is None:
        raise SystemExit("missing PDF or matching SyncTeX sidecar")
    source_file = (args.source or read_source_map(pdf)["inputs"][1]).resolve()
    if not source_file.is_file():
        raise SystemExit(f"missing source {source_file}; pass --source PATH")
    if (
        not args.allow_stale
        and min(pdf.stat().st_mtime_ns, companion.stat().st_mtime_ns)
        < source_file.stat().st_mtime_ns
    ):
        raise SystemExit(
            "PDF/SyncTeX pair predates the TeX source; rebuild or pass --allow-stale"
        )
    source = source_file.read_text()

    rng = random.Random(args.seed)
    forward_positions = sample_source_positions(source, rng, args.lines)
    page_count = pdf_page_count(pdf)
    if args.page and max(args.page) > page_count:
        parser.error(f"--page must not exceed the PDF's {page_count} pages")
    word_points, inverse_coverage = painted_word_points(
        pdf, page_count, args.pages, args.words, rng, args.page
    )
    visible_words = pdf_visible_words(pdf) if forward_positions else []
    if visible_words and len(visible_words) != page_count:
        raise SystemExit("pdftotext page count differs from pdfinfo")

    results = list(forward(pdf, source_file, forward_positions, page_count, visible_words))
    batch_error = None
    try:
        results.extend(inverse(pdf, source_file, word_points))
    except (OSError, subprocess.TimeoutExpired, RuntimeError) as error:
        batch_error = str(error)
    failures = sum(not probe["ok"] and not probe.get("abstained") for probe in results) + bool(batch_error)
    inverse_results = [probe for probe in results if probe.get("direction") == "inverse"]
    reasons = {}
    for probe in inverse_results:
        if probe.get("abstained"):
            reason = probe["reason"]
            reasons[reason] = reasons.get(reason, 0) + 1
    if inverse_coverage["page_cap_unselected"]:
        reasons["page_cap_unselected"] = inverse_coverage["page_cap_unselected"]
    if inverse_coverage["word_cap_unselected"]:
        reasons["word_cap_unselected"] = inverse_coverage["word_cap_unselected"]
    abstained = sum(bool(probe.get("abstained")) for probe in inverse_results)
    inverse_coverage.update({
        "probed": len(inverse_results),
        "checked": len(inverse_results) - abstained,
        "abstained": abstained,
        "mapping_checked": sum(bool(probe.get("mapping_ok")) for probe in inverse_results),
        "reasons": reasons,
        "failed": sum(not probe["ok"] and not probe.get("abstained") for probe in inverse_results) + bool(batch_error),
        "document_metadata_matches": sum(probe.get("match") == "document_metadata" for probe in inverse_results),
    })
    print(json.dumps({
        "pdf": str(pdf),
        "source": str(source_file),
        "probes": len(results),
        "fails": failures,
        "stopped_early": bool(batch_error),
        "batch_error": batch_error,
        "inverse_coverage": inverse_coverage,
        "visible_checked": sum(bool(probe.get("visible_checked")) for probe in results),
        "results": results,
    }, indent=2))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
