from __future__ import annotations

import re
from dataclasses import dataclass

from lxml import etree

from doctranslator.ir import Atom, TranslationUnit
from doctranslator.sentinels import ANY_SENTINEL_RE


_W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
_W = f"{{{_W_NS}}}"

_HEADING_NAME_RE = re.compile(r"^(?:Heading|标题)\s*(\d+)$", flags=re.IGNORECASE)
_HEADING_ID_RE = re.compile(r"(?:Heading|标题)(\d+)", flags=re.IGNORECASE)


@dataclass(frozen=True)
class StyleInfo:
    style_id: str
    name: str | None
    outline_level: int | None
    based_on: str | None


@dataclass(frozen=True)
class ParagraphContext:
    part_name: str
    scope_key: str
    section_path: tuple[str, ...] = ()
    is_heading: bool = False
    heading_level: int | None = None
    style_id: str | None = None
    style_name: str | None = None
    outline_level: int | None = None
    num_id: str | None = None
    list_level: int | None = None
    in_table: bool = False

    def format_for_prompt(self) -> str:
        lines: list[str] = []
        if self.section_path:
            lines.append("Section path: " + " > ".join(self.section_path))
        if self.is_heading and self.heading_level is not None:
            lines.append(f"Paragraph role: Heading(level={self.heading_level})")
        if self.style_name or self.style_id:
            lines.append(f"Paragraph style: {(self.style_name or self.style_id or '').strip()}")
        if self.outline_level is not None:
            lines.append(f"Outline level: {self.outline_level}")
        if self.num_id is not None or self.list_level is not None:
            lines.append(
                f"List: numId={self.num_id or ''} ilvl={self.list_level if self.list_level is not None else ''}"
            )
        if self.in_table:
            lines.append("In table: yes")
        return "\n".join(lines)


def _clean_text(text: str, max_chars: int = 120) -> str:
    t = ANY_SENTINEL_RE.sub(" ", text or "")
    t = re.sub(r"\s+", " ", t).strip()
    if max_chars > 0 and len(t) > max_chars:
        return t[: max_chars - 3] + "..."
    return t


def _first_text_node_atom(atoms: list[Atom]) -> Atom | None:
    for atom in atoms:
        if atom.kind == "TEXT" and atom.node_ref is not None:
            return atom
    return None


def _find_w_paragraph(tu: TranslationUnit) -> etree._Element | None:
    if "#w:p@" not in tu.scope_key:
        return None
    first = _first_text_node_atom(tu.atoms)
    if first is None or first.node_ref is None:
        return None
    node = first.node_ref.element
    while node is not None and node.tag != f"{_W}p":
        node = node.getparent()
    return node


def _parse_styles(styles_tree: etree._ElementTree | None) -> dict[str, StyleInfo]:
    if styles_tree is None:
        return {}
    root = styles_tree.getroot()
    out: dict[str, StyleInfo] = {}
    for style in root.findall(f".//{_W}style"):
        sid = style.get(f"{_W}styleId") or ""
        if not sid:
            continue
        name_elem = style.find(f"{_W}name")
        name = name_elem.get(f"{_W}val") if name_elem is not None else None
        based_on_elem = style.find(f"{_W}basedOn")
        based_on = based_on_elem.get(f"{_W}val") if based_on_elem is not None else None
        outline = None
        outline_elem = style.find(f".//{_W}pPr/{_W}outlineLvl")
        if outline_elem is not None:
            try:
                outline = int(outline_elem.get(f"{_W}val") or "")
            except Exception:  # noqa: BLE001
                outline = None
        out[sid] = StyleInfo(style_id=sid, name=name, outline_level=outline, based_on=based_on)
    return out


def _resolve_style_outline(style_id: str | None, styles: dict[str, StyleInfo]) -> int | None:
    seen: set[str] = set()
    cur = style_id
    while cur and cur not in seen:
        seen.add(cur)
        info = styles.get(cur)
        if info is None:
            break
        if info.outline_level is not None:
            return info.outline_level
        cur = info.based_on
    return None


def _guess_heading_level(style_id: str | None, style_name: str | None, outline: int | None) -> int | None:
    if outline is not None:
        if outline >= 0:
            return outline + 1
        return None
    for cand in (style_name or "", style_id or ""):
        m = _HEADING_NAME_RE.match(cand.strip())
        if m:
            try:
                return int(m.group(1))
            except Exception:  # noqa: BLE001
                pass
        m = _HEADING_ID_RE.search(cand)
        if m:
            try:
                return int(m.group(1))
            except Exception:  # noqa: BLE001
                pass
    return None


def build_paragraph_contexts(
    tus: list[TranslationUnit], *, styles_tree: etree._ElementTree | None
) -> dict[int, ParagraphContext]:
    styles = _parse_styles(styles_tree)
    by_part: dict[str, list[TranslationUnit]] = {}
    for tu in tus:
        if "#w:p@" not in tu.scope_key:
            continue
        by_part.setdefault(tu.part_name, []).append(tu)

    result: dict[int, ParagraphContext] = {}

    for part_name, part_tus in by_part.items():
        heading_stack: list[tuple[int, str]] = []

        for tu in part_tus:
            p = _find_w_paragraph(tu)
            if p is None:
                continue

            ppr = p.find(f"{_W}pPr")
            style_id = None
            outline = None
            num_id = None
            ilvl = None
            in_table = False

            node = p.getparent()
            while node is not None:
                if node.tag == f"{_W}tc":
                    in_table = True
                    break
                node = node.getparent()

            if ppr is not None:
                pstyle = ppr.find(f"{_W}pStyle")
                if pstyle is not None:
                    style_id = pstyle.get(f"{_W}val")
                outline_elem = ppr.find(f"{_W}outlineLvl")
                if outline_elem is not None:
                    try:
                        outline = int(outline_elem.get(f"{_W}val") or "")
                    except Exception:  # noqa: BLE001
                        outline = None
                numpr = ppr.find(f"{_W}numPr")
                if numpr is not None:
                    num_id_elem = numpr.find(f"{_W}numId")
                    if num_id_elem is not None:
                        num_id = num_id_elem.get(f"{_W}val")
                    ilvl_elem = numpr.find(f"{_W}ilvl")
                    if ilvl_elem is not None:
                        try:
                            ilvl = int(ilvl_elem.get(f"{_W}val") or "")
                        except Exception:  # noqa: BLE001
                            ilvl = None

            style_info = styles.get(style_id or "")
            style_name = style_info.name if style_info is not None else None
            if outline is None:
                outline = _resolve_style_outline(style_id, styles)

            heading_level = _guess_heading_level(style_id, style_name, outline)
            is_heading = heading_level is not None

            if is_heading:
                heading_text = _clean_text(tu.source_surface, max_chars=100)
                while heading_stack and heading_stack[-1][0] >= heading_level:
                    heading_stack.pop()
                if heading_text:
                    heading_stack.append((heading_level, heading_text))

            section_path = tuple(text for _lvl, text in heading_stack if text)

            result[tu.tu_id] = ParagraphContext(
                part_name=tu.part_name,
                scope_key=tu.scope_key,
                section_path=section_path,
                is_heading=is_heading,
                heading_level=heading_level,
                style_id=style_id,
                style_name=style_name,
                outline_level=outline,
                num_id=num_id,
                list_level=ilvl,
                in_table=in_table,
            )

    return result

