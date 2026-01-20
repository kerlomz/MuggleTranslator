from __future__ import annotations

import re
from collections import Counter

from doctranslator.freezer import unfreeze_text
from doctranslator.ir import TranslationUnit
from doctranslator.sentinels import ANY_SENTINEL_RE
from doctranslator.textutil import (
    _CJK_RE,
    _LATIN_RE,
    _LATIN_EXT_RE,
    _looks_like_english,
    _looks_like_entity_name,
    _text_for_lang,
)


_ZERO_WIDTH_RE = re.compile(r"[\u200b\u200c\u200d\u2060]")
_REPEAT_CHAR_RE = re.compile(r"(.)\1{12,}")
_LATIN_PHRASE_RE = re.compile(r"[A-Za-z][A-Za-z0-9 ,.;:'\"()/\\-]{30,}")
_LATIN_PHRASE_SHORT_RE = re.compile(r"[A-Za-z][A-Za-z0-9 ,.;:'\"()/\\-]{12,}")

_UNEXPECTED_SCRIPT_CHAR_RE = re.compile(
    r"[\u0900-\u097F\u0980-\u09FF\u0600-\u06FF\u0400-\u04FF\u0370-\u03FF\u0590-\u05FF\u0E00-\u0E7F\uAC00-\uD7AF\u3040-\u309F\u30A0-\u30FF]"
)

_PROMPT_TAG_RE = re.compile(
    r"\[(?:/?(?:CONTEXT|TEXT|TARGET|SRC|DRAFT|DOC_CONTEXT|CURRENT_PROBLEMS|STRUCTURE|EXCERPTS|TERMS|"
    r"NEIGHBOR_SRC_PREV|NEIGHBOR_SRC_NEXT|BAD_OUTPUT|BAD_OUTPUT_SEG|SRC_SEG))\]",
    flags=re.IGNORECASE,
)
_PROMPT_KV_RE = re.compile(
    r"\b(?:Domain|Document\s+type|Document\s+summary|Target\s+writing\s+style|Style\s+guide|Glossary|"
    r"Context\s*\(|Relevant\s+excerpts|Text\s+to\s+translate|Source\s+text|Draft\s+translation|Bad\s+output|"
    r"Structure\s+hints|Previous\s+source\s+paragraph|Next\s+source\s+paragraph)\b\s*:",
    flags=re.IGNORECASE,
)

_ZH_BAD_REF_PLACEHOLDER_RE = re.compile(r"第\s*(?P<id>X|x|\?|\*|[IVXLCDM]{1,8})\s*(?P<kind>条|款|节|段|章|篇)")
_ZH_BAD_REF_MISSING_ID_RE = re.compile(r"第\s*(条|款|节|段|章|篇)")

_EN_COND_RE = re.compile(
    r"\b(if|unless|provided\s+that|in\s+the\s+event|to\s+the\s+extent|subject\s+to|"
    r"if\s+applicable|if\s+specified)\b",
    flags=re.IGNORECASE,
)
_ZH_COND_INJECT_RE = re.compile(r"(如果|若|如)\s*适用")


HARD_QUALITY_ISSUES: set[str] = {
    "protocol_error",
    "empty_output",
    "prompt_artifact",
    "unexpected_script",
    "zero_width_chars",
    "repeated_char_run",
    "repeated_sentence",
    "bad_reference_placeholder",
    "variable_marker_missing",
    "too_short",
    "coverage_low",
    "over_expansion",
    "unjustified_condition",
    "it_default_sense",
    "looks_untranslated",
    "english_skeleton",
    "mixed_language",
    "untranslated_english",
    "source_echo",
    "duplicate_paragraph",
    "stitch_duplicate_chunk",
}


def hard_issues(issues: list[str]) -> list[str]:
    hard: list[str] = []
    for it in issues or []:
        s = str(it)
        if s in HARD_QUALITY_ISSUES or s.startswith("glossary_leakage:"):
            hard.append(s)
    return hard


def quality_issues(
    tu: TranslationUnit,
    translated: str,
    *,
    source_lang: str,
    target_lang: str,
    glossary_dict: dict[str, str] | None = None,
) -> list[str]:
    if not translated:
        return ["empty_output"]

    issues: list[str] = []

    tgt_unfrozen = unfreeze_text(translated, tu.nt_map)
    plain_out = ANY_SENTINEL_RE.sub(" ", tgt_unfrozen)
    plain_src = ANY_SENTINEL_RE.sub(" ", tu.source_surface)

    if _ZERO_WIDTH_RE.search(plain_out):
        issues.append("zero_width_chars")

    if _REPEAT_CHAR_RE.search(plain_out):
        issues.append("repeated_char_run")

    if (_PROMPT_TAG_RE.search(plain_out) or _PROMPT_KV_RE.search(plain_out)) and (
        _PROMPT_TAG_RE.search(plain_src) is None and _PROMPT_KV_RE.search(plain_src) is None
    ):
        issues.append("prompt_artifact")

    tgt = (target_lang or "").lower()
    src = (source_lang or "").lower()

    if tgt.startswith("zh"):
        allowed_scripts = set(_UNEXPECTED_SCRIPT_CHAR_RE.findall(plain_src))
        found_scripts = set(_UNEXPECTED_SCRIPT_CHAR_RE.findall(plain_out))
        if found_scripts - allowed_scripts:
            issues.append("unexpected_script")

        if "默认" in plain_out and re.search(r"\bDefault\b", plain_src, flags=re.IGNORECASE):
            if re.search(
                r"\bby\s+default\b|\bdefault\s+settings?\b|\bdefault\s+value\b|\bdefault\s+configuration\b",
                plain_src,
                flags=re.IGNORECASE,
            ) is None:
                issues.append("it_default_sense")

        if _ZH_BAD_REF_MISSING_ID_RE.search(plain_out) or _ZH_BAD_REF_PLACEHOLDER_RE.search(plain_out):
            issues.append("bad_reference_placeholder")

        # Variable/party marker preservation (X/Y/Z) after unfreezing.
        for ch in ("X", "Y", "Z"):
            if re.search(rf"\b{ch}\b", plain_src) and re.search(rf"\b{ch}\b", plain_out) is None:
                issues.append("variable_marker_missing")
                break

        src_latin = len(_LATIN_RE.findall(plain_src))
        src_cjk = len(_CJK_RE.findall(plain_src))
        out_latin = len(_LATIN_RE.findall(plain_out))
        out_cjk = len(_CJK_RE.findall(plain_out))
        out_latin_ext = len(_LATIN_EXT_RE.findall(plain_out))

        if src.startswith("en") and src_latin >= 12:
            if out_cjk <= max(2, int(src_latin * 0.08)) and out_latin >= max(8, int(src_latin * 0.35)):
                issues.append("looks_untranslated")

            if src_latin >= 120:
                if out_cjk <= max(18, int(src_latin * 0.25)):
                    issues.append("too_short")
                    if out_cjk <= max(14, int(src_latin * 0.18)):
                        issues.append("coverage_low")

        out_low = re.sub(r"\s+", " ", plain_out).strip().lower()
        src_low = re.sub(r"\s+", " ", plain_src).strip().lower()

        # Detect untranslated English skeletons / mixed language.
        english_like = sum(1 for w in re.findall(r"[A-Za-z]{2,}", plain_out) if w.lower() in {"the", "and", "of", "to"})
        if out_latin >= 18 and out_cjk >= 6:
            issues.append("mixed_language")
        if out_latin >= 24 and (english_like >= 1 or _looks_like_english(plain_out)):
            issues.append("untranslated_english")

        for phrase in _LATIN_PHRASE_RE.findall(plain_out):
            ph = re.sub(r"\s+", " ", phrase).strip()
            if len(ph) < 30:
                continue
            if _looks_like_entity_name(ph):
                continue
            issues.append("english_skeleton")
            break

        # Detect source echo (copy-paste) excluding entity names.
        for phrase in _LATIN_PHRASE_SHORT_RE.findall(plain_out):
            ph = re.sub(r"\s+", " ", phrase).strip().lower()
            if len(ph) < 12:
                continue
            if ph not in src_low or ph not in out_low:
                continue
            if _looks_like_entity_name(phrase):
                continue
            issues.append("source_echo")
            break

        # Over expansion: too long compared to source.
        src_len = len(re.sub(r"\s+", " ", plain_src).strip())
        out_len = len(re.sub(r"\s+", " ", plain_out).strip())
        if src_len >= 40 and out_len >= int(src_len * 2.8):
            issues.append("over_expansion")

        # Condition injection heuristic (especially the “如果适用…” virus).
        if _ZH_COND_INJECT_RE.search(plain_out) and _EN_COND_RE.search(plain_src) is None:
            issues.append("unjustified_condition")

        if src_len >= 80:
            sent = re.split(r"[。！？；：.!?;:]", plain_out)
            sent_norm = [re.sub(r"\s+", " ", s).strip() for s in sent if s and s.strip()]
            freq: dict[str, int] = {}
            for s in sent_norm:
                if len(s) < 12:
                    continue
                freq[s] = freq.get(s, 0) + 1
            if any(v >= 2 for v in freq.values()):
                issues.append("repeated_sentence")

    elif tgt.startswith("en"):
        allowed_scripts = set(_UNEXPECTED_SCRIPT_CHAR_RE.findall(plain_src))
        found_scripts = set(_UNEXPECTED_SCRIPT_CHAR_RE.findall(plain_out))
        if found_scripts - allowed_scripts:
            issues.append("unexpected_script")

        src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
        src_cjk = len(_CJK_RE.findall(src_plain))
        out_words = len(re.findall(r"[A-Za-z]{2,}", plain_out))

        if src.startswith("zh") and src_cjk > 0:
            if out_words == 0 and src_cjk >= 12 and len(plain_out.strip()) >= 6:
                issues.append("looks_untranslated")

            if src_cjk >= 60 and out_words <= max(10, int(src_cjk * 0.25)):
                issues.append("too_short")
                if src_cjk >= 120 and out_words <= max(16, int(src_cjk * 0.18)):
                    issues.append("coverage_low")

        src_low = re.sub(r"\s+", " ", plain_src).strip().lower()
        out_low = re.sub(r"\s+", " ", plain_out).strip().lower()

        for phrase in _LATIN_PHRASE_SHORT_RE.findall(plain_out):
            ph = re.sub(r"\s+", " ", phrase).strip().lower()
            if len(ph) < 12:
                continue
            if ph not in src_low or ph not in out_low:
                continue
            if _looks_like_entity_name(phrase):
                continue
            issues.append("source_echo")
            break

        src_len = len(re.sub(r"\s+", " ", plain_src).strip())
        if src_len >= 80:
            sent = re.split(r"[。！？；：.!?;:]", plain_out)
            sent_norm = [re.sub(r"\s+", " ", s).strip() for s in sent if s and s.strip()]
            freq: dict[str, int] = {}
            for s in sent_norm:
                if len(s) < 12:
                    continue
                freq[s] = freq.get(s, 0) + 1
            if any(v >= 2 for v in freq.values()):
                issues.append("repeated_sentence")

    if glossary_dict:
        src_plain = _text_for_lang(tu.source_surface)
        out_plain = _text_for_lang(tgt_unfrozen)
        src_low = src_plain.lower()

        if src.startswith("en") and tgt.startswith("zh"):
            for src_term, dst_term in glossary_dict.items():
                st = str(src_term).strip()
                dt = str(dst_term).strip()
                if not st or not dt:
                    continue
                if not _LATIN_RE.search(st) or not _CJK_RE.search(dt):
                    continue
                words = [w.lower() for w in re.findall(r"[A-Za-z]{4,}", st)]
                if len(words) < 2:
                    continue
                if len(dt) < 6:
                    continue
                if dt not in out_plain:
                    continue
                if re.search(re.escape(st), src_plain, flags=re.IGNORECASE) is not None:
                    continue
                if any(re.search(rf"\b{re.escape(w)}\b", src_low, flags=re.IGNORECASE) for w in words):
                    continue
                issues.append(f"glossary_leakage:{st[:32]}")
                break

        if src.startswith("zh") and tgt.startswith("en"):
            for src_term, dst_term in glossary_dict.items():
                st = str(src_term).strip()
                dt = str(dst_term).strip()
                if not st or not dt:
                    continue
                if _LATIN_RE.search(st) or not _LATIN_RE.search(dt):
                    continue
                if len(st) < 4 or len(dt) < 8:
                    continue
                if dt not in out_plain:
                    continue
                if st in src_plain:
                    continue
                issues.append(f"glossary_leakage:{dt[:32]}")
                break

    return sorted(set(issues))

