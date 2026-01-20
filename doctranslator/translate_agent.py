from __future__ import annotations
import threading
import time
from collections import Counter
from dataclasses import dataclass
from typing import Callable

import re

from doctranslator.chunking import _detect_stitch_duplicate_chunks, _split_text_to_fit_tokens
from doctranslator.hierarchy import ParagraphContext
from doctranslator.ir import TranslationUnit
from doctranslator.models import ChatModel, TranslateGemmaModel
from doctranslator.protocol import normalize_candidate_translation, _split_by_sentinels, _split_edge_ws, _validate_sentinels
from doctranslator.quality import hard_issues, quality_issues
from doctranslator.sentinels import ANY_SENTINEL_RE, decode_sentinels_from_model
from doctranslator.textutil import (
    _lang_prompt_name,
    _lang_prompt_native,
    _preview_for_log,
    _scope_type,
    _should_translate_tu,
    _try_extract_json_obj,
    number_tokens_in_text,
)


@dataclass(frozen=True)
class AgentContext:
    domain: str | None = None
    doc_type: str | None = None
    summary: str | None = None
    target_style: str | None = None
    style_guide: str | None = None
    glossary: dict[str, str] | None = None


def _expand_counter(c: Counter[str]) -> list[str]:
    out: list[str] = []
    for k, v in c.items():
        for _ in range(int(v)):
            out.append(str(k))
    return out


def _run_with_heartbeat(
    *,
    progress: object,
    label: str,
    heartbeat_seconds: float,
    fn: Callable[[], str],
) -> str:
    done = threading.Event()
    started = time.time()

    def worker() -> None:
        while not done.wait(max(0.1, float(heartbeat_seconds))):
            try:
                elapsed = time.time() - started
                progress.info(f"{label} running... elapsed={elapsed:.1f}s")
            except Exception:  # noqa: BLE001
                return

    t: threading.Thread | None = None
    if heartbeat_seconds and heartbeat_seconds > 0:
        t = threading.Thread(target=worker, daemon=True)
        t.start()
    try:
        return fn()
    finally:
        done.set()
        if t is not None:
            t.join(timeout=0.05)


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
        if any(ch.isalpha() for ch in ks):
            if ks and (re.search(re.escape(ks), src_plain, flags=re.IGNORECASE) is None):
                continue
        else:
            if ks not in src_plain:
                continue
        matched.append((ks, vs))
        if len(matched) >= max_items:
            break
    if not matched:
        return None
    return "\n".join([f"- {k} -> {v}" for k, v in matched])


def _neighbor_texts(tus: list[TranslationUnit], idx0: int) -> tuple[str | None, str | None]:
    prev_text = tus[idx0 - 1].source_surface if idx0 - 1 >= 0 else None
    next_text = tus[idx0 + 1].source_surface if idx0 + 1 < len(tus) else None
    return (prev_text, next_text)


def _max_new_for_tokens(src_tokens: int, cap: int) -> int:
    cap = int(cap)
    if cap <= 0:
        cap = 1024
    if src_tokens <= 16:
        return min(cap, 64)
    if src_tokens <= 64:
        return min(cap, 128)
    if src_tokens <= 160:
        return min(cap, 256)
    if src_tokens <= 320:
        return min(cap, 384)
    if src_tokens <= 640:
        return min(cap, 512)
    return min(cap, 768)


def _agent_instruction_json(
    *,
    agent: ChatModel,
    src_name: str,
    tgt_name: str,
    tgt_native: str,
    seg_src: str,
    bad_out: str,
    issues: list[str],
    protocol_error: str | None,
    ctx: AgentContext,
    struct_hint: str | None,
    neighbor_prev: str | None,
    neighbor_next: str | None,
    glossary_lines: str | None,
    max_new_tokens: int,
) -> str | None:
    ctx_lines: list[str] = []
    if ctx.domain:
        ctx_lines.append(f"domain={ctx.domain}")
    if ctx.doc_type:
        ctx_lines.append(f"doc_type={ctx.doc_type}")
    if ctx.target_style:
        ctx_lines.append(f"target_style={ctx.target_style}")
    ctx_block = ("Document context: " + " | ".join(ctx_lines) + "\n\n") if ctx_lines else ""
    if ctx.style_guide:
        ctx_block += "Style guide (must follow):\n" + str(ctx.style_guide).strip()[:800] + "\n\n"
    if glossary_lines:
        ctx_block += "Glossary (must follow):\n" + str(glossary_lines).strip()[:800] + "\n\n"
    if ctx.summary:
        ctx_block += "Document summary (context only):\n" + str(ctx.summary).strip()[:800] + "\n\n"
    if struct_hint:
        ctx_block += "Structure hints (context only):\n" + str(struct_hint).strip()[:600] + "\n\n"
    if neighbor_prev:
        ctx_block += "Prev source paragraph (context only):\n" + str(neighbor_prev).strip()[:420] + "\n\n"
    if neighbor_next:
        ctx_block += "Next source paragraph (context only):\n" + str(neighbor_next).strip()[:420] + "\n\n"

    prompt = (
        f"You are a {src_name} to {tgt_name} translation pipeline agent.\n"
        "Write ONE targeted instruction for the translation model to fix the failure.\n"
        "Return STRICT JSON only.\n"
        'Schema: {"instruction": "..."}\n\n'
        "Hard constraints:\n"
        f"- Output language must be {tgt_native}.\n"
        "- Output only the translation (no labels/metadata).\n"
        "- Do NOT omit any content; do NOT summarize; do NOT output partial translations.\n"
        "- Do NOT add any new information; do NOT expand.\n"
        "- Do NOT introduce any new conditions/limitations/exceptions that are not in the source.\n"
        "- Preserve all placeholder tokens enclosed in ⟦...⟧ exactly; do not add/remove/reorder.\n\n"
        + (f"protocol_error: {protocol_error}\n" if protocol_error else "")
        + ("issues: " + ", ".join(issues) + "\n\n" if issues else "\n")
        + ctx_block
        + "SOURCE:\n"
        + seg_src
        + "\n\nBAD_OUTPUT:\n"
        + bad_out
        + "\n"
    )
    out = agent.generate(prompt, max_new_tokens=max_new_tokens, do_sample=False)
    data = _try_extract_json_obj(out)
    if not isinstance(data, dict):
        return None
    instr = data.get("instruction")
    if not isinstance(instr, str):
        return None
    instr = instr.strip()
    return instr or None


def _agent_translate_plain(
    *,
    agent: ChatModel,
    src_name: str,
    tgt_name: str,
    tgt_native: str,
    text: str,
    ctx: AgentContext,
    struct_hint: str | None,
    neighbor_prev: str | None,
    neighbor_next: str | None,
    glossary_lines: str | None,
    max_new_tokens: int,
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
        ctx_block += "Style guide (must follow):\n" + str(ctx.style_guide).strip()[:800] + "\n\n"
    if glossary_lines:
        ctx_block += "Glossary (must follow):\n" + str(glossary_lines).strip()[:800] + "\n\n"
    if ctx.summary:
        ctx_block += "Document summary (context only):\n" + str(ctx.summary).strip()[:800] + "\n\n"
    if struct_hint:
        ctx_block += "Structure hints (context only):\n" + str(struct_hint).strip()[:600] + "\n\n"
    if neighbor_prev:
        ctx_block += "Prev source paragraph (context only):\n" + str(neighbor_prev).strip()[:420] + "\n\n"
    if neighbor_next:
        ctx_block += "Next source paragraph (context only):\n" + str(neighbor_next).strip()[:420] + "\n\n"

    nums = _expand_counter(number_tokens_in_text(text))
    nums_hint = ", ".join(nums) if nums else "(none)"

    prompt = (
        f"You are a professional {src_name} to {tgt_name} translator.\n"
        f"Translate the TEXT from {src_name} to {tgt_native}.\n"
        "Output ONLY the translation.\n\n"
        "Constraints:\n"
        "- Do NOT omit any content; do NOT summarize.\n"
        "- Do NOT add new information; do NOT expand.\n"
        "- Do NOT output any labels/metadata.\n"
        "- Preserve any scripts not in the target language exactly if they appear in SOURCE.\n"
        f"- Must preserve these digits as digits with exact counts: {nums_hint}\n\n"
        + ctx_block
        + "TEXT:\n"
        + text
        + "\n"
    )
    out = agent.generate(prompt, max_new_tokens=max_new_tokens, do_sample=False)
    return out


def _agent_translate_tu_skeleton(
    *,
    progress: object,
    agent: ChatModel,
    tu: TranslationUnit,
    source_lang: str,
    target_lang: str,
    ctx: AgentContext,
    para_context: ParagraphContext | None,
    neighbor_prev: str | None,
    neighbor_next: str | None,
    glossary_lines: str | None,
    heartbeat_seconds: float,
    max_input_tokens: int,
    max_new_tokens_cap: int,
) -> str:
    src_name = _lang_prompt_name(source_lang)
    tgt_name = _lang_prompt_name(target_lang)
    tgt_native = _lang_prompt_native(target_lang)
    struct_hint = para_context.format_for_prompt() if para_context is not None else None

    max_seg_tokens = int(max_input_tokens or 0)
    if max_seg_tokens <= 0:
        max_seg_tokens = 1800
    max_seg_tokens = max(256, min(max_seg_tokens, max(256, int(agent.n_ctx) - 700)))

    parts = _split_by_sentinels(tu.frozen_surface)
    out_parts: list[str] = []
    for part in parts:
        if ANY_SENTINEL_RE.fullmatch(part) is not None:
            out_parts.append(part)
            continue
        pre, core, suf = _split_edge_ws(part)
        if not core.strip():
            out_parts.append(part)
            continue

        chunks = _split_text_to_fit_tokens(core, count_tokens=agent.count_tokens, max_tokens=max_seg_tokens)
        out_chunks: list[str] = []
        for ch in chunks:
            if not ch.strip():
                out_chunks.append(ch)
                continue
            src_tokens = agent.count_tokens(ch) or 0
            seg_max_new = _max_new_for_tokens(src_tokens, max_new_tokens_cap)

            def do_translate() -> str:
                return _agent_translate_plain(
                    agent=agent,
                    src_name=src_name,
                    tgt_name=tgt_name,
                    tgt_native=tgt_native,
                    text=ch,
                    ctx=ctx,
                    struct_hint=struct_hint,
                    neighbor_prev=neighbor_prev,
                    neighbor_next=neighbor_next,
                    glossary_lines=glossary_lines,
                    max_new_tokens=seg_max_new,
                )

            raw = _run_with_heartbeat(
                progress=progress,
                label=f"Agent TU#{tu.tu_id} seg",
                heartbeat_seconds=heartbeat_seconds,
                fn=do_translate,
            )
            raw = decode_sentinels_from_model(raw)
            out_chunks.append(raw)

        out_parts.append(pre + "".join(out_chunks) + suf)

    stitched = decode_sentinels_from_model("".join(out_parts))
    normalized, _ws = normalize_candidate_translation(tu, stitched, source_lang=source_lang, target_lang=target_lang)
    return normalized


def _translate_tu_skeleton(
    *,
    progress: object,
    model: TranslateGemmaModel,
    agent: ChatModel | None,
    tu: TranslationUnit,
    idx0: int,
    tus: list[TranslationUnit],
    source_lang: str,
    target_lang: str,
    ctx: AgentContext,
    para_context: ParagraphContext | None,
    heartbeat_seconds: float,
    max_input_tokens: int,
    max_new_tokens_cap: int,
    glossary_max_items_per_tu: int,
) -> tuple[str, list[str], str | None]:
    src_name = _lang_prompt_name(source_lang)
    tgt_name = _lang_prompt_name(target_lang)
    tgt_native = _lang_prompt_native(target_lang)

    struct_hint = para_context.format_for_prompt() if para_context is not None else None
    neighbor_prev, neighbor_next = _neighbor_texts(tus, idx0)
    glossary_lines = _local_glossary_lines(ctx.glossary, text=tu.source_surface, max_items=glossary_max_items_per_tu)

    # Token-preserving skeleton: translate only plain segments between sentinels, keep sentinels unchanged.
    parts = _split_by_sentinels(tu.frozen_surface)
    out_parts: list[str] = []
    chunk_src: list[str] = []
    chunk_out: list[str] = []

    max_seg_tokens = int(max_input_tokens or 0)
    if max_seg_tokens <= 0:
        max_seg_tokens = 1800
    # Leave headroom for prompt and output.
    max_seg_tokens = max(256, min(max_seg_tokens, max(256, int(model.n_ctx) - 600)))

    for part in parts:
        if ANY_SENTINEL_RE.fullmatch(part) is not None:
            out_parts.append(part)
            continue

        pre, core, suf = _split_edge_ws(part)
        if not core.strip():
            out_parts.append(part)
            continue

        chunks = _split_text_to_fit_tokens(core, count_tokens=model.count_tokens, max_tokens=max_seg_tokens)
        chunk_src.extend(chunks)
        out_chunks: list[str] = []
        for ch in chunks:
            if not ch.strip():
                out_chunks.append(ch)
                continue

            src_tokens = model.count_tokens(ch) or 0
            seg_max_new = _max_new_for_tokens(src_tokens, max_new_tokens_cap)
            label = f"TG TU#{tu.tu_id} seg"

            def do_translate() -> str:
                required_numbers = _expand_counter(number_tokens_in_text(ch))
                return model.translate_text(
                    text=ch,
                    source_lang_code=source_lang,
                    target_lang_code=target_lang,
                    max_new_tokens=seg_max_new,
                    domain=ctx.domain,
                    doc_type=ctx.doc_type,
                    doc_summary=ctx.summary,
                    target_style=ctx.target_style,
                    style_guide=ctx.style_guide,
                    glossary=glossary_lines,
                    structure_hint=struct_hint,
                    neighbor_prev=neighbor_prev,
                    neighbor_next=neighbor_next,
                    retrieved_context=None,
                    agent_instruction=None,
                    required_numbers=required_numbers,
                )

            raw = _run_with_heartbeat(
                progress=progress,
                label=f"{label} {tu.tu_id}/{len(tus)}",
                heartbeat_seconds=heartbeat_seconds,
                fn=do_translate,
            )
            raw = decode_sentinels_from_model(raw)
            out_chunks.append(raw)

        out_core = "".join(out_chunks)
        chunk_out.extend(out_chunks)
        out_parts.append(pre + out_core + suf)

    stitched = "".join(out_parts)
    stitched = decode_sentinels_from_model(stitched)
    normalized, ws_flags = normalize_candidate_translation(tu, stitched, source_lang=source_lang, target_lang=target_lang)

    protocol_error: str | None = None
    issues: list[str] = []
    try:
        _validate_sentinels(tu, normalized)
    except Exception as exc:  # noqa: BLE001
        protocol_error = f"{type(exc).__name__}: {exc}"
        issues.append("protocol_error")

    issues.extend(
        quality_issues(tu, normalized, source_lang=source_lang, target_lang=target_lang, glossary_dict=ctx.glossary)
    )

    if _detect_stitch_duplicate_chunks(chunk_src, chunk_out):
        issues.append("stitch_duplicate_chunk")

    if ws_flags:
        tu.ws_flags = sorted(set([*tu.ws_flags, *ws_flags]))

    return (normalized, sorted(set(issues)), protocol_error)


def translate_units(
    *,
    progress: object,
    model: TranslateGemmaModel,
    agent: ChatModel | None,
    tus: list[TranslationUnit],
    source_lang: str,
    target_lang: str,
    ctx: AgentContext,
    para_contexts: dict[int, ParagraphContext] | None,
    enable_agent: bool,
    decision_min_chars: int,
    heartbeat_seconds: float,
    max_input_tokens: int,
    max_new_tokens: int,
    glossary_max_items_per_tu: int,
    log_tu_every: int,
    on_tu_done: Callable[[TranslationUnit, int, int], None] | None = None,
) -> None:
    total = len(tus)
    progress.progress("Translating (TranslateGemma)", 0, max(total, 1))

    for i, tu in enumerate(tus, start=1):
        should_translate, skip_reason = _should_translate_tu(tu, source_lang=source_lang)
        if not should_translate:
            tu.draft_translation = tu.frozen_surface
            tu.draft_translation_model = "skip"
            tu.final_translation = tu.draft_translation
            tu.qe_flags = []
            if i <= 5 or i % max(1, int(log_tu_every)) == 0:
                progress.info(
                    f"TG TU#{tu.tu_id} skipped: reason={skip_reason} type={_scope_type(tu.scope_key)} part={tu.part_name}"
                )
            progress.progress("Translating (TranslateGemma)", i, max(total, 1))
            if on_tu_done:
                on_tu_done(tu, i, total)
            continue

        para_ctx = para_contexts.get(tu.tu_id) if para_contexts is not None else None
        src_plain = ANY_SENTINEL_RE.sub(" ", tu.source_surface)
        src_chars = len(src_plain)

        if i <= 8 or i % max(1, int(log_tu_every)) == 0:
            progress.info(
                f"TG TU#{tu.tu_id} {i}/{total} type={_scope_type(tu.scope_key)} part={tu.part_name} src_chars={src_chars}"
            )
            if i <= 8:
                progress.info(f"TG TU#{tu.tu_id} src: {_preview_for_log(tu.source_surface, 180)}")

        out_text, issues, protocol_error = _translate_tu_skeleton(
            progress=progress,
            model=model,
            agent=agent,
            tu=tu,
            idx0=i - 1,
            tus=tus,
            source_lang=source_lang,
            target_lang=target_lang,
            ctx=ctx,
            para_context=para_ctx,
            heartbeat_seconds=heartbeat_seconds,
            max_input_tokens=max_input_tokens,
            max_new_tokens_cap=max_new_tokens,
            glossary_max_items_per_tu=glossary_max_items_per_tu,
        )

        tu.draft_translation = out_text
        tu.draft_translation_model = "translategemma"
        tu.qe_flags = issues

        needs_agent = enable_agent and (src_chars >= max(0, int(decision_min_chars)))
        if hard_issues(issues):
            needs_agent = True

        if needs_agent and agent is not None:
            progress.info(
                f"Decision gate TU#{tu.tu_id}: triggers="
                + ",".join([x for x in (hard_issues(issues) or issues)][:6])
            )
            fixed = _attempt_agent_repairs(
                progress=progress,
                model=model,
                agent=agent,
                tu=tu,
                idx0=i - 1,
                tus=tus,
                source_lang=source_lang,
                target_lang=target_lang,
                ctx=ctx,
                para_ctx=para_ctx,
                heartbeat_seconds=heartbeat_seconds,
                max_input_tokens=max_input_tokens,
                max_new_tokens=max_new_tokens,
                glossary_max_items_per_tu=glossary_max_items_per_tu,
                initial_bad=out_text,
                initial_issues=issues,
                initial_protocol_error=protocol_error,
            )
            tu.draft_translation = fixed.text
            tu.draft_translation_model = fixed.model_label
            tu.qe_flags = fixed.issues

        tu.final_translation = tu.draft_translation
        progress.progress("Translating (TranslateGemma)", i, max(total, 1))
        if on_tu_done:
            on_tu_done(tu, i, total)


@dataclass(frozen=True)
class AgentRepairResult:
    text: str
    issues: list[str]
    protocol_error: str | None
    model_label: str


def _eval_candidate(
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


def _attempt_agent_repairs(
    *,
    progress: object,
    model: TranslateGemmaModel,
    agent: ChatModel,
    tu: TranslationUnit,
    idx0: int,
    tus: list[TranslationUnit],
    source_lang: str,
    target_lang: str,
    ctx: AgentContext,
    para_ctx: ParagraphContext | None,
    heartbeat_seconds: float,
    max_input_tokens: int,
    max_new_tokens: int,
    glossary_max_items_per_tu: int,
    initial_bad: str,
    initial_issues: list[str],
    initial_protocol_error: str | None,
) -> AgentRepairResult:
    src_name = _lang_prompt_name(source_lang)
    tgt_name = _lang_prompt_name(target_lang)
    tgt_native = _lang_prompt_native(target_lang)

    struct_hint = para_ctx.format_for_prompt() if para_ctx is not None else None
    neighbor_prev, neighbor_next = _neighbor_texts(tus, idx0)
    glossary_lines = _local_glossary_lines(ctx.glossary, text=tu.source_surface, max_items=glossary_max_items_per_tu)

    # 1) Agent produces a targeted instruction, we retry TranslateGemma once.
    instr = _agent_instruction_json(
        agent=agent,
        src_name=src_name,
        tgt_name=tgt_name,
        tgt_native=tgt_native,
        seg_src=tu.frozen_surface,
        bad_out=initial_bad,
        issues=initial_issues,
        protocol_error=initial_protocol_error,
        ctx=ctx,
        struct_hint=struct_hint,
        neighbor_prev=neighbor_prev,
        neighbor_next=neighbor_next,
        glossary_lines=glossary_lines,
        max_new_tokens=min(int(max_new_tokens or 1024), 256),
    )

    if instr:
        required_numbers = _expand_counter(number_tokens_in_text(tu.source_surface))
        try:
            raw = _run_with_heartbeat(
                progress=progress,
                label=f"TG TU#{tu.tu_id} agent-instr",
                heartbeat_seconds=heartbeat_seconds,
                fn=lambda: model.translate_text(
                    text=tu.frozen_surface,
                    source_lang_code=source_lang,
                    target_lang_code=target_lang,
                    max_new_tokens=min(int(max_new_tokens or 1024), 512),
                    domain=ctx.domain,
                    doc_type=ctx.doc_type,
                    doc_summary=ctx.summary,
                    target_style=ctx.target_style,
                    style_guide=ctx.style_guide,
                    glossary=glossary_lines,
                    structure_hint=struct_hint,
                    neighbor_prev=neighbor_prev,
                    neighbor_next=neighbor_next,
                    retrieved_context=None,
                    agent_instruction=instr,
                    required_numbers=required_numbers,
                ),
            )
            raw = decode_sentinels_from_model(raw)
            cand, ws_flags = normalize_candidate_translation(
                tu, raw, source_lang=source_lang, target_lang=target_lang
            )
            if ws_flags:
                tu.ws_flags = sorted(set([*tu.ws_flags, *ws_flags]))
            issues2, proto2 = _eval_candidate(
                tu, cand, source_lang=source_lang, target_lang=target_lang, glossary_dict=ctx.glossary
            )
            if not hard_issues(issues2):
                return AgentRepairResult(text=cand, issues=issues2, protocol_error=proto2, model_label="tg+agent_instr")
        except Exception as exc:  # noqa: BLE001
            progress.info(f"TG TU#{tu.tu_id} agent-instr retry failed: {type(exc).__name__}: {exc}")

    # 2) Agent translates the full TU (chunked by sentinels).
    try:
        cand2 = _agent_translate_tu_skeleton(
            progress=progress,
            agent=agent,
            tu=tu,
            source_lang=source_lang,
            target_lang=target_lang,
            ctx=ctx,
            para_context=para_ctx,
            neighbor_prev=neighbor_prev,
            neighbor_next=neighbor_next,
            glossary_lines=glossary_lines,
            heartbeat_seconds=heartbeat_seconds,
            max_input_tokens=max_input_tokens,
            max_new_tokens_cap=min(int(max_new_tokens or 1024), 768),
        )
        _cand2, ws_flags2 = normalize_candidate_translation(tu, cand2, source_lang=source_lang, target_lang=target_lang)
        cand2 = _cand2
        if ws_flags2:
            tu.ws_flags = sorted(set([*tu.ws_flags, *ws_flags2]))
        issues3, proto3 = _eval_candidate(tu, cand2, source_lang=source_lang, target_lang=target_lang, glossary_dict=ctx.glossary)
        return AgentRepairResult(text=cand2, issues=issues3, protocol_error=proto3, model_label="agent_direct")
    except Exception as exc:  # noqa: BLE001
        progress.info(f"Agent direct translation failed TU#{tu.tu_id}: {type(exc).__name__}: {exc}")

    # 3) Last resort: keep the best available draft (never crash).
    return AgentRepairResult(text=initial_bad, issues=initial_issues, protocol_error=initial_protocol_error, model_label="keep_bad")
