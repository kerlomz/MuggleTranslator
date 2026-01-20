from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal

from lxml import etree


TextNodeKind = Literal["w:t", "a:t", "attr"]


@dataclass(frozen=True)
class TextNodeRef:
    part_name: str
    kind: TextNodeKind
    element: etree._Element
    attr_name: str | None
    original_text: str


@dataclass(frozen=True)
class Atom:
    kind: Literal["TEXT", "TAB", "BR", "NBH", "SHY"]
    node_ref: TextNodeRef | None
    value: str
    style_sig: str


@dataclass(frozen=True)
class FormatSpan:
    style_sig: str
    node_refs: list[TextNodeRef]
    source_text: str


@dataclass
class TranslationUnit:
    tu_id: int
    part_name: str
    scope_key: str
    atoms: list[Atom]
    spans: list[FormatSpan]
    source_surface: str
    frozen_surface: str
    nt_map: dict[str, str] = field(default_factory=dict)
    draft_translation: str | None = None
    final_translation: str | None = None
    alt_translation: str | None = None
    draft_translation_model: str | None = None
    alt_translation_model: str | None = None
    force_ape: bool = False
    needs_ws_audit: bool = False
    ws_flags: list[str] = field(default_factory=list)
    qe_score: int | None = None
    qe_flags: list[str] = field(default_factory=list)
