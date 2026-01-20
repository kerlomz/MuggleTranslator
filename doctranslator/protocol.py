from __future__ import annotations

import re
from collections import Counter

from doctranslator.errors import TranslationProtocolError
from doctranslator.freezer import unfreeze_text
from doctranslator.ir import TranslationUnit
from doctranslator.sentinels import ANY_SENTINEL_RE, CONTROL_TOKENS, NT_RE, BR, TAB, control_tokens_from_text
from doctranslator.textutil import (
    _CJK_RE,
    _LATIN_RE,
    _NUMBER_TOKEN_RE,
    _NUMBER_VALUE_RE,
    _text_for_lang,
)


_ZERO_WIDTH_RE = re.compile(r"[\u200b\u200c\u200d\u2060]")
_REPEAT_CHAR_RE = re.compile(r"(.)\\1{12,}")

# NOTE: Do not include raw tab (\u0009) here. Tabs must be represented by the explicit sentinel token ⟦TAB⟧.
_WEIRD_WS_RE = re.compile(r"[\u000B\u000C\u00A0\u2000-\u200A\u202F\u205F\u3000\uFEFF]")
_CJK_INNER_SPACE_RE = re.compile(r"(?<=[\u4e00-\u9fff])\s+(?=[\u4e00-\u9fff])")
_EDGE_WS_RE = re.compile(r"^(\s*)(.*?)(\s*)$", flags=re.DOTALL)
_MULTI_SPACE_RE = re.compile(r"[ ]{2,}")
_ZH_SPACE_BEFORE_PUNCT_RE = re.compile(r"\s+([，。！？；：、）】》」』])")
_ZH_SPACE_AFTER_OPEN_PUNCT_RE = re.compile(r"([（【《「『])\s+")
_EN_SPACE_BEFORE_PUNCT_RE = re.compile(r"\s+([,.;:!?])")
_EN_SPACE_AFTER_OPEN_PUNCT_RE = re.compile(r"([(\[{])\s+")
_EN_SPACE_BEFORE_CLOSE_PUNCT_RE = re.compile(r"\s+([)\]}])")


_UNEXPECTED_SCRIPT_CHAR_RE = re.compile(
    r"[\u0900-\u097F\u0980-\u09FF\u0600-\u06FF\u0400-\u04FF\u0370-\u03FF\u0590-\u05FF\u0E00-\u0E7F\uAC00-\uD7AF\u3040-\u309F\u30A0-\u30FF]"
)

_ZH_BAD_REF_PLACEHOLDER_RE = re.compile(r"第\s*(?P<id>X|x|\?|\*|[IVXLCDM]{1,8})\s*(?P<kind>条|款|节|段|章|篇)")
_ZH_BAD_REF_MISSING_ID_RE = re.compile(r"第\s*(条|款|节|段|章|篇)")

_SECTION_REF_RE = re.compile(r"\bSection\s+(\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?)\b", flags=re.IGNORECASE)
_ARTICLE_REF_RE = re.compile(r"\bArticle\s+(\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?)\b", flags=re.IGNORECASE)
_CLAUSE_REF_WORD_RE = re.compile(r"\bClause\s+(\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?)\b", flags=re.IGNORECASE)
_PARA_REF_RE = re.compile(r"\bParagraph\s+(\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?)\b", flags=re.IGNORECASE)
_SCHEDULE_REF_RE = re.compile(r"\bSchedule\s+(\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?)\b", flags=re.IGNORECASE)


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


def _split_by_sentinels(text: str) -> list[str]:
    if not text:
        return [""]
    parts: list[str] = []
    pos = 0
    for m in ANY_SENTINEL_RE.finditer(text):
        parts.append(text[pos : m.start()])
        parts.append(m.group(0))
        pos = m.end()
    parts.append(text[pos:])
    return parts


def _split_edge_ws(text: str) -> tuple[str, str, str]:
    m = _EDGE_WS_RE.match(text or "")
    if not m:
        return ("", text, "")
    return (m.group(1), m.group(2), m.group(3))


def _strip_unexpected_sentinels(text: str, allowed: set[str]) -> str:
    if not text:
        return ""

    def repl(m: re.Match[str]) -> str:
        tok = m.group(0)
        return tok if tok in allowed else ""

    return ANY_SENTINEL_RE.sub(repl, text)


def _strip_unexpected_scripts_for_tu(
    tu: TranslationUnit, translated: str, target_lang: str
) -> tuple[str, bool]:
    if not translated:
        return (translated, False)
    if not (target_lang or "").lower().startswith("zh"):
        return (translated, False)

    src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
    allowed_chars = set(_UNEXPECTED_SCRIPT_CHAR_RE.findall(src_plain))

    parts = _split_by_sentinels(translated)
    out_parts: list[str] = []
    changed = False
    for part in parts:
        if ANY_SENTINEL_RE.fullmatch(part) is not None:
            out_parts.append(part)
            continue
        if not part:
            out_parts.append(part)
            continue
        if not allowed_chars:
            new = _UNEXPECTED_SCRIPT_CHAR_RE.sub("", part)
        else:
            buf: list[str] = []
            for ch in part:
                if _UNEXPECTED_SCRIPT_CHAR_RE.match(ch) and ch not in allowed_chars:
                    changed = True
                    continue
                buf.append(ch)
            new = "".join(buf)
        if new != part:
            changed = True
        out_parts.append(new)
    return ("".join(out_parts), changed)


def _normalize_model_output(text: str) -> str:
    if not text:
        return ""
    text = _WEIRD_WS_RE.sub(" ", text)
    # IMPORTANT: do not normalize raw newlines/tabs here. Those are protocol violations and must be
    # repaired deterministically or rejected by validation (never silently "accepted").
    text = _CJK_INNER_SPACE_RE.sub("", text)
    # Only trim plain spaces; keep any raw control characters so validation can hard-fail if not repaired.
    return text.strip(" ")


def _normalize_sentinel_edge_whitespace_to_source(tu: TranslationUnit, translated: str) -> str:
    if not translated:
        return translated

    src_parts = _split_by_sentinels(tu.frozen_surface)
    tgt_parts = _split_by_sentinels(translated)
    if len(src_parts) != len(tgt_parts):
        return translated

    out_parts: list[str] = []
    for src_part, tgt_part in zip(src_parts, tgt_parts, strict=True):
        if ANY_SENTINEL_RE.fullmatch(src_part) is not None:
            out_parts.append(src_part)
            continue
        src_pre, _src_core, src_suf = _split_edge_ws(src_part)
        tgt_pre, tgt_core, tgt_suf = _split_edge_ws(tgt_part)
        # Never hide raw control characters by "normalizing" them away.
        if (
            ("\r" in tgt_pre)
            or ("\n" in tgt_pre)
            or ("\t" in tgt_pre)
            or ("\r" in tgt_suf)
            or ("\n" in tgt_suf)
            or ("\t" in tgt_suf)
        ):
            out_parts.append(tgt_part)
            continue
        out_parts.append(src_pre + tgt_core + src_suf)
    return "".join(out_parts)


def _normalize_inner_whitespace_for_lang(
    tu: TranslationUnit, translated: str, *, source_lang: str, target_lang: str
) -> tuple[str, list[str], bool]:
    if not translated:
        return ("", [], False)

    tgt = (target_lang or "").lower()
    pieces = _split_by_sentinels(translated)
    out_parts: list[str] = []
    flags: list[str] = []
    changed = False

    for part in pieces:
        if ANY_SENTINEL_RE.fullmatch(part) is not None:
            out_parts.append(part)
            continue
        if not part:
            out_parts.append(part)
            continue

        pre, core, suf = _split_edge_ws(part)
        cur = core
        if tgt.startswith("zh"):
            if _CJK_INNER_SPACE_RE.search(cur):
                flags.append("cjk_inner_space")
                cur = _CJK_INNER_SPACE_RE.sub("", cur)

            if _ZH_SPACE_BEFORE_PUNCT_RE.search(cur) or _ZH_SPACE_AFTER_OPEN_PUNCT_RE.search(cur):
                flags.append("space_punct")
            cur = _ZH_SPACE_BEFORE_PUNCT_RE.sub(r"\1", cur)
            cur = _ZH_SPACE_AFTER_OPEN_PUNCT_RE.sub(r"\1", cur)
        else:
            if _MULTI_SPACE_RE.search(cur):
                flags.append("multi_space")
                cur = _MULTI_SPACE_RE.sub(" ", cur)

            if (
                _EN_SPACE_BEFORE_PUNCT_RE.search(cur)
                or _EN_SPACE_AFTER_OPEN_PUNCT_RE.search(cur)
                or _EN_SPACE_BEFORE_CLOSE_PUNCT_RE.search(cur)
            ):
                flags.append("space_punct")
            cur = _EN_SPACE_BEFORE_PUNCT_RE.sub(r"\1", cur)
            cur = _EN_SPACE_AFTER_OPEN_PUNCT_RE.sub(r"\1", cur)
            cur = _EN_SPACE_BEFORE_CLOSE_PUNCT_RE.sub(r"\1", cur)

        if cur != core:
            changed = True
        out_parts.append(pre + cur + suf)

    out = "".join(out_parts)
    return (out, sorted(set(flags)), changed)


def _int_to_zh_numeral(n: int) -> str:
    digits = "零一二三四五六七八九"
    if n == 0:
        return digits[0]
    if n < 0:
        return "负" + _int_to_zh_numeral(-n)

    units = [(1000, "千"), (100, "百"), (10, "十")]
    parts: list[str] = []
    started = False
    for base, unit in units:
        d = n // base
        n = n % base
        if d:
            started = True
            if base == 10 and d == 1 and not parts:
                parts.append(unit)
            else:
                parts.append(digits[d] + unit)
        else:
            if started and n:
                if not parts or parts[-1] != digits[0]:
                    parts.append(digits[0])
    if n:
        parts.append(digits[n])
    out = "".join(parts)
    out = out.replace("零零", "零")
    if out.endswith("零"):
        out = out[:-1]
    return out


def _sanitize_number_tokens_to_match_source(tu: TranslationUnit, translated: str, target_lang: str) -> str:
    if not translated:
        return translated

    src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
    required = Counter(_NUMBER_TOKEN_RE.findall(src_plain))

    # Count digits contributed by NT tokens already present in the output.
    nt_contrib: Counter[str] = Counter()
    for tok in ANY_SENTINEL_RE.findall(translated):
        if NT_RE.fullmatch(tok):
            original = tu.nt_map.get(tok, "")
            if original:
                nt_contrib.update(_NUMBER_TOKEN_RE.findall(original))

    # We can only remove/keep digits that appear in the plain (non-sentinel) parts.
    needed_plain = required - nt_contrib
    remaining = Counter(needed_plain)

    parts = _split_by_sentinels(translated)
    out_parts: list[str] = []
    for part in parts:
        if ANY_SENTINEL_RE.fullmatch(part) is not None:
            out_parts.append(part)
            continue
        if not part:
            out_parts.append(part)
            continue
        if not required:
            out_parts.append(_NUMBER_TOKEN_RE.sub("", part))
            continue

        rebuilt: list[str] = []
        pos = 0
        for m in _NUMBER_TOKEN_RE.finditer(part):
            rebuilt.append(part[pos : m.start()])
            tok = m.group(0)
            if remaining.get(tok, 0) > 0:
                rebuilt.append(tok)
                remaining[tok] -= 1
            pos = m.end()
        rebuilt.append(part[pos:])
        out_parts.append("".join(rebuilt))

    out = "".join(out_parts)

    def count_numbers_after(text: str) -> Counter[str]:
        tgt_unfrozen = unfreeze_text(text, tu.nt_map)
        tgt_plain = ANY_SENTINEL_RE.sub(" ", tgt_unfrozen)
        return Counter(_NUMBER_TOKEN_RE.findall(tgt_plain))

    cur = count_numbers_after(out)
    missing = required - cur
    if not missing:
        return out

    # Try to convert Chinese numerals in common legal-reference patterns into the required Arabic digits.
    if target_lang == "zh":
        for tok, cnt in list(missing.items()):
            if cnt <= 0:
                continue
            if not tok.isdigit():
                continue
            if tok != "0" and tok.startswith("0"):
                continue
            try:
                n = int(tok)
            except Exception:  # noqa: BLE001
                continue
            cn = _int_to_zh_numeral(n)
            if not cn:
                continue
            pat = re.compile(rf"第\s*{re.escape(cn)}\s*(条|节|款|段|章|篇)")
            out2, rep = pat.subn(lambda m: f"第{tok}{m.group(1)}", out, count=cnt)
            if rep > 0:
                out = out2
                cur = count_numbers_after(out)
                missing = required - cur
                if not missing:
                    return out

        # If the source starts with a 4-digit year (e.g., "2002 ...") but the model dropped it,
        # insert it deterministically to satisfy strict number preservation.
        for tok, cnt in list(missing.items()):
            if cnt <= 0:
                continue
            if re.fullmatch(r"\d{4}", tok) is None:
                continue
            if re.search(rf"(?<!\d){re.escape(tok)}(?!\d)", src_plain[:32]) is None:
                continue

            parts2 = _split_by_sentinels(out)
            inserted = False
            for pi, part in enumerate(parts2):
                if ANY_SENTINEL_RE.fullmatch(part) is not None:
                    continue
                pre, core, suf = _split_edge_ws(part)
                if not core.strip():
                    continue
                ins = tok + "年"
                parts2[pi] = pre + ins + core + suf
                inserted = True
                break
            if inserted:
                out = "".join(parts2)
                cur = count_numbers_after(out)
                missing = required - cur
                if not missing:
                    return out

    # Contextual reference repair for "Section/Article/Clause/Paragraph/Schedule <n>" when the model drops the digit.

    def ref_kind_for_number(num: str) -> str | None:
        for m in _SECTION_REF_RE.finditer(src_plain):
            if m.group(1) == num:
                return "section"
        for m in _ARTICLE_REF_RE.finditer(src_plain):
            if m.group(1) == num:
                return "article"
        for m in _CLAUSE_REF_WORD_RE.finditer(src_plain):
            if m.group(1) == num:
                return "clause"
        for m in _PARA_REF_RE.finditer(src_plain):
            if m.group(1) == num:
                return "paragraph"
        for m in _SCHEDULE_REF_RE.finditer(src_plain):
            if m.group(1) == num:
                return "schedule"
        return None

    def zh_wrapper(kind: str | None, num: str) -> str:
        if kind in {"section", "article"}:
            return f"第{num}条"
        if kind == "clause":
            return f"第{num}款"
        if kind == "paragraph":
            return f"第{num}段"
        if kind == "schedule":
            return f"附表{num}"
        return num

    def try_replace_generic_ref(text: str, wrapper: str) -> str:
        for phrase in ("本条款", "本条", "该条款", "该条", "本节", "该节"):
            idx = text.find(phrase)
            if idx >= 0:
                return text[:idx] + wrapper + text[idx + len(phrase) :]
        return text

    for num, cnt in list(missing.items()):
        if cnt <= 0:
            continue
        for _ in range(cnt):
            if target_lang != "zh":
                continue
            kind = ref_kind_for_number(num)
            if kind is None:
                continue
            insert_text = zh_wrapper(kind, num)
            out2 = try_replace_generic_ref(out, insert_text)
            if out2 != out:
                out = out2

    return out


def _fix_reference_placeholders(tu: TranslationUnit, translated: str, target_lang: str) -> tuple[str, bool]:
    if not translated:
        return (translated, False)
    if not (target_lang or "").lower().startswith("zh"):
        return (translated, False)

    src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
    required_numbers = list(dict.fromkeys(_NUMBER_TOKEN_RE.findall(src_plain)))
    if not required_numbers:
        return (translated, False)

    # If the model produced "第X条/第X款/..." placeholders, try to repair deterministically when unambiguous.
    uniq = required_numbers[0] if len(required_numbers) == 1 else None
    if not uniq:
        return (translated, False)

    parts = _split_by_sentinels(translated)
    out_parts: list[str] = []
    changed = False
    for part in parts:
        if ANY_SENTINEL_RE.fullmatch(part) is not None:
            out_parts.append(part)
            continue
        cur = part
        cur2 = re.sub(rf"第\s*(?:X|x|\?|\*)\s*(条|款|节|段|章|篇)", rf"第{uniq}\1", cur)
        cur2 = re.sub(r"第\s*(条|款|节|段|章|篇)", rf"第{uniq}\1", cur2)
        if cur2 != cur:
            changed = True
            cur = cur2
        out_parts.append(cur)
    return ("".join(out_parts), changed)


def _validate_sentinels(tu: TranslationUnit, translated: str) -> None:
    if "\r" in translated or "\n" in translated:
        raise TranslationProtocolError(f"Raw newline characters found in TU {tu.tu_id}")
    if "\t" in translated:
        raise TranslationProtocolError(f"Raw tab characters found in TU {tu.tu_id}")
    if control_tokens_from_text(tu.frozen_surface) != control_tokens_from_text(translated):
        raise TranslationProtocolError(f"Control tokens mismatch for TU {tu.tu_id}")

    numeric_nt = {token for token, original in tu.nt_map.items() if _NUMBER_VALUE_RE.fullmatch(original) is not None}
    expected_nt = set(tu.nt_map.keys()) - numeric_nt
    found_nt = NT_RE.findall(translated)
    found_nt_tokens = {f"⟦NT:{n}⟧" for n in found_nt}
    found_nt_non_numeric = found_nt_tokens - numeric_nt
    if expected_nt != found_nt_non_numeric:
        missing = len(expected_nt - found_nt_non_numeric)
        extra = len(found_nt_non_numeric - expected_nt)
        raise TranslationProtocolError(f"NT placeholders mismatch for TU {tu.tu_id} (missing={missing}, extra={extra})")

    for token in expected_nt:
        if translated.count(token) != 1:
            raise TranslationProtocolError(f"NT placeholder count != 1 for TU {tu.tu_id}")

    for token in ANY_SENTINEL_RE.findall(translated):
        if token in CONTROL_TOKENS:
            continue
        if NT_RE.fullmatch(token):
            continue
        raise TranslationProtocolError(f"Unexpected sentinel token in TU {tu.tu_id}")

    src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
    tgt_unfrozen = unfreeze_text(translated, tu.nt_map)
    tgt_plain = ANY_SENTINEL_RE.sub(" ", tgt_unfrozen)
    if Counter(_NUMBER_TOKEN_RE.findall(src_plain)) != Counter(_NUMBER_TOKEN_RE.findall(tgt_plain)):
        raise TranslationProtocolError(f"Number tokens mismatch for TU {tu.tu_id}")


def _repair_raw_controls_to_match_source(tu: TranslationUnit, text: str) -> str:
    if not text:
        return ""

    expected = control_tokens_from_text(tu.frozen_surface)
    exp_counts = Counter(expected)
    cur_counts = Counter(control_tokens_from_text(text))

    need_br = max(0, int(exp_counts.get(BR, 0)) - int(cur_counts.get(BR, 0)))
    need_tab = max(0, int(exp_counts.get(TAB, 0)) - int(cur_counts.get(TAB, 0)))

    if ("\r" not in text) and ("\n" not in text) and ("\t" not in text):
        return text

    buf: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "\r":
            if (i + 1) < n and text[i + 1] == "\n":
                i += 2
            else:
                i += 1
            if need_br > 0:
                buf.append(BR)
                need_br -= 1
            else:
                buf.append(" ")
            continue
        if ch == "\n":
            i += 1
            if need_br > 0:
                buf.append(BR)
                need_br -= 1
            else:
                buf.append(" ")
            continue
        if ch == "\t":
            i += 1
            if need_tab > 0:
                buf.append(TAB)
                need_tab -= 1
            else:
                buf.append(" ")
            continue
        buf.append(ch)
        i += 1
    return "".join(buf)


def _strip_prompt_artifacts_if_unexpected(tu: TranslationUnit, translated: str) -> str:
    if not translated:
        return translated
    src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
    out_plain = ANY_SENTINEL_RE.sub(" ", translated)
    if (_PROMPT_TAG_RE.search(out_plain) or _PROMPT_KV_RE.search(out_plain)) and (
        _PROMPT_TAG_RE.search(src_plain) is None and _PROMPT_KV_RE.search(src_plain) is None
    ):
        cur = _PROMPT_TAG_RE.sub(" ", translated)
        # Remove common metadata "Key: Value" fragments injected by prompts.
        cur = _PROMPT_KV_RE.sub(" ", cur)
        cur = re.sub(r"\s{2,}", " ", cur).strip()
        return cur
    return translated


def normalize_candidate_translation(
    tu: TranslationUnit,
    raw: str,
    *,
    source_lang: str,
    target_lang: str,
) -> tuple[str, list[str]]:
    out = _normalize_model_output(raw or "")
    out = _repair_raw_controls_to_match_source(tu, out)
    out = _strip_prompt_artifacts_if_unexpected(tu, out)

    # Restore any missing NT tokens deterministically if the model output the raw original instead.
    if tu.nt_map:
        for tok, original in (tu.nt_map or {}).items():
            if not tok or tok in out:
                continue
            if not original:
                continue
            if original in out:
                out = out.replace(original, tok, 1)

    out = _strip_unexpected_sentinels(out, allowed=set(ANY_SENTINEL_RE.findall(tu.frozen_surface)))

    # Deterministic repairs that do not require any model:
    out = _sanitize_number_tokens_to_match_source(tu, out, target_lang)
    out, _ref_changed = _fix_reference_placeholders(tu, out, target_lang)
    out, _script_changed = _strip_unexpected_scripts_for_tu(tu, out, target_lang)

    out = _normalize_sentinel_edge_whitespace_to_source(tu, out)
    out, ws_flags, _ = _normalize_inner_whitespace_for_lang(tu, out, source_lang=source_lang, target_lang=target_lang)
    out = _normalize_sentinel_edge_whitespace_to_source(tu, out)
    return (out, ws_flags)


def _glossary_lines_for_text(glossary: dict[str, str] | None, *, text: str, max_items: int) -> str:
    if not glossary or not text or max_items <= 0:
        return ""
    src_plain = ANY_SENTINEL_RE.sub(" ", text)
    matched: list[tuple[str, str]] = []
    # Prefer longer source terms first to avoid partial overlaps.
    items = sorted(glossary.items(), key=lambda kv: len(str(kv[0])), reverse=True)
    for k, v in items:
        ks = str(k).strip()
        vs = str(v).strip()
        if not ks or not vs:
            continue
        if _LATIN_RE.search(ks):
            if re.search(re.escape(ks), src_plain, flags=re.IGNORECASE) is None:
                continue
        else:
            if ks not in src_plain:
                continue
        matched.append((ks, vs))
        if len(matched) >= max_items:
            break
    if not matched:
        return ""
    return "\n".join([f"- {k} -> {v}" for k, v in matched])

