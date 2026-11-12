#!/usr/bin/env python3
"""Wrapper around synctex-parser's canonical synthetic LaTeX SyncTeX fuzzer."""

from pathlib import Path
import runpy

_ROOT = Path(__file__).resolve().parent.parent
globals().update(
    runpy.run_path(
        str(_ROOT.parent / "synctex-parser" / "scripts" / "fuzz_synthetic.py"),
        init_globals={"PDFTERM_DEFAULT_BINARY": _ROOT / "target" / "release" / "pdfterm"},
        run_name=__name__,
    )
)
