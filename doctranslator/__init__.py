from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pathlib import Path


def translate_docx(docx_path: str | "Path"):
    # Lazy import to avoid circular imports when running `doctranslator/api.py` as a script.
    from doctranslator.api import translate_docx as _translate_docx

    return _translate_docx(docx_path)

__all__ = ["translate_docx"]
