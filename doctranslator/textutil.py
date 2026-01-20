from __future__ import annotations

import json
import re
from collections import Counter

from doctranslator.ir import TranslationUnit
from doctranslator.sentinels import ANY_SENTINEL_RE


_CJK_RE = re.compile(r"[\u4e00-\u9fff]")
_LATIN_RE = re.compile(r"[A-Za-z]")
_LATIN_EXT_RE = re.compile(r"[\u00C0-\u024F]")
_DEVANAGARI_RE = re.compile(r"[\u0900-\u097F]")
_BENGALI_RE = re.compile(r"[\u0980-\u09FF]")
_ARABIC_RE = re.compile(r"[\u0600-\u06FF]")
_CYRILLIC_RE = re.compile(r"[\u0400-\u04FF]")
_GREEK_RE = re.compile(r"[\u0370-\u03FF]")
_HEBREW_RE = re.compile(r"[\u0590-\u05FF]")
_THAI_RE = re.compile(r"[\u0E00-\u0E7F]")
_HANGUL_RE = re.compile(r"[\uAC00-\uD7AF]")
_HIRAGANA_RE = re.compile(r"[\u3040-\u309F]")
_KATAKANA_RE = re.compile(r"[\u30A0-\u30FF]")

_NUMBER_TOKEN_RE = re.compile(r"(?<!\d)\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?(?!\d)")
_NUMBER_VALUE_RE = re.compile(r"^\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?$")

_SCOPE_TYPE_RE = re.compile(r"#(w:p|a:p|w:lvlText)@")

_EN_COMMON_WORDS = {
    "a",
    "an",
    "and",
    "are",
    "as",
    "at",
    "be",
    "between",
    "by",
    "dated",
    "for",
    "from",
    "has",
    "have",
    "in",
    "into",
    "is",
    "it",
    "its",
    "may",
    "not",
    "of",
    "on",
    "or",
    "shall",
    "subject",
    "such",
    "that",
    "the",
    "their",
    "this",
    "to",
    "under",
    "will",
    "with",
}

_ENTITY_STOPWORDS = {
    "is",
    "are",
    "was",
    "were",
    "to",
    "the",
    "a",
    "an",
    "of",
    "for",
    "in",
    "on",
    "at",
    "by",
    "as",
    "from",
    "and",
    "or",
    "not",
    "will",
    "shall",
    "may",
    "must",
    "subject",
}

_ENTITY_SUFFIX_RE = re.compile(
    r"\b("
    r"inc\.?|incorporated|ltd\.?|limited|llc|l\.l\.c\.|plc|gmbh|s\.a\.|s\.a\.s\.|s\.r\.l\.|"
    r"corp\.?|corporation|company|co\.|n\.a\.|n\.v\.|ag|bv|b\.v\."
    r")\b",
    flags=re.IGNORECASE,
)

_OTHER_SCRIPT_RES = (
    _DEVANAGARI_RE,
    _BENGALI_RE,
    _ARABIC_RE,
    _CYRILLIC_RE,
    _GREEK_RE,
    _HEBREW_RE,
    _THAI_RE,
    _HANGUL_RE,
    _HIRAGANA_RE,
    _KATAKANA_RE,
)


def _scope_type(scope_key: str) -> str:
    m = _SCOPE_TYPE_RE.search(scope_key)
    return m.group(1) if m else "unknown"


def _preview_for_log(text: str, max_chars: int) -> str:
    if not text:
        return ""
    clean = text.replace("\r", "\\r").replace("\n", "\\n")
    clean = ANY_SENTINEL_RE.sub(lambda m: f"<{m.group(0)[1:-1]}>", clean)
    clean = re.sub(r"\s+", " ", clean).strip()
    if max_chars > 0 and len(clean) > max_chars:
        return clean[: max(0, max_chars - 3)] + "..."
    return clean


def _try_extract_json_obj(text: str) -> dict | None:
    if not text:
        return None
    start = text.find("{")
    if start < 0:
        return None
    try:
        decoder = json.JSONDecoder()
        obj, _end = decoder.raw_decode(text[start:])
    except Exception:  # noqa: BLE001
        return None
    return obj if isinstance(obj, dict) else None


def _normalize_lang(code: str | None) -> str | None:
    if not code:
        return None
    c = code.strip().lower().replace("_", "-")
    if c.startswith("en"):
        return "en"
    if c.startswith("zh"):
        return "zh"
    return None


def _lang_prompt_name(code: str) -> str:
    c = (code or "").strip().lower().replace("_", "-")
    if c.startswith("en"):
        return "English"
    if c.startswith("zh"):
        return "Simplified Chinese"
    return code


def _lang_prompt_native(code: str) -> str:
    c = (code or "").strip().lower().replace("_", "-")
    if c.startswith("en"):
        return "English"
    if c.startswith("zh"):
        return "简体中文"
    return code


def _text_for_lang(text: str) -> str:
    if not text:
        return ""
    t = ANY_SENTINEL_RE.sub(" ", text)
    t = re.sub(r"\s+", " ", t).strip()
    return t


def _other_script_count(text: str) -> int:
    if not text:
        return 0
    return sum(len(rx.findall(text)) for rx in _OTHER_SCRIPT_RES)


def _looks_like_english(text: str) -> bool:
    if not text:
        return False
    words = re.findall(r"[A-Za-z]{2,}", text.lower())
    if not words:
        return False
    common_hits = sum(1 for w in words if w in _EN_COMMON_WORDS)
    if len(words) >= 8:
        return common_hits >= 1
    if len(words) >= 4:
        return common_hits >= 1
    return True


def _looks_like_entity_name(text: str) -> bool:
    if not text:
        return False
    t = re.sub(r"\s+", " ", text).strip()
    if not t or len(t) > 120:
        return False
    if not _ENTITY_SUFFIX_RE.search(t):
        return False
    words = re.findall(r"[A-Za-z][A-Za-z'.]{1,}", t)
    if not words:
        return False
    stop_hits = sum(1 for w in words if w.lower().strip(".") in _ENTITY_STOPWORDS and w.lower() != "and")
    if stop_hits > 0:
        return False

    title_like = 0
    upper_like = 0
    for w in words:
        w2 = w.strip(".")
        if not w2:
            continue
        if w2.isupper() and len(w2) >= 2:
            upper_like += 1
            title_like += 1
            continue
        if w2[0].isupper() and (len(w2) == 1 or w2[1:].islower()):
            title_like += 1

    n = max(len(words), 1)
    if upper_like >= 2 and upper_like / n >= 0.6:
        return True
    if n <= 4 and title_like >= 2 and title_like / n >= 0.6:
        return True
    if n >= 5 and title_like >= 3 and title_like / n >= 0.6:
        return True
    return False


def _detect_language_pair_from_tus(tus: list[TranslationUnit]) -> tuple[str, str, str] | None:
    preferred = [
        tu
        for tu in tus
        if tu.part_name.endswith("word/document.xml") and _scope_type(tu.scope_key) == "w:p" and tu.source_surface
    ]
    base = preferred if preferred else tus
    samples = base[: min(200, len(base))]

    total_cjk = 0
    total_latin = 0
    total_latin_ext = 0
    total_other = 0
    en_votes = 0
    zh_votes = 0
    unknown_votes = 0

    for tu in samples:
        t = _text_for_lang(tu.source_surface)
        if not t:
            continue
        cjk = len(_CJK_RE.findall(t))
        latin = len(_LATIN_RE.findall(t))
        latin_ext = len(_LATIN_EXT_RE.findall(t))
        other = _other_script_count(t)

        total_cjk += cjk
        total_latin += latin
        total_latin_ext += latin_ext
        total_other += other

        if other > 0:
            unknown_votes += 1
            continue
        if cjk >= 4 and cjk >= int((latin + latin_ext) * 2.0):
            zh_votes += 1
            continue
        if latin >= 4 and latin >= int(cjk * 2.0) and latin_ext <= max(1, int(latin * 0.03)):
            if _looks_like_english(t):
                en_votes += 1
            else:
                unknown_votes += 1
            continue
        if cjk > 0 and latin == 0 and latin_ext == 0:
            zh_votes += 1
            continue
        if latin > 0 and cjk == 0 and latin_ext == 0:
            en_votes += 1
            continue
        unknown_votes += 1

    detail = (
        "Auto language detect (en<->zh only): "
        f"en_votes={en_votes} zh_votes={zh_votes} unknown={unknown_votes} "
        f"chars(cjk={total_cjk} latin={total_latin} latin_ext={total_latin_ext} other={total_other})"
    )

    has_en_signal = en_votes > 0 or total_latin > 0
    has_zh_signal = zh_votes > 0 or total_cjk > 0
    if not has_en_signal and not has_zh_signal and total_other > 0:
        return None

    if zh_votes >= 3 and zh_votes >= int(en_votes * 1.2) and total_cjk >= int(total_latin * 1.1):
        return ("zh", "en", detail + " decision=zh->en")
    if en_votes >= 3 and en_votes >= int(zh_votes * 1.2) and total_latin >= int(total_cjk * 1.1):
        return ("en", "zh", detail + " decision=en->zh")

    if total_cjk >= int(total_latin * 1.25) and total_cjk >= 20:
        return ("zh", "en", detail + " decision=zh->en(low_conf)")
    if total_latin >= int(total_cjk * 1.25) and total_latin >= 20:
        return ("en", "zh", detail + " decision=en->zh(low_conf)")

    if zh_votes > en_votes:
        return ("zh", "en", detail + " decision=zh->en(weak_vote)")
    if en_votes > zh_votes:
        return ("en", "zh", detail + " decision=en->zh(weak_vote)")

    if total_cjk > total_latin:
        return ("zh", "en", detail + " decision=zh->en(weak_char)")
    return ("en", "zh", detail + " decision=en->zh(weak_char)")


def _should_translate_tu(tu: TranslationUnit, source_lang: str) -> tuple[bool, str]:
    t = _text_for_lang(tu.source_surface)
    if not t:
        return (False, "empty")
    # If the TU is composed purely of sentinel tokens (e.g. a frozen trademark/URL/leader),
    # there is nothing to translate. Keep the frozen surface unchanged so it can be
    # projected back to Word runs without any placeholder drift.
    if not ANY_SENTINEL_RE.sub("", tu.frozen_surface or "").strip():
        return (False, "sentinel_only")

    other = _other_script_count(t)
    cjk = len(_CJK_RE.findall(t))
    latin = len(_LATIN_RE.findall(t))
    latin_ext = len(_LATIN_EXT_RE.findall(t))

    if other > 0:
        signal = cjk + latin + latin_ext
        if signal == 0:
            return (False, "other_script")

    if source_lang == "en":
        if latin == 0 and cjk > 0:
            return (False, "already_zh")
        if latin == 0:
            return (False, "no_latin")
        if latin_ext >= 2 and not _looks_like_english(t):
            return (False, "non_english_latin")
        return (True, "ok")

    if source_lang == "zh":
        if cjk == 0 and latin > 0:
            return (False, "already_en")
        if cjk == 0:
            return (False, "no_cjk")
        return (True, "ok")

    return (True, "ok")


def number_tokens_in_text(text: str) -> Counter[str]:
    plain = ANY_SENTINEL_RE.sub(" ", text or "")
    return Counter(_NUMBER_TOKEN_RE.findall(plain))

