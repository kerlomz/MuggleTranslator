from __future__ import annotations

import re

from doctranslator.ir import TranslationUnit


_LEGAL_REF_ID_SPLIT_RE = re.compile(
    r"^(?P<num>\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?)(?P<suffix>(?:\([A-Za-z0-9]+\))*)$"
)


# NOTE: Keep these patterns aligned with doctranslator/freezer.py so frozen legal references
# can be deterministically mapped between en<->zh without model involvement.
_EN_LEGAL_REF_RE = re.compile(
    r"\b(?P<kind>Section|Article|Clause|Paragraph|Schedule)s?\s+"
    r"(?P<id>\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?(?:\([A-Za-z0-9]+\))*|[IVXLCDM]{1,8})\b",
    flags=re.IGNORECASE,
)
_EN_LEGAL_REF_ABBR_RE = re.compile(
    r"\b(?P<kind>Sec|Art|Cl|Para|Sch)s?\.\s+"
    r"(?P<id>\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?(?:\([A-Za-z0-9]+\))*|[IVXLCDM]{1,8})\b",
    flags=re.IGNORECASE,
)
_ZH_LEGAL_REF_RE = re.compile(r"第\s*(?P<id>\d+(?:\([A-Za-z0-9]+\))*)\s*(?P<kind>条|款|节|段|章|篇)(?P<post>(?:\([A-Za-z0-9]+\))*)")
_ZH_SCHEDULE_REF_RE = re.compile(r"附表\s*(?P<id>\d+(?:\([A-Za-z0-9]+\))*)")


def _map_legal_reference_text(original: str, source_lang: str, target_lang: str) -> str | None:
    if not original:
        return None

    src = (source_lang or "").lower()
    tgt = (target_lang or "").lower()

    def zh_ref(label: str, ref_id: str) -> str:
        m2 = _LEGAL_REF_ID_SPLIT_RE.match(ref_id or "")
        if m2:
            num = (m2.group("num") or "").strip()
            suffix = (m2.group("suffix") or "").strip()
            if suffix:
                return f"第{num}{label}{suffix}"
        return f"第{ref_id}{label}"

    if src.startswith("en") and tgt.startswith("zh"):
        m = _EN_LEGAL_REF_RE.search(original) or _EN_LEGAL_REF_ABBR_RE.search(original)
        if not m:
            return None
        kind = (m.group("kind") or "").lower().strip(".")
        kind = kind.rstrip("s")
        ref_id = (m.group("id") or "").strip()
        if not ref_id:
            return None
        if kind in {"section", "article", "sec", "art"}:
            return zh_ref("条", ref_id)
        if kind in {"clause", "cl"}:
            return zh_ref("款", ref_id)
        if kind in {"paragraph", "para"}:
            return zh_ref("段", ref_id)
        if kind in {"schedule", "sch"}:
            return f"附表{ref_id}"
        return None

    if src.startswith("zh") and tgt.startswith("en"):
        m = _ZH_LEGAL_REF_RE.search(original)
        if m:
            ref_id = ((m.group("id") or "") + (m.group("post") or "")).strip()
            kind = (m.group("kind") or "").strip()
            if not ref_id or not kind:
                return None
            if kind in {"条", "节", "章", "篇"}:
                return f"Section {ref_id}"
            if kind == "款":
                return f"Clause {ref_id}"
            if kind == "段":
                return f"Paragraph {ref_id}"
            return None
        m2 = _ZH_SCHEDULE_REF_RE.search(original)
        if m2:
            ref_id = (m2.group("id") or "").strip()
            if not ref_id:
                return None
            return f"Schedule {ref_id}"
        return None

    return None


def _rewrite_nt_maps_for_target_lang(tus: list[TranslationUnit], source_lang: str, target_lang: str) -> int:
    changed = 0
    for tu in tus:
        if not tu.nt_map:
            continue
        for tok, original in list(tu.nt_map.items()):
            mapped = _map_legal_reference_text(original, source_lang=source_lang, target_lang=target_lang)
            if mapped and mapped != original:
                tu.nt_map[tok] = mapped
                changed += 1
    return changed
