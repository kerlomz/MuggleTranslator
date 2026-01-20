from __future__ import annotations

import re
from collections import Counter
from typing import Callable


_STRONG_SENT_BOUNDARY_RE = re.compile(r"(?<=[.!?;:。！？；：])\s+")
_WEAK_SENT_BOUNDARY_RE = re.compile(r"(?<=[,，])\s+")


def _split_text_to_fit_tokens(text: str, *, count_tokens: Callable[[str], int], max_tokens: int) -> list[str]:
    if not text:
        return [""]
    if max_tokens <= 0:
        return [text]

    try:
        total = count_tokens(text)
    except Exception:  # noqa: BLE001
        total = 0
    if total and total <= max_tokens:
        return [text]

    # Collect candidate cut positions (greedy). We preserve original whitespace by cutting at match.end().
    positions = [m.end() for m in _STRONG_SENT_BOUNDARY_RE.finditer(text)]
    if not positions:
        positions = [m.end() for m in _WEAK_SENT_BOUNDARY_RE.finditer(text)]

    def hard_split(start: int) -> int:
        # Approximate: 1 token ~= 3 chars (English) or ~= 1 char (CJK). Use a conservative cap.
        cap = max(64, int(max_tokens * 3))
        end = min(len(text), start + cap)
        if end <= start:
            return min(len(text), start + 1)
        while end > start + 32:
            try:
                if count_tokens(text[start:end]) <= max_tokens:
                    break
            except Exception:  # noqa: BLE001
                break
            end -= 16
        return max(end, start + 1)

    out: list[str] = []
    start = 0
    last_good = start
    for pos in positions:
        if pos <= start:
            continue
        try:
            ok = count_tokens(text[start:pos]) <= max_tokens
        except Exception:  # noqa: BLE001
            ok = True
        if ok:
            last_good = pos
            continue
        if last_good > start:
            out.append(text[start:last_good])
            start = last_good
            last_good = start
            continue
        end = hard_split(start)
        out.append(text[start:end])
        start = end
        last_good = start

    if start < len(text):
        tail = text[start:]
        try:
            if count_tokens(tail) <= max_tokens:
                out.append(tail)
            else:
                while start < len(text):
                    end = hard_split(start)
                    out.append(text[start:end])
                    start = end
        except Exception:  # noqa: BLE001
            out.append(tail)

    return [seg for seg in out if seg is not None and seg != ""]


def _detect_stitch_duplicate_chunks(src_chunks: list[str], out_chunks: list[str]) -> bool:
    if not src_chunks or not out_chunks or len(src_chunks) != len(out_chunks):
        return False

    def norm(s: str) -> str:
        return re.sub(r"\s+", " ", (s or "")).strip()

    src_norm = [norm(s) for s in src_chunks]
    out_norm = [norm(s) for s in out_chunks]

    src_counts: Counter[str] = Counter([s for s in src_norm if len(s) >= 24])
    out_counts: Counter[str] = Counter([s for s in out_norm if len(s) >= 24])

    for out_val, out_cnt in out_counts.items():
        if out_cnt < 2:
            continue
        idxs = [i for i, v in enumerate(out_norm) if v == out_val]
        for i in idxs:
            if src_counts.get(src_norm[i], 0) <= 1:
                return True
    return False

