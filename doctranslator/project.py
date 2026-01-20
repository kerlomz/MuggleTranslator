from __future__ import annotations

import re
from dataclasses import dataclass

from doctranslator.freezer import unfreeze_text
from doctranslator.ir import FormatSpan, TextNodeRef
from doctranslator.sentinels import CONTROL_TOKENS, control_tokens_from_text


_TOKEN_RE = re.compile(r"⟦[^⟧]+⟧")


@dataclass(frozen=True)
class SpanSlice:
    span: FormatSpan
    text: str


def _split_by_control_sequence(text: str) -> list[str]:
    if not text:
        return [""]
    pattern = re.compile("|".join(re.escape(tok) for tok in CONTROL_TOKENS))
    parts: list[str] = []
    pos = 0
    for m in pattern.finditer(text):
        parts.append(text[pos : m.start()])
        parts.append(m.group(0))
        pos = m.end()
    parts.append(text[pos:])
    return parts


def _unitize(text: str) -> list[str]:
    units: list[str] = []
    pos = 0
    for m in _TOKEN_RE.finditer(text):
        if m.start() > pos:
            units.extend(list(text[pos : m.start()]))
        units.append(m.group(0))
        pos = m.end()
    if pos < len(text):
        units.extend(list(text[pos:]))
    return units


def _count_plain_units(units: list[str]) -> int:
    return sum(1 for u in units if _TOKEN_RE.fullmatch(u) is None)


def _allocate_plain_counts(total_plain: int, weights: list[int]) -> list[int]:
    if not weights:
        return []
    if total_plain <= 0:
        return [0 for _ in weights]

    total_w = sum(weights)
    if total_w <= 0:
        base = total_plain // len(weights)
        out = [base for _ in weights]
        out[-1] += total_plain - sum(out)
        return out

    raw = [total_plain * w / total_w for w in weights]
    floored = [int(x) for x in raw]
    remain = total_plain - sum(floored)
    frac = sorted([(raw[i] - floored[i], i) for i in range(len(weights))], reverse=True)
    for k in range(remain):
        _, idx = frac[k % len(frac)]
        floored[idx] += 1
    return floored


def project_translation_to_spans(
    spans: list[FormatSpan], source_surface: str, target_surface: str, nt_map: dict[str, str]
) -> list[SpanSlice]:
    if control_tokens_from_text(source_surface) != control_tokens_from_text(target_surface):
        raise ValueError("Control token sequence mismatch")

    source_parts = _split_by_control_sequence(source_surface)
    target_parts = _split_by_control_sequence(target_surface)
    if len(source_parts) != len(target_parts):
        raise ValueError("Control token part count mismatch")

    span_slices: list[SpanSlice] = []
    span_idx = 0

    for src_part, tgt_part in zip(source_parts, target_parts, strict=True):
        if src_part in CONTROL_TOKENS:
            if tgt_part != src_part:
                raise ValueError("Control token mismatch")
            continue

        block_spans: list[FormatSpan] = []
        block_src_len = 0
        while span_idx < len(spans) and block_src_len < len(src_part):
            span = spans[span_idx]
            block_spans.append(span)
            block_src_len += len(span.source_text)
            span_idx += 1

        if not block_spans:
            if tgt_part.strip():
                raise ValueError("Translated text exists for empty source block")
            continue

        tgt_units = _unitize(tgt_part)
        total_plain = _count_plain_units(tgt_units)
        weights = [max(len(span.source_text), 1) for span in block_spans]
        desired = _allocate_plain_counts(total_plain, weights)

        slices_units: list[list[str]] = [[] for _ in block_spans]
        current_span = 0
        current_plain = 0

        for unit in tgt_units:
            slices_units[current_span].append(unit)
            if _TOKEN_RE.fullmatch(unit) is None:
                current_plain += 1
            if current_span < len(block_spans) - 1 and current_plain >= desired[current_span]:
                current_span += 1
                current_plain = 0

        for span, units in zip(block_spans, slices_units, strict=True):
            span_text_frozen = "".join(units)
            span_text = unfreeze_text(span_text_frozen, nt_map)
            span_slices.append(SpanSlice(span=span, text=span_text))

    if span_idx != len(spans):
        remaining = spans[span_idx:]
        if any(s.source_text.strip() for s in remaining):
            raise ValueError("Span coverage mismatch")
        for span in remaining:
            span_slices.append(SpanSlice(span=span, text=""))

    return span_slices


def distribute_span_text_to_nodes(span: FormatSpan, text: str) -> list[tuple[TextNodeRef, str]]:
    if not span.node_refs:
        return []
    if len(span.node_refs) == 1:
        return [(span.node_refs[0], text)]

    weights = [max(len(n.original_text), 1) for n in span.node_refs]
    units = list(text)
    total = len(units)
    desired = _allocate_plain_counts(total, weights)
    out: list[tuple[TextNodeRef, str]] = []
    idx = 0
    for node_ref, count in zip(span.node_refs, desired, strict=True):
        piece = "".join(units[idx : idx + count])
        idx += count
        out.append((node_ref, piece))
    if idx < len(units):
        last_ref, last_text = out[-1]
        out[-1] = (last_ref, last_text + "".join(units[idx:]))
    return out

