from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Callable

from doctranslator.hierarchy import ParagraphContext
from doctranslator.ir import TranslationUnit
from doctranslator.models import ChatModel
from doctranslator.protocol import normalize_candidate_translation, _validate_sentinels
from doctranslator.quality import hard_issues, quality_issues
from doctranslator.sentinels import ANY_SENTINEL_RE, decode_sentinels_from_model
from doctranslator.textutil import (
    _lang_prompt_name,
    _lang_prompt_native,
    _preview_for_log,
    _scope_type,
    _should_translate_tu,
    _try_extract_json_obj,
)
from doctranslator.translate_agent import AgentContext


_DECISION_RISK_RE = re.compile(
    r"\b("
    r"shall|must|may\s+not|may|unless|provided\s+that|in\s+the\s+event|notwithstanding|subject\s+to|"
    r"void|invalid|terminate|termination|breach|indemnif|representation|warrant|condition|"
    r"governing\s+law|jurisdiction|assignment|transfer|consent|notice|default"
    r")\b",
    flags=re.IGNORECASE,
)


@dataclass(frozen=True)
class HardFailure:
    tu: TranslationUnit
    skip_reason: str
    issues: list[str]
    protocol_error: str | None


def _needs_review(
    tu: TranslationUnit,
    *,
    issues: list[str],
    para_ctx: ParagraphContext | None,
    decision_min_chars: int,
) -> bool:
    if hard_issues(issues):
        return True
    if tu.force_ape:
        return True
    src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
    if len(src_plain) >= max(0, int(decision_min_chars)):
        return True
    if para_ctx is not None:
        if para_ctx.is_heading:
            return True
        if para_ctx.list_level is not None:
            return True
        if para_ctx.in_table:
            return True
    if _DECISION_RISK_RE.search(src_plain):
        return True
    return False


def _eval(
    tu: TranslationUnit,
    text: str,
    *,
    source_lang: str,
    target_lang: str,
    glossary_dict: dict[str, str] | None,
) -> tuple[list[str], str | None]:
    protocol_error: str | None = None
    issues: list[str] = []
    try:
        _validate_sentinels(tu, text)
    except Exception as exc:  # noqa: BLE001
        protocol_error = f"{type(exc).__name__}: {exc}"
        issues.append("protocol_error")
    issues.extend(quality_issues(tu, text, source_lang=source_lang, target_lang=target_lang, glossary_dict=glossary_dict))
    return (sorted(set(issues)), protocol_error)


def _review_prompt(
    *,
    src_name: str,
    tgt_name: str,
    tgt_native: str,
    ctx: AgentContext,
    struct_hint: str | None,
    neighbor_prev: str | None,
    neighbor_next: str | None,
    glossary_lines: str | None,
    issues: list[str],
    protocol_error: str | None,
    source_text: str,
    draft: str,
) -> str:
    ctx_lines: list[str] = []
    if ctx.domain:
        ctx_lines.append(f"domain={ctx.domain}")
    if ctx.doc_type:
        ctx_lines.append(f"doc_type={ctx.doc_type}")
    if ctx.target_style:
        ctx_lines.append(f"target_style={ctx.target_style}")
    ctx_block = ("Document context: " + " | ".join(ctx_lines) + "\n\n") if ctx_lines else ""
    if ctx.style_guide:
        ctx_block += "Style guide (must follow):\n" + str(ctx.style_guide).strip()[:900] + "\n\n"
    if glossary_lines:
        ctx_block += "Glossary (must follow):\n" + str(glossary_lines).strip()[:900] + "\n\n"
    if ctx.summary:
        ctx_block += "Document summary (context only):\n" + str(ctx.summary).strip()[:900] + "\n\n"
    if struct_hint:
        ctx_block += "Structure hints (context only):\n" + str(struct_hint).strip()[:700] + "\n\n"
    if neighbor_prev:
        ctx_block += "Prev source paragraph (context only):\n" + str(neighbor_prev).strip()[:520] + "\n\n"
    if neighbor_next:
        ctx_block += "Next source paragraph (context only):\n" + str(neighbor_next).strip()[:520] + "\n\n"

    role_desc = "translation reviewer"
    if ctx.doc_type:
        role_desc = f"{str(ctx.doc_type).strip()} translation reviewer"
    elif ctx.domain:
        role_desc = f"{str(ctx.domain).strip()} translation reviewer"

    return (
        f"You are a professional {src_name} to {tgt_name} {role_desc}.\n"
        f"Target language: {tgt_native}.\n"
        "Task: Review DRAFT against SOURCE and decide whether a rewrite is needed.\n"
        "Return STRICT JSON only.\n"
        'Schema: {"ok": true/false, "score": 0-100, "rewrite": "...", "flags": ["..."]}\n\n'
        "Hard constraints for rewrite:\n"
        "- Output ONLY the translation (no labels/metadata).\n"
        "- Do NOT omit any content; do NOT summarize.\n"
        "- Do NOT add new information; do NOT expand.\n"
        "- Do NOT introduce any new conditions/limitations/exceptions that are not in the source.\n"
        "- Preserve all placeholder tokens enclosed in ⟦...⟧ exactly; do not add/remove/reorder.\n\n"
        + (f"protocol_error: {protocol_error}\n" if protocol_error else "")
        + ("issues: " + ", ".join(issues) + "\n\n" if issues else "\n")
        + ctx_block
        + "SOURCE:\n"
        + source_text
        + "\n\nDRAFT:\n"
        + draft
        + "\n"
    )


def _local_glossary_lines(glossary: dict[str, str] | None, *, text: str, max_items: int) -> str | None:
    if not glossary or not text or max_items <= 0:
        return None
    src_plain = ANY_SENTINEL_RE.sub(" ", text)
    matched: list[tuple[str, str]] = []
    items = sorted(glossary.items(), key=lambda kv: len(str(kv[0])), reverse=True)
    for k, v in items:
        ks = str(k).strip()
        vs = str(v).strip()
        if not ks or not vs:
            continue
        if ks and (re.search(re.escape(ks), src_plain, flags=re.IGNORECASE) is not None):
            matched.append((ks, vs))
            if len(matched) >= max_items:
                break
    if not matched:
        return None
    return "\n".join([f"- {k} -> {v}" for k, v in matched])


def scan_hard_failures(
    *,
    tus: list[TranslationUnit],
    source_lang: str,
    target_lang: str,
    ctx: AgentContext,
) -> list[HardFailure]:
    failures: list[HardFailure] = []
    glossary_dict = ctx.glossary
    for tu in tus:
        should_translate, skip_reason = _should_translate_tu(tu, source_lang=source_lang)
        if not should_translate:
            continue
        final = tu.final_translation or tu.draft_translation or tu.frozen_surface
        issues, proto = _eval(tu, final, source_lang=source_lang, target_lang=target_lang, glossary_dict=glossary_dict)
        if hard_issues(issues):
            failures.append(HardFailure(tu=tu, skip_reason=skip_reason, issues=issues, protocol_error=proto))
    return failures


def final_review_and_repair(
    *,
    progress: object,
    agent: ChatModel | None,
    tus: list[TranslationUnit],
    source_lang: str,
    target_lang: str,
    ctx: AgentContext,
    para_contexts: dict[int, ParagraphContext] | None,
    decision_min_chars: int,
    glossary_max_items_per_tu: int,
    max_new_tokens: int,
    repair_rounds: int,
    log_tu_every: int,
    on_tu_revised: Callable[[TranslationUnit], None] | None = None,
) -> None:
    total = len(tus)
    glossary_dict = ctx.glossary

    if agent is None:
        for tu in tus:
            tu.final_translation = tu.draft_translation or tu.frozen_surface
        return

    progress.info("Final review enabled: agent review + strict hard-failure scan")
    progress.progress("Final review", 0, max(total, 1))

    src_name = _lang_prompt_name(source_lang)
    tgt_name = _lang_prompt_name(target_lang)
    tgt_native = _lang_prompt_native(target_lang)

    for i, tu in enumerate(tus, start=1):
        should_translate, skip_reason = _should_translate_tu(tu, source_lang=source_lang)
        draft = tu.final_translation or tu.draft_translation or tu.frozen_surface
        if not should_translate:
            tu.final_translation = draft
            progress.progress("Final review", i, max(total, 1))
            continue

        para_ctx = para_contexts.get(tu.tu_id) if para_contexts is not None else None
        struct_hint = para_ctx.format_for_prompt() if para_ctx is not None else None
        neighbor_prev = tus[i - 2].source_surface if i - 2 >= 0 else None
        neighbor_next = tus[i].source_surface if i < len(tus) else None
        glossary_lines = _local_glossary_lines(glossary_dict, text=tu.source_surface, max_items=glossary_max_items_per_tu)

        issues, proto = _eval(tu, draft, source_lang=source_lang, target_lang=target_lang, glossary_dict=glossary_dict)
        if not _needs_review(tu, issues=issues, para_ctx=para_ctx, decision_min_chars=decision_min_chars):
            tu.final_translation = draft
            progress.progress("Final review", i, max(total, 1))
            continue

        prompt = _review_prompt(
            src_name=src_name,
            tgt_name=tgt_name,
            tgt_native=tgt_native,
            ctx=ctx,
            struct_hint=struct_hint,
            neighbor_prev=neighbor_prev,
            neighbor_next=neighbor_next,
            glossary_lines=glossary_lines,
            issues=issues,
            protocol_error=proto,
            source_text=tu.frozen_surface,
            draft=draft,
        )

        try:
            out = agent.generate(prompt, max_new_tokens=min(int(max_new_tokens or 1024), 640), do_sample=False)
        except Exception as exc:  # noqa: BLE001
            progress.info(f"Final review TU#{tu.tu_id} failed: {type(exc).__name__}: {exc}")
            tu.final_translation = draft
            progress.progress("Final review", i, max(total, 1))
            continue

        data = _try_extract_json_obj(out)
        rewrite = ""
        score = None
        if isinstance(data, dict):
            r = data.get("rewrite")
            rewrite = r.strip() if isinstance(r, str) else ""
            s = data.get("score")
            if isinstance(s, int):
                score = max(0, min(100, int(s)))
        if score is not None:
            tu.qe_score = score

        if not rewrite:
            tu.final_translation = draft
            if (tu.tu_id <= 5) or (tu.tu_id % max(1, int(log_tu_every)) == 0) or hard_issues(issues):
                progress.info(f"Final review TU#{tu.tu_id}: keep score={tu.qe_score} skip={skip_reason}")
            progress.progress("Final review", i, max(total, 1))
            continue

        cand = decode_sentinels_from_model(rewrite)
        cand, ws_flags = normalize_candidate_translation(tu, cand, source_lang=source_lang, target_lang=target_lang)
        if ws_flags:
            tu.ws_flags = sorted(set([*tu.ws_flags, *ws_flags]))
        new_issues, _new_proto = _eval(tu, cand, source_lang=source_lang, target_lang=target_lang, glossary_dict=glossary_dict)

        # Accept rewrite only if it does not introduce hard issues, and improves (or fixes) problems.
        if hard_issues(new_issues):
            tu.final_translation = draft
            progress.info(f"Final review TU#{tu.tu_id}: rewrite rejected (issues={','.join(hard_issues(new_issues)[:5])})")
        else:
            tu.final_translation = cand
            tu.qe_flags = new_issues
            if on_tu_revised:
                on_tu_revised(tu)
        progress.progress("Final review", i, max(total, 1))

    failures = scan_hard_failures(tus=tus, source_lang=source_lang, target_lang=target_lang, ctx=ctx)
    if not failures:
        return

    progress.info(f"Hard failures detected: {len(failures)}. Running automatic repair with Gemma agent.")

    max_rounds = max(0, int(repair_rounds))
    for round_idx in range(max_rounds):
        failures = scan_hard_failures(tus=tus, source_lang=source_lang, target_lang=target_lang, ctx=ctx)
        if not failures:
            return
        progress.info(f"Hard-failure repair round {round_idx + 1}/{max_rounds}: items={len(failures)}")
        for item in failures:
            tu = item.tu
            draft = tu.final_translation or tu.draft_translation or tu.frozen_surface
            para_ctx = para_contexts.get(tu.tu_id) if para_contexts is not None else None
            struct_hint = para_ctx.format_for_prompt() if para_ctx is not None else None
            glossary_lines = _local_glossary_lines(glossary_dict, text=tu.source_surface, max_items=glossary_max_items_per_tu)

            fix_prompt = (
                f"You are a professional {src_name} to {tgt_name} translator and editor.\n"
                "Fix DRAFT to satisfy ALL constraints.\n"
                f"Output language must be {tgt_native}.\n"
                "Output ONLY the fixed translation.\n\n"
                "Constraints:\n"
                "- Do NOT omit any content; do NOT summarize.\n"
                "- Do NOT add new information; do NOT expand.\n"
                "- Do NOT introduce any new conditions/limitations/exceptions.\n"
                "- Preserve all placeholder tokens enclosed in ⟦...⟧ exactly; do not add/remove/reorder.\n\n"
                + ("Structure hints (context only):\n" + str(struct_hint).strip()[:700] + "\n\n" if struct_hint else "")
                + ("Glossary (must follow):\n" + str(glossary_lines).strip()[:900] + "\n\n" if glossary_lines else "")
                + ("Document summary (context only):\n" + str(ctx.summary).strip()[:900] + "\n\n" if ctx.summary else "")
                + "SOURCE:\n"
                + tu.frozen_surface
                + "\n\nDRAFT:\n"
                + draft
                + "\n"
            )

            try:
                out = agent.generate(fix_prompt, max_new_tokens=min(int(max_new_tokens or 1024), 768), do_sample=False)
            except Exception as exc:  # noqa: BLE001
                progress.info(f"Hard-failure repair TU#{tu.tu_id} failed: {type(exc).__name__}: {exc}")
                continue

            cand = decode_sentinels_from_model(out)
            cand, ws_flags = normalize_candidate_translation(tu, cand, source_lang=source_lang, target_lang=target_lang)
            if ws_flags:
                tu.ws_flags = sorted(set([*tu.ws_flags, *ws_flags]))
            new_issues, _new_proto = _eval(tu, cand, source_lang=source_lang, target_lang=target_lang, glossary_dict=glossary_dict)
            if hard_issues(new_issues):
                progress.info(
                    f"Hard-failure repair TU#{tu.tu_id} still has issues: "
                    + ",".join(hard_issues(new_issues)[:6])
                    + f" src={_preview_for_log(tu.source_surface, 120)}"
                )
                continue

            tu.final_translation = cand
            tu.qe_flags = new_issues
            if on_tu_revised:
                on_tu_revised(tu)
