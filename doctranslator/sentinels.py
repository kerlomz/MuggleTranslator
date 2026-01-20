from __future__ import annotations

import re
from dataclasses import dataclass

# NOTE: This module is used by the verification scripts in `scripts/`.
# It must be stable ASCII/UTF-8 and match the Rust sentinels in `src/sentinels.rs`.

SEG_ID_WIDTH = 6
NT_ID_WIDTH = 4

TAB = "<<MT_TAB>>"
BR = "<<MT_BR>>"
NBH = "<<MT_NBH>>"
SHY = "<<MT_SHY>>"

CONTROL_TOKENS = (TAB, BR, NBH, SHY)


def nt_token(nt_id: int) -> str:
    return f"<<MT_NT:{nt_id:0{NT_ID_WIDTH}d}>>"


def seg_start(seg_id: int) -> str:
    return f"<<MT_SEG:{seg_id:0{SEG_ID_WIDTH}d}>>"


def seg_end(seg_id: int) -> str:
    return f"<<MT_END:{seg_id:0{SEG_ID_WIDTH}d}>>"


ANY_SENTINEL_RE = re.compile(
    r"<<MT_(?:TAB|BR|NBH|SHY|NT:\d{4}|SEG:\d{6}|END:\d{6})>>"
)
NT_RE = re.compile(r"<<MT_NT:(\d{4})>>")

_ALT_BRACKET_SENTINEL_RE = re.compile(
    r"(?:\[\[|\u3010|\u300a)\s*([A-Za-z]{2,4}(?::\d{1,6})?)\s*(?:\]\]|\u3011|\u300b)"
)


def _normalize_sentinel_content(raw: str) -> str | None:
    raw = raw.strip()
    if raw in {"TAB", "BR", "NBH", "SHY"}:
        return raw
    m = re.fullmatch(r"NT:(\d{1,4})", raw)
    if m:
        return f"NT:{int(m.group(1)):0{NT_ID_WIDTH}d}"
    m = re.fullmatch(r"(SEG|END):(\d{1,6})", raw)
    if m:
        return f"{m.group(1)}:{int(m.group(2)):0{SEG_ID_WIDTH}d}"
    return None


def decode_sentinels_from_model(text: str) -> str:
    if not text:
        return ""

    def repl_alt(m: re.Match[str]) -> str:
        norm = _normalize_sentinel_content(m.group(1))
        return f"<<MT_{norm}>>" if norm is not None else m.group(0)

    return _ALT_BRACKET_SENTINEL_RE.sub(repl_alt, text)


@dataclass(frozen=True)
class ParsedSegments:
    by_id: dict[int, str]


def _find_marker(text: str, marker_type: str, seg_id: int, cursor: int) -> tuple[int, int]:
    exact = f"<<MT_{marker_type}:{seg_id:0{SEG_ID_WIDTH}d}>>"
    idx = text.find(exact, cursor)
    if idx >= 0:
        return (idx, idx + len(exact))

    # Tolerant fallback: allow whitespace around id.
    pat = re.compile(rf"<<MT_{marker_type}:\s*0*{seg_id}\s*>>")
    m = pat.search(text, cursor)
    if not m:
        return (-1, -1)
    return (m.start(), m.end())


def parse_segmented_output(text: str, expected_ids: list[int]) -> ParsedSegments:
    segments: dict[int, str] = {}
    cursor = 0
    for seg_id in expected_ids:
        start_idx, start_end = _find_marker(text, "SEG", seg_id, cursor)
        if start_idx < 0:
            raise ValueError(f"Missing SEG start for id={seg_id}")
        end_idx, end_end = _find_marker(text, "END", seg_id, start_end)
        if end_idx < 0:
            raise ValueError(f"Missing SEG end for id={seg_id}")
        segments[seg_id] = text[start_end:end_idx]
        cursor = end_end
    if set(segments.keys()) != set(expected_ids):
        raise ValueError("SEG id mismatch")
    return ParsedSegments(by_id=segments)


def control_tokens_from_text(text: str) -> list[str]:
    if not text:
        return []
    pattern = re.compile("|".join(re.escape(tok) for tok in CONTROL_TOKENS))
    return [m.group(0) for m in pattern.finditer(text)]

