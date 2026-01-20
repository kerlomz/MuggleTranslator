from __future__ import annotations

import math
import re
from dataclasses import dataclass


_SENT_SPLIT_RE = re.compile(r"(?<=[.!?;:。！？；：])\s+")
_WS_RE = re.compile(r"\s+")


def _normalize_for_embedding(text: str) -> str:
    if not text:
        return ""
    return _WS_RE.sub(" ", text.replace("\r", " ").replace("\n", " ")).strip()


def _split_for_embedding(text: str, *, max_chars: int) -> list[str]:
    norm = _normalize_for_embedding(text)
    if not norm:
        return []
    if max_chars <= 0 or len(norm) <= max_chars:
        return [norm]
    parts = [p.strip() for p in _SENT_SPLIT_RE.split(norm) if p and p.strip()]
    if not parts:
        return [norm[:max_chars]]
    out: list[str] = []
    cur = ""
    for p in parts:
        if not cur:
            cur = p
            continue
        if len(cur) + 1 + len(p) <= max_chars:
            cur = cur + " " + p
            continue
        out.append(cur)
        cur = p
    if cur:
        out.append(cur)
    if not out:
        return [norm[:max_chars]]
    return out


def vector_norm(vec: list[float]) -> float:
    if not vec:
        return 0.0
    return math.sqrt(sum((float(x) * float(x)) for x in vec))


def cosine_similarity(a: list[float], b: list[float], *, norm_a: float | None = None, norm_b: float | None = None) -> float:
    if not a or not b:
        return 0.0
    if len(a) != len(b):
        return 0.0
    na = vector_norm(a) if norm_a is None else float(norm_a)
    nb = vector_norm(b) if norm_b is None else float(norm_b)
    if na <= 0.0 or nb <= 0.0:
        return 0.0
    dot = 0.0
    for x, y in zip(a, b, strict=False):
        dot += float(x) * float(y)
    return float(dot / (na * nb))


def embed_text_with_chunking(
    embed_fn,
    text: str,
    *,
    max_chunk_chars: int,
) -> tuple[list[float], float]:
    chunks = _split_for_embedding(text, max_chars=max_chunk_chars)
    if not chunks:
        return ([], 0.0)
    vecs: list[list[float]] = []
    weights: list[float] = []
    for ch in chunks:
        try:
            v = embed_fn(ch)
        except Exception:  # noqa: BLE001
            continue
        if not v:
            continue
        vecs.append(v)
        weights.append(max(1.0, float(len(ch))))
    if not vecs:
        return ([], 0.0)
    dim = len(vecs[0])
    acc = [0.0] * dim
    total_w = sum(weights) or 1.0
    for v, w in zip(vecs, weights, strict=False):
        if len(v) != dim:
            continue
        for i, x in enumerate(v):
            acc[i] += float(x) * w
    acc = [x / total_w for x in acc]
    n = vector_norm(acc)
    return (acc, n)


@dataclass(frozen=True)
class EmbeddedExcerpt:
    tu_id: int
    part_name: str
    scope_type: str
    section_path: tuple[str, ...] | None
    text: str
    vec: list[float]
    norm: float


class EmbeddingIndex:
    def __init__(self) -> None:
        self.items: list[EmbeddedExcerpt] = []
        self.by_tu_id: dict[int, EmbeddedExcerpt] = {}

    def add(self, item: EmbeddedExcerpt) -> None:
        self.items.append(item)
        self.by_tu_id[item.tu_id] = item

    def query(
        self,
        query_vec: list[float],
        *,
        query_norm: float | None = None,
        top_k: int,
        exclude_ids: set[int] | None = None,
        prefer_section: tuple[str, ...] | None = None,
    ) -> list[tuple[EmbeddedExcerpt, float]]:
        if not query_vec or top_k <= 0:
            return []
        exclude = exclude_ids or set()
        scored: list[tuple[EmbeddedExcerpt, float]] = []
        for it in self.items:
            if it.tu_id in exclude:
                continue
            sim = cosine_similarity(query_vec, it.vec, norm_a=query_norm, norm_b=it.norm)
            if prefer_section is not None and it.section_path == prefer_section:
                sim += 0.02
            scored.append((it, sim))
        scored.sort(key=lambda kv: kv[1], reverse=True)
        return scored[:top_k]
