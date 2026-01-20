from __future__ import annotations

import re
from dataclasses import dataclass, field

from doctranslator.sentinels import ANY_SENTINEL_RE, nt_token


_URL = r"https?://[^\s<>()]+"
_EMAIL = r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"
_WIN_PATH = r"(?:[A-Za-z]:\\(?:[^\\/:*?\"<>|\r\n]+\\)*[^\\/:*?\"<>|\r\n]*)"
_PLACEHOLDER = r"(?:\{[^{}\r\n]{1,100}\}|\$\{[^{}\r\n]{1,100}\})"
_PERCENT_SLOT = r"%\d+"
# Freeze legal references as an atomic unit so the translation model cannot drop/alter
# section/article numbers (a common catastrophic failure in contracts).
#
# Examples:
# - Section 7
# - Section 2(c)(i)
# - Sec. 6(b)(iv)
# - Article IV
_EN_LEGAL_REF = (
    r"\b(?:Section|Article|Clause|Paragraph|Schedule)s?\s+"
    r"(?:\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?(?:\([A-Za-z0-9]+\))*|[IVXLCDM]{1,8})\b"
)
_EN_LEGAL_REF_ABBR = (
    r"\b(?:Sec|Art|Cl|Para|Sch)s?\.\s+"
    r"(?:\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?(?:\([A-Za-z0-9]+\))*|[IVXLCDM]{1,8})\b"
)
_ZH_LEGAL_REF = r"第\s*\d+(?:\([A-Za-z0-9]+\))*\s*(?:条|款|项|段|章|节)\b"
_ZH_SCHEDULE_REF = r"(?:附表|附件)\s*\d+(?:\([A-Za-z0-9]+\))*\b"
_CLAUSE_REF = r"\b\d+(?:\([A-Za-z0-9]+\))+"
_ENUM_NUM = r"\(\d{1,3}\)"
_ENUM_ROMAN = r"\((?:[ivxlcdmIVXLCDM]{1,6})\)"
_ENUM_ALPHA = r"\([A-Za-z]\)"
_DOT_LEADER = r"[.\u2026]{8,}"
_UNDERSCORE_LEADER = r"_{5,}"
_DASH_LEADER = r"[-\u2010\u2011\u2012\u2013\u2014\u2015\u2212]{5,}"
_TRADEMARK_TOKEN = r"[A-Za-z0-9]{2,24}[®™℠]"
_OTHER_SCRIPT_RUN = r"[\u0900-\u097F\u0980-\u09FF\u0600-\u06FF\u0400-\u04FF\u0370-\u03FF\u0590-\u05FF\u0E00-\u0E7F\uAC00-\uD7AF\u3040-\u309F\u30A0-\u30FF]+"
# Contracts often use standalone X/Y/Z as neutral party variables. These must never be
# translated (e.g., into 甲/乙) or dropped, otherwise cross-references break.
_VAR_MARKER = r"\b[XYZ]\b"

FREEZE_RE = re.compile(
    rf"({_TRADEMARK_TOKEN}|{_OTHER_SCRIPT_RUN}|{_URL}|{_EMAIL}|{_WIN_PATH}|{_PLACEHOLDER}|{_PERCENT_SLOT}|"
    rf"{_EN_LEGAL_REF}|{_EN_LEGAL_REF_ABBR}|{_ZH_LEGAL_REF}|{_ZH_SCHEDULE_REF}|"
    rf"{_CLAUSE_REF}|{_ENUM_NUM}|{_ENUM_ROMAN}|{_ENUM_ALPHA}|{_DOT_LEADER}|{_UNDERSCORE_LEADER}|{_DASH_LEADER}|{_VAR_MARKER})",
    flags=re.IGNORECASE,
)


@dataclass
class FreezeResult:
    text: str
    nt_map: dict[str, str] = field(default_factory=dict)


def freeze_text(text: str) -> FreezeResult:
    nt_map: dict[str, str] = {}
    next_id = 1

    def add_token(original: str) -> str:
        nonlocal next_id
        token = nt_token(next_id)
        next_id += 1
        nt_map[token] = original
        return token

    def freeze_plain(plain: str) -> str:
        return FREEZE_RE.sub(lambda m: add_token(m.group(0)), plain)

    pieces: list[str] = []
    pos = 0
    for m in ANY_SENTINEL_RE.finditer(text):
        pieces.append(freeze_plain(text[pos : m.start()]))
        pieces.append(m.group(0))
        pos = m.end()
    pieces.append(freeze_plain(text[pos:]))
    return FreezeResult(text="".join(pieces), nt_map=nt_map)


def unfreeze_text(text: str, nt_map: dict[str, str]) -> str:
    if not nt_map:
        return text

    def repl(m: re.Match[str]) -> str:
        token = m.group(0)
        return nt_map.get(token, token)

    return re.sub(r"⟦NT:\d{4}⟧", repl, text)
