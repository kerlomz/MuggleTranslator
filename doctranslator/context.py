from __future__ import annotations

import json
import re
from dataclasses import dataclass, field

from doctranslator.models import ChatModel
from doctranslator.sentinels import ANY_SENTINEL_RE, BR, NBH, SHY, TAB


_QUOTE_TERM_RE = re.compile(r"(?:[“\"‘'])([^”\"’'\r\n]{2,80})(?:[”\"’'])")
_ACRONYM_RE = re.compile(r"\b[A-Z][A-Z0-9]{1,12}\b")
_TITLE_RE = re.compile(r"\b[A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,6}\b")
_TITLE_WITH_CONNECTOR_RE = re.compile(
    r"\b[A-Z][a-z]+(?:\s+[A-Z][a-z]+){0,6}"
    r"(?:\s+(?:and|of|for|to|the|&)\s+[A-Z][a-z]+(?:\s+[A-Z][a-z]+){0,6})+\b"
)


def _try_extract_json(text: str) -> dict | None:
    start = text.find("{")
    if start < 0:
        return None
    try:
        decoder = json.JSONDecoder()
        obj, _end = decoder.raw_decode(text[start:])
    except Exception:  # noqa: BLE001
        return None
    return obj if isinstance(obj, dict) else None


def _parse_kv_fallback(text: str) -> dict[str, str]:
    out: dict[str, str] = {}
    if not text:
        return out

    def put(key: str, value: str) -> None:
        v = (value or "").strip().strip('"').strip("'")
        if v:
            out[key] = v

    for raw_line in text.splitlines():
        line = raw_line.strip().lstrip("-*•").strip()
        if not line:
            continue
        low = line.lower()
        if ":" in line:
            k, v = line.split(":", 1)
            k2 = k.strip().lower().replace(" ", "_")
            if k2 in {"domain", "领域"}:
                put("domain", v)
            elif k2 in {"doc_type", "document_type", "documenttype", "type", "文档类型", "类型"}:
                put("doc_type", v)
            elif k2 in {"target_style", "zh_style", "chinese_style", "english_style", "style", "风格", "写作风格"}:
                put("target_style", v)
            elif k2 in {"summary", "摘要"}:
                put("summary", v)
        if "domain" in low and "domain" not in out:
            m = re.search(r"domain\s*[:=]\s*(.+)$", line, flags=re.IGNORECASE)
            if m:
                put("domain", m.group(1))
        if ("doc_type" in low or "document type" in low) and "doc_type" not in out:
            m = re.search(r"(?:doc[_ ]?type|document type)\s*[:=]\s*(.+)$", line, flags=re.IGNORECASE)
            if m:
                put("doc_type", m.group(1))
        if ("target_style" in low or "zh_style" in low or "chinese style" in low or "english style" in low or "style" in low) and "target_style" not in out:
            m = re.search(
                r"(?:target[_ ]?style|zh[_ ]?style|chinese style|english style|style)\s*[:=]\s*(.+)$",
                line,
                flags=re.IGNORECASE,
            )
            if m:
                put("target_style", m.group(1))
        if "summary" in low and "summary" not in out:
            m = re.search(r"summary\s*[:=]\s*(.+)$", line, flags=re.IGNORECASE)
            if m:
                put("summary", m.group(1))

    return out


def _analysis_text(text: str) -> str:
    if not text:
        return ""
    out = text
    out = out.replace(TAB, "\t")
    out = out.replace(BR, "\n")
    out = out.replace(NBH, "-")
    out = out.replace(SHY, "-")
    out = ANY_SENTINEL_RE.sub(" ", out)
    return out


def extract_candidate_terms(excerpts: list[str], max_terms: int) -> list[str]:
    counts: dict[str, int] = {}

    def add(term: str, weight: int = 1) -> None:
        t = term.strip()
        if not t:
            return
        if len(t) < 2:
            return
        if len(t) > 120:
            return
        counts[t] = counts.get(t, 0) + weight

    for raw in excerpts:
        text = _analysis_text(raw)
        for m in _QUOTE_TERM_RE.finditer(text):
            add(m.group(1), weight=8)
        for m in _ACRONYM_RE.finditer(text):
            add(m.group(0), weight=4)
        for m in _TITLE_WITH_CONNECTOR_RE.finditer(text):
            add(m.group(0), weight=3)
        for m in _TITLE_RE.finditer(text):
            add(m.group(0), weight=2)

    ranked = sorted(counts.items(), key=lambda kv: (kv[1], len(kv[0])), reverse=True)
    out: list[str] = []
    out_low: list[str] = []
    for term, _score in ranked:
        term_low = term.lower()
        if any(term_low in prev for prev in out_low if len(prev) >= len(term_low)):
            continue
        out.append(term)
        out_low.append(term_low)
        if max_terms and len(out) >= max_terms:
            break
    return out


@dataclass(frozen=True)
class DocumentContext:
    source_lang: str
    target_lang: str
    domain: str | None = None
    doc_type: str | None = None
    target_style: str | None = None
    style_guide: str | None = None
    summary: str | None = None
    glossary: dict[str, str] = field(default_factory=dict)

    def format_glossary(self, max_items: int = 30) -> str:
        if not self.glossary:
            return ""
        items = list(self.glossary.items())
        items = items[: max_items if max_items > 0 else len(items)]
        return "\n".join([f"- {k} -> {v}" for k, v in items])


def build_document_context(
    *,
    excerpts: list[str],
    source_lang: str,
    target_lang: str,
    term_candidates: list[str],
    gemma1b: ChatModel | None,
    gemma4b: ChatModel | None,
    enable_style_guide: bool = True,
    forced_target_style: str | None = None,
) -> DocumentContext:
    domain = None
    doc_type = None
    target_style = None
    style_guide = None
    summary = None
    glossary: dict[str, str] = {}

    # Keep context prompts within the (often small) runtime n_ctx (e.g. 2k).
    # We progressively add excerpts until we hit a safe token budget.
    raw_excerpts = [_analysis_text(x).strip() for x in excerpts if x and x.strip()]
    # Clip each excerpt to avoid one huge paragraph dominating the budget.
    raw_excerpts = [x[:1200] for x in raw_excerpts if x]
    excerpts_text = "\n\n".join(raw_excerpts)

    # Prefer the larger model for document-level analysis when available.
    analyst = gemma4b or gemma1b
    if analyst is not None and excerpts_text.strip():
        target_style_label = "writing style"
        if "chinese" in target_lang.lower() or "中文" in target_lang:
            target_style_label = "Chinese writing style"
        elif "english" in target_lang.lower():
            target_style_label = "English writing style"
        prompt_prefix = (
            "You are a document translation analyst.\n"
            f"The document is in {source_lang}. The target language is {target_lang}.\n"
            f"Given the excerpts, infer the domain and document type, and recommend a target {target_style_label}.\n"
            "Return STRICT JSON only.\n"
            'Keys: "domain", "doc_type", "target_style", "summary". Values must be strings.\n\n'
        )
        kept: list[str] = []
        budget = max(256, int(getattr(analyst, "n_ctx", 2048)) - 256)
        for ex in raw_excerpts:
            cand = "\n\n".join([*kept, ex]).strip()
            probe = prompt_prefix + "Excerpts (context):\n" + cand + "\n"
            if (analyst.count_tokens(probe) or 0) <= budget:
                kept.append(ex)
            else:
                break
        excerpts_text = "\n\n".join(kept).strip()
        prompt = prompt_prefix + "Excerpts (context):\n" + excerpts_text + "\n"
        out = analyst.generate(prompt, max_new_tokens=512, do_sample=False)
        data = _try_extract_json(out) or _parse_kv_fallback(out)
        domain = str(data.get("domain") or "").strip() or None
        doc_type = str(data.get("doc_type") or "").strip() or None
        target_style = str(data.get("target_style") or "").strip() or None
        summary = str(data.get("summary") or "").strip() or None

    if not target_style:
        doc_low = (doc_type or "").strip().lower()
        domain_low = (domain or "").strip().lower()

        is_legal = any(k in doc_low for k in ("contract", "agreement", "legal", "terms")) or domain_low in {
            "law",
            "legal",
        }
        is_paper = any(k in doc_low for k in ("paper", "research", "academic", "thesis", "journal"))
        is_news = any(k in doc_low for k in ("news", "press", "report"))
        is_novel = any(k in doc_low for k in ("novel", "fiction", "story"))
        is_diary = any(k in doc_low for k in ("diary", "journal", "blog"))

        if "chinese" in target_lang.lower() or "中文" in target_lang:
            if is_legal:
                target_style = "正式、严谨的法律/合同中文；术语一致；引用条款/章节保留阿拉伯数字；不增删、不弱化义务与条件；句式清晰。"
            elif is_paper:
                target_style = "学术中文；逻辑严密；术语一致；保留符号/单位/公式；不增删观点；表达自然、无翻译腔。"
            elif is_news:
                target_style = "新闻报道中文；客观简洁；信息完整；专有名词规范；数字与时间准确；语言自然流畅。"
            elif is_novel:
                target_style = "小说/叙事中文；语气与人物口吻贴合；节奏自然；保留修辞与情感色彩；避免直译与拼接感。"
            elif is_diary:
                target_style = "日记/随笔中文；口吻自然；情绪与语气一致；不刻意书面化；表达地道。"
            else:
                target_style = "自然流畅、忠实准确；保持原文文体与语气；术语一致；数字与占位符保留；避免翻译腔与拼接感。"
        elif "english" in target_lang.lower():
            if is_legal:
                target_style = "Formal legal English; precise and faithful; consistent terminology; keep Section/Article digits; no additions/omissions; clear contract register."
            elif is_paper:
                target_style = "Academic English; precise and structured; consistent terminology; preserve symbols/units; no added claims; natural scholarly tone."
            elif is_news:
                target_style = "News English; neutral and concise; complete facts; consistent proper nouns; accurate numbers/dates; natural flow."
            elif is_novel:
                target_style = "Literary English; natural narrative voice; preserve tone and imagery; avoid literal stiffness; no stitching feel."
            elif is_diary:
                target_style = "Diary-style English; natural and personal voice; consistent mood; avoid over-formalization."
            else:
                target_style = "Faithful, natural, and consistent; match the document register and tone; preserve numbers/placeholders; avoid translationese."

    if forced_target_style is not None:
        forced = str(forced_target_style).strip()
        if forced:
            target_style = forced

    if gemma4b is not None and term_candidates:
        # Token-budget the term list to avoid context overflow on small n_ctx.
        kept_terms: list[str] = []
        budget = max(256, int(getattr(gemma4b, "n_ctx", 2048)) - 256)
        for t in term_candidates:
            kept_terms.append(t)
            term_lines_probe = "\n".join([f"- {x}" for x in kept_terms])
            probe = (
                f"You are a professional {source_lang} to {target_lang} translator and terminologist.\n"
                "Your task is to create a preferred translation glossary for a single document.\n"
                "Rules:\n"
                "1) Output STRICT JSON only (no markdown, no commentary).\n"
                "2) Keys must be EXACTLY the source terms as provided.\n"
                "3) Values must be the preferred translation in the target language.\n"
                "4) Be consistent across the glossary.\n"
                "5) If an acronym should remain unchanged, output the same acronym.\n\n"
                f"Domain: {domain or ''}\n"
                f"Document type: {doc_type or ''}\n"
                f"Target writing style: {target_style or ''}\n\n"
                "Terms:\n"
                f"{term_lines_probe}\n"
            )
            if (gemma4b.count_tokens(probe) or 0) > budget:
                kept_terms.pop()
                break
        term_lines = "\n".join([f"- {t}" for t in kept_terms])
        prompt = (
            f"You are a professional {source_lang} to {target_lang} translator and terminologist.\n"
            "Your task is to create a preferred translation glossary for a single document.\n"
            "Rules:\n"
            "1) Output STRICT JSON only (no markdown, no commentary).\n"
            "2) Keys must be EXACTLY the source terms as provided.\n"
            "3) Values must be the preferred translation in the target language.\n"
            "4) Be consistent across the glossary.\n"
            "5) If an acronym should remain unchanged, output the same acronym.\n\n"
            f"Domain: {domain or ''}\n"
            f"Document type: {doc_type or ''}\n"
            f"Target writing style: {target_style or ''}\n\n"
            "Terms:\n"
            f"{term_lines}\n"
        )
        out = gemma4b.generate(prompt, max_new_tokens=1024, do_sample=False)
        data = _try_extract_json(out)
        if isinstance(data, dict) and data:
            for k, v in data.items():
                ks = str(k).strip()
                vs = str(v).strip()
                if not ks or not vs:
                    continue
                glossary[ks] = vs
        else:
            # Fallback: parse "Term -> Translation" lines.
            for raw_line in out.splitlines():
                line = raw_line.strip().lstrip("-*•").strip()
                if not line:
                    continue
                if "->" in line:
                    left, right = line.split("->", 1)
                    ks = left.strip()
                    vs = right.strip()
                elif ":" in line:
                    left, right = line.split(":", 1)
                    ks = left.strip()
                    vs = right.strip()
                else:
                    continue
                if not ks or not vs:
                    continue
                glossary[ks] = vs

    if enable_style_guide and gemma4b is not None:
        glossary_preview = ""
        if glossary:
            shown = 0
            lines: list[str] = []
            for k, v in glossary.items():
                lines.append(f"- {k} -> {v}")
                shown += 1
                if shown >= 12:
                    break
            glossary_preview = "\n".join(lines)
        base_prompt = (
            f"You are a professional {source_lang} to {target_lang} translator.\n"
            "Create a concise global style guide for translating a single document.\n"
            "Output plain text only (no JSON), max 10 bullet lines.\n"
            "Must cover: tone/register, consistency, punctuation/quotes, and how to translate legal references like Section/Article (keep digits).\n\n"
            f"Domain: {domain or ''}\n"
            f"Document type: {doc_type or ''}\n"
            f"Target writing style: {target_style or ''}\n\n"
        )
        prompt = (
            base_prompt
            + ("Glossary (must follow):\n" + glossary_preview + "\n\n" if glossary_preview else "")
            + ("Document summary:\n" + summary + "\n\n" if summary else "")
        )
        budget = max(256, int(getattr(gemma4b, "n_ctx", 2048)) - 256)
        if (gemma4b.count_tokens(prompt) or 0) > budget:
            prompt = base_prompt + ("Glossary (must follow):\n" + glossary_preview + "\n\n" if glossary_preview else "")
        if (gemma4b.count_tokens(prompt) or 0) > budget:
            prompt = base_prompt
        out = gemma4b.generate(prompt, max_new_tokens=384, do_sample=False)
        sg = out.strip()
        if sg:
            style_guide = sg[:1200]

    return DocumentContext(
        source_lang=source_lang,
        target_lang=target_lang,
        domain=domain,
        doc_type=doc_type,
        target_style=target_style,
        style_guide=style_guide,
        summary=summary,
        glossary=glossary,
    )
