from __future__ import annotations

from dataclasses import dataclass

from lxml import etree

from doctranslator.ir import Atom, FormatSpan, TextNodeRef
from doctranslator.sentinels import BR, NBH, SHY, TAB


W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
A_NS = "http://schemas.openxmlformats.org/drawingml/2006/main"

_W = f"{{{W_NS}}}"
_A = f"{{{A_NS}}}"


@dataclass(frozen=True)
class ScopeExtract:
    scope_key: str
    atoms: list[Atom]
    spans: list[FormatSpan]
    surface_text: str


def _w_style_sig_for_run(run: etree._Element | None) -> str:
    if run is None:
        return "w:rPr()"
    rpr = run.find(f"{_W}rPr")
    if rpr is None:
        return "w:rPr()"

    def bool_prop(tag: str) -> str:
        elem = rpr.find(f"{_W}{tag}")
        if elem is None:
            return "0"
        val = elem.get(f"{_W}val")
        if val is None:
            return "1"
        return "0" if val in {"0", "false", "off", "none"} else "1"

    def val_prop(tag: str) -> str:
        elem = rpr.find(f"{_W}{tag}")
        if elem is None:
            return ""
        return elem.get(f"{_W}val") or ""

    fonts = rpr.find(f"{_W}rFonts")
    fonts_sig = ""
    if fonts is not None:
        fonts_sig = "|".join(
            [
                fonts.get(f"{_W}ascii") or "",
                fonts.get(f"{_W}hAnsi") or "",
                fonts.get(f"{_W}eastAsia") or "",
                fonts.get(f"{_W}cs") or "",
            ]
        )

    return "|".join(
        [
            f"b={bool_prop('b')}",
            f"i={bool_prop('i')}",
            f"u={val_prop('u')}",
            f"strike={bool_prop('strike')}",
            f"color={val_prop('color')}",
            f"highlight={val_prop('highlight')}",
            f"sz={val_prop('sz')}",
            f"szCs={val_prop('szCs')}",
            f"rStyle={val_prop('rStyle')}",
            f"fonts={fonts_sig}",
        ]
    )


def _a_style_sig_for_run(run: etree._Element | None) -> str:
    if run is None:
        return "a:rPr()"
    rpr = run.find(f"{_A}rPr")
    if rpr is None:
        return "a:rPr()"
    parts: list[str] = []
    for attr in ("b", "i", "u", "strike", "sz"):
        val = rpr.get(attr)
        parts.append(f"{attr}={val or ''}")
    latin = rpr.find(f"{_A}latin")
    if latin is not None:
        parts.append(f"typeface={latin.get('typeface') or ''}")
    return "|".join(parts)


def _find_run_ancestor(node: etree._Element, qname: str) -> etree._Element | None:
    run = node.getparent()
    while run is not None and run.tag != qname:
        run = run.getparent()
    return run


def _build_spans(atoms: list[Atom]) -> list[FormatSpan]:
    spans: list[FormatSpan] = []
    current_style = None
    current_nodes: list[TextNodeRef] = []
    current_text_parts: list[str] = []

    def flush() -> None:
        nonlocal current_style, current_nodes, current_text_parts
        if current_nodes:
            spans.append(
                FormatSpan(
                    style_sig=current_style or "",
                    node_refs=list(current_nodes),
                    source_text="".join(current_text_parts),
                )
            )
        current_style = None
        current_nodes = []
        current_text_parts = []

    for atom in atoms:
        if atom.kind != "TEXT":
            flush()
            continue
        if current_style is None or atom.style_sig != current_style:
            flush()
            current_style = atom.style_sig
        if atom.node_ref is None:
            continue
        current_nodes.append(atom.node_ref)
        current_text_parts.append(atom.value)
    flush()
    return spans


def extract_scopes_from_xml(part_name: str, tree: etree._ElementTree) -> list[ScopeExtract]:
    root = tree.getroot()
    scopes: list[ScopeExtract] = []

    for p in root.findall(f".//{_W}p"):
        scope_key = f"{part_name}#w:p@{id(p)}"
        atoms: list[Atom] = []

        for node in p.iter():
            if node.tag == f"{_W}t":
                text = node.text or ""
                run = _find_run_ancestor(node, f"{_W}r")
                style_sig = _w_style_sig_for_run(run)
                node_ref = TextNodeRef(
                    part_name=part_name, kind="w:t", element=node, attr_name=None, original_text=text
                )
                atoms.append(Atom(kind="TEXT", node_ref=node_ref, value=text, style_sig=style_sig))
            elif node.tag == f"{_W}tab":
                atoms.append(Atom(kind="TAB", node_ref=None, value=TAB, style_sig=""))
            elif node.tag in {f"{_W}br", f"{_W}cr"}:
                atoms.append(Atom(kind="BR", node_ref=None, value=BR, style_sig=""))
            elif node.tag == f"{_W}noBreakHyphen":
                atoms.append(Atom(kind="NBH", node_ref=None, value=NBH, style_sig=""))
            elif node.tag == f"{_W}softHyphen":
                atoms.append(Atom(kind="SHY", node_ref=None, value=SHY, style_sig=""))

        if not any(atom.kind == "TEXT" and atom.value.strip() for atom in atoms):
            continue

        spans = _build_spans(atoms)
        surface_text = "".join(atom.value for atom in atoms)
        scopes.append(ScopeExtract(scope_key=scope_key, atoms=atoms, spans=spans, surface_text=surface_text))

    for p in root.findall(f".//{_A}p"):
        scope_key = f"{part_name}#a:p@{id(p)}"
        atoms: list[Atom] = []

        for node in p.iter():
            if node.tag == f"{_A}t":
                text = node.text or ""
                run = _find_run_ancestor(node, f"{_A}r")
                style_sig = _a_style_sig_for_run(run)
                node_ref = TextNodeRef(
                    part_name=part_name, kind="a:t", element=node, attr_name=None, original_text=text
                )
                atoms.append(Atom(kind="TEXT", node_ref=node_ref, value=text, style_sig=style_sig))
            elif node.tag == f"{_A}tab":
                atoms.append(Atom(kind="TAB", node_ref=None, value=TAB, style_sig=""))
            elif node.tag == f"{_A}br":
                atoms.append(Atom(kind="BR", node_ref=None, value=BR, style_sig=""))

        if not any(atom.kind == "TEXT" and atom.value.strip() for atom in atoms):
            continue

        spans = _build_spans(atoms)
        surface_text = "".join(atom.value for atom in atoms)
        scopes.append(ScopeExtract(scope_key=scope_key, atoms=atoms, spans=spans, surface_text=surface_text))

    for lvl_text in root.findall(f".//{_W}lvlText"):
        val_attr = lvl_text.get(f"{_W}val")
        if not val_attr or not val_attr.strip():
            continue
        scope_key = f"{part_name}#w:lvlText@{id(lvl_text)}"
        node_ref = TextNodeRef(
            part_name=part_name,
            kind="attr",
            element=lvl_text,
            attr_name=f"{_W}val",
            original_text=val_attr,
        )
        atom = Atom(kind="TEXT", node_ref=node_ref, value=val_attr, style_sig="attr")
        span = FormatSpan(style_sig="attr", node_refs=[node_ref], source_text=val_attr)
        scopes.append(ScopeExtract(scope_key=scope_key, atoms=[atom], spans=[span], surface_text=val_attr))

    return scopes
