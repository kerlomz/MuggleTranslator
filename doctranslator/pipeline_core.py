from __future__ import annotations

import os
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from lxml import etree

from doctranslator.context import build_document_context, extract_candidate_terms
from doctranslator.docx_package import parse_xml_part, read_docx, structure_hash, write_docx
from doctranslator.errors import ModelLoadError, TranslationProtocolError
from doctranslator.extract import extract_scopes_from_xml
from doctranslator.freezer import freeze_text
from doctranslator.hierarchy import ParagraphContext, build_paragraph_contexts
from doctranslator.ir import TranslationUnit
from doctranslator.legal_refs import _rewrite_nt_maps_for_target_lang
from doctranslator.models import ChatModel, TranslateGemmaModel
from doctranslator.progress import ConsoleProgress, NullProgress
from doctranslator.project import distribute_span_text_to_nodes, project_translation_to_spans
from doctranslator.quality import hard_issues
from doctranslator.review import final_review_and_repair, scan_hard_failures
from doctranslator.settings import Settings
from doctranslator.textutil import (
    _detect_language_pair_from_tus,
    _lang_prompt_name,
    _preview_for_log,
    _scope_type,
    _should_translate_tu,
)
from doctranslator.translate_agent import AgentContext, translate_units


_W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"


@dataclass
class DocxTranslationPipeline:
    settings: Settings
    progress: object | None = None

    def __post_init__(self) -> None:
        self._translategemma: TranslateGemmaModel | None = None
        self._gemma3_12b: ChatModel | None = None
        if self.progress is None:
            self._progress = ConsoleProgress(enabled=self.settings.progress)
        else:
            self._progress = self.progress
        if not hasattr(self._progress, "info") or not hasattr(self._progress, "progress"):
            self._progress = NullProgress()

    def _load_translate_model(self) -> TranslateGemmaModel:
        if self._translategemma is not None:
            return self._translategemma
        model_path = self.settings.translategemma_gguf
        if not model_path.exists():
            if self.settings.hy_mt_gguf and self.settings.hy_mt_gguf.exists():
                model_path = self.settings.hy_mt_gguf
            else:
                raise ModelLoadError(f"Translate model GGUF not found: {model_path}")

        is_hy_mt = "hy-mt" in model_path.name.lower() or (self.settings.llama_chat_format_translate or "").lower() == "hunyuan"
        n_ctx = self.settings.llama_n_ctx_hy_mt if is_hy_mt else self.settings.llama_n_ctx_translate
        chat_format = self.settings.llama_chat_format_translate
        if is_hy_mt and (not chat_format or str(chat_format).strip().lower() == "gemma"):
            chat_format = self.settings.llama_chat_format_hy_mt or chat_format
        self._translategemma = TranslateGemmaModel.load(
            model_path=model_path,
            n_ctx=n_ctx,
            n_threads=self.settings.llama_n_threads,
            n_gpu_layers=self.settings.llama_n_gpu_layers,
            seed=self.settings.llama_seed,
            verbose=self.settings.llama_verbose,
            chat_format=chat_format,
        )
        return self._translategemma

    def _load_agent_model(self) -> ChatModel | None:
        if self._gemma3_12b is not None:
            return self._gemma3_12b
        if not self.settings.gemma3_12b_gguf:
            return None
        if not self.settings.gemma3_12b_gguf.exists():
            return None
        self._gemma3_12b = ChatModel.load(
            model_path=self.settings.gemma3_12b_gguf,
            n_ctx=self.settings.llama_n_ctx_gemma,
            n_threads=self.settings.llama_n_threads,
            n_gpu_layers=self.settings.llama_n_gpu_layers,
            seed=self.settings.llama_seed,
            verbose=self.settings.llama_verbose,
            chat_format=self.settings.llama_chat_format_gemma,
        )
        return self._gemma3_12b

    def translate_file(self, input_path: Path, output_path: Path) -> None:
        p = self._progress
        p.info(f"Input docx: {input_path}")
        p.info(f"Output docx: {output_path}")

        with read_docx(input_path) as zin:
            p.info("Reading DOCX package (zip)")
            xml_entries = [i.filename for i in zin.infolist() if i.filename.lower().endswith(".xml")]
            p.info(f"XML parts: {len(xml_entries)}")

            parts: dict[str, etree._ElementTree] = {}
            standalone: dict[str, bool | None] = {}

            for i, name in enumerate(xml_entries, start=1):
                xml_bytes = zin.read(name)
                part = parse_xml_part(name, xml_bytes)
                parts[name] = part.tree
                standalone[name] = part.standalone
                p.progress("Parsing XML", i, len(xml_entries))

            text_qnames = {
                "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}t",
                "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}delText",
                "{http://schemas.openxmlformats.org/drawingml/2006/main}t",
            }
            attr_qnames: set[str] = {"{http://www.w3.org/XML/1998/namespace}space"}
            attr_pairs = {(f"{{{_W_NS}}}lvlText", f"{{{_W_NS}}}val")}

            baseline_hash: dict[str, str] = {}
            for i, (name, tree) in enumerate(parts.items(), start=1):
                baseline_hash[name] = structure_hash(
                    tree,
                    text_qnames=text_qnames,
                    attr_qnames=attr_qnames,
                    attr_pairs=attr_pairs,
                )
                p.progress("Hashing structure", i, len(parts))

            p.info("Extracting translation units")
            tus: list[TranslationUnit] = []
            next_tu_id = 1
            for i, (part_name, tree) in enumerate(parts.items(), start=1):
                for scope in extract_scopes_from_xml(part_name, tree):
                    if not scope.surface_text.strip():
                        continue
                    freeze = freeze_text(scope.surface_text)
                    tus.append(
                        TranslationUnit(
                            tu_id=next_tu_id,
                            part_name=part_name,
                            scope_key=scope.scope_key,
                            atoms=scope.atoms,
                            spans=scope.spans,
                            source_surface=scope.surface_text,
                            frozen_surface=freeze.text,
                            nt_map=freeze.nt_map,
                        )
                    )
                    next_tu_id += 1
                p.progress("Scanning XML", i, len(parts))

            if not tus:
                p.info("No translatable text found; writing output as-is")
                write_docx(zin, output_path, replacements={})
                return

            if self.settings.max_tus:
                tus = tus[: int(self.settings.max_tus)]

            counts = Counter([_scope_type(t.scope_key) for t in tus])
            if counts:
                p.info("TU breakdown: " + ", ".join([f"{k}={v}" for k, v in sorted(counts.items())]))
            top_parts = Counter([t.part_name for t in tus]).most_common(6)
            if top_parts:
                p.info("Top XML parts: " + ", ".join([f"{k}={v}" for k, v in top_parts]))

            for tu in tus[:8]:
                src_plain = tu.source_surface.replace("\r", "").replace("\n", "")
                p.info(
                    f"TU#{tu.tu_id} type={_scope_type(tu.scope_key)} part={tu.part_name} "
                    f"spans={len(tu.spans)} chars={len(src_plain)} nt={len(tu.nt_map)} "
                    f"text={_preview_for_log(tu.source_surface, self.settings.log_tu_max_chars)}"
                )

            longest = max(tus, key=lambda t: len(t.source_surface or ""), default=None)
            if longest is not None:
                p.info(
                    "Longest TU: "
                    f"TU#{longest.tu_id} chars={len(longest.source_surface)} "
                    f"type={_scope_type(longest.scope_key)} part={longest.part_name} "
                    f"text={_preview_for_log(longest.source_surface, 180)}"
                )

            styles_tree = parts.get("word/styles.xml")
            para_contexts: dict[int, ParagraphContext] | None = None
            try:
                para_contexts = build_paragraph_contexts(tus, styles_tree=styles_tree)
            except Exception as exc:  # noqa: BLE001
                p.info(f"Hierarchy build failed: {type(exc).__name__}: {exc}")
                para_contexts = None

            if para_contexts:
                paragraphs = sum(1 for t in tus if _scope_type(t.scope_key) == "w:p")
                headings = sum(1 for c in para_contexts.values() if c.is_heading)
                list_paragraphs = sum(1 for c in para_contexts.values() if c.list_level is not None)
                in_table = sum(1 for c in para_contexts.values() if c.in_table)
                p.info(
                    f"Hierarchy: paragraphs={paragraphs} headings={headings} list_paragraphs={list_paragraphs} in_table={in_table}"
                )

            forced_src = (self.settings.source_lang_code or "").strip()
            forced_tgt = (self.settings.target_lang_code or "").strip()
            if forced_src and forced_tgt:
                source_lang = forced_src
                target_lang = forced_tgt
                p.info(f"Language forced: {source_lang} -> {target_lang}")
            else:
                det = _detect_language_pair_from_tus(tus)
                if det is None:
                    p.info("Auto language detect: no en/zh signal; skipping translation and writing output as-is")
                    write_docx(zin, output_path, replacements={})
                    return
                source_lang, target_lang, detail = det
                p.info(detail)
                p.info(f"Language: {source_lang} -> {target_lang}")

            try:
                ref_changed = _rewrite_nt_maps_for_target_lang(tus, source_lang=source_lang, target_lang=target_lang)
                if ref_changed:
                    p.info(f"Locked legal references (NT remap): {ref_changed}")
            except Exception as exc:  # noqa: BLE001
                p.info(f"Legal reference remap failed: {type(exc).__name__}: {exc}")

            p.info(f"Translation units: {len(tus)}")
            p.info(
                "Config: "
                f"agent={'on' if self.settings.enable_decision else 'off'} "
                f"decision_min_chars={self.settings.decision_min_chars} "
                f"style_guide={'on' if self.settings.enable_style_guide else 'off'} "
                f"target_style={'override' if self.settings.target_style else 'auto'} "
                f"glossary_per_tu={self.settings.glossary_max_items_per_tu} "
                f"checkpoint_every={self.settings.checkpoint_every} "
                f"heartbeat={self.settings.heartbeat_seconds}s "
                f"log_tu_every={self.settings.log_tu_every}"
            )
            p.info(
                "LLM backend: llama.cpp "
                f"chat_format_translate={self.settings.llama_chat_format_translate or 'auto'} "
                f"n_ctx_translate={self.settings.llama_n_ctx_translate} "
                f"chat_format_gemma={self.settings.llama_chat_format_gemma or 'auto'} "
                f"n_ctx_gemma={self.settings.llama_n_ctx_gemma} "
                f"n_threads={self.settings.llama_n_threads} n_gpu_layers={self.settings.llama_n_gpu_layers}"
            )
            p.info(
                "Models configured: "
                f"TranslateModel={'yes' if (self.settings.translategemma_gguf.exists() or (self.settings.hy_mt_gguf and self.settings.hy_mt_gguf.exists())) else 'no'} "
                f"Gemma3-12B={'yes' if self.settings.gemma3_12b_gguf else 'no'}"
            )
            translate_name = (
                self.settings.translategemma_gguf.name
                if self.settings.translategemma_gguf.exists()
                else (self.settings.hy_mt_gguf.name if self.settings.hy_mt_gguf else "(missing)")
            )
            p.info(f"Workflow: translation model ({translate_name}) for translation; Gemma3-12B as the agent (context/QE/repair).")

            agent_model: ChatModel | None = None
            if self.settings.enable_decision or self.settings.enable_context:
                if self.settings.gemma3_12b_gguf:
                    p.info(f"Loading Gemma3-12B from: {self.settings.gemma3_12b_gguf}")
                agent_model = self._load_agent_model()
                if (self.settings.enable_decision or self.settings.enable_context) and agent_model is None:
                    raise ModelLoadError("Gemma3-12B is required but not configured (missing GGUF).")

            doc_context = None
            if self.settings.enable_context and agent_model is not None:
                p.info("Building document context (domain/style/glossary)")
                preferred = [
                    tu
                    for tu in tus
                    if tu.part_name.endswith("word/document.xml") and _scope_type(tu.scope_key) == "w:p"
                ]
                base = preferred if preferred else tus
                excerpts = [tu.source_surface for tu in base[: self.settings.context_max_excerpts]]
                term_candidates = extract_candidate_terms(excerpts, max_terms=self.settings.glossary_max_terms)
                if term_candidates:
                    p.info(f"Glossary candidates: {len(term_candidates)} (top={min(10, len(term_candidates))})")
                    for t in term_candidates[: min(10, len(term_candidates))]:
                        p.info(f"  - {t}")
                try:
                    doc_context = build_document_context(
                        excerpts=excerpts,
                        source_lang=_lang_prompt_name(source_lang),
                        target_lang=_lang_prompt_name(target_lang),
                        term_candidates=term_candidates,
                        gemma1b=None,
                        gemma4b=agent_model,
                        enable_style_guide=self.settings.enable_style_guide,
                        forced_target_style=self.settings.target_style,
                    )
                except Exception as exc:  # noqa: BLE001
                    p.info(f"Document context build failed: {type(exc).__name__}: {exc}")
                    doc_context = None

                if doc_context is not None:
                    p.info(
                        "Doc context: "
                        f"domain={doc_context.domain or ''} "
                        f"doc_type={doc_context.doc_type or ''} "
                        f"target_style={doc_context.target_style or ''}"
                    )
                    if doc_context.summary:
                        p.info(f"Doc summary: {_preview_for_log(doc_context.summary, self.settings.log_tu_max_chars)}")
                    if doc_context.glossary:
                        p.info(f"Glossary ready: {len(doc_context.glossary)} entries")
                        shown = 0
                        for k, v in doc_context.glossary.items():
                            p.info(f"  - {k} -> {v}")
                            shown += 1
                            if shown >= 10:
                                break

            ctx = AgentContext(
                domain=getattr(doc_context, "domain", None) if doc_context is not None else None,
                doc_type=getattr(doc_context, "doc_type", None) if doc_context is not None else None,
                summary=getattr(doc_context, "summary", None) if doc_context is not None else None,
                target_style=getattr(doc_context, "target_style", None) if doc_context is not None else None,
                style_guide=getattr(doc_context, "style_guide", None) if doc_context is not None else None,
                glossary=getattr(doc_context, "glossary", None) if doc_context is not None else None,
            )
            if self.settings.target_style and not ctx.target_style:
                ctx = AgentContext(
                    domain=ctx.domain,
                    doc_type=ctx.doc_type,
                    summary=ctx.summary,
                    target_style=self.settings.target_style,
                    style_guide=ctx.style_guide,
                    glossary=ctx.glossary,
                )

            checkpoint_modified_parts: set[str] = set()
            checkpoint_path = output_path.with_name(f"{output_path.stem}_进度{output_path.suffix}")
            checkpoint_seq = 0

            def apply_tu_translation_to_xml(tu: TranslationUnit) -> None:
                final = tu.final_translation or tu.draft_translation
                if final is None:
                    return
                span_slices = project_translation_to_spans(
                    spans=tu.spans,
                    source_surface=tu.frozen_surface,
                    target_surface=final,
                    nt_map=tu.nt_map,
                )
                for span_slice in span_slices:
                    for node_ref, node_text in distribute_span_text_to_nodes(span_slice.span, span_slice.text):
                        elem = node_ref.element
                        if node_ref.attr_name:
                            elem.set(node_ref.attr_name, node_text)
                        else:
                            elem.text = node_text
                            if node_text.startswith(" ") or node_text.endswith(" "):
                                elem.set("{http://www.w3.org/XML/1998/namespace}space", "preserve")
                        checkpoint_modified_parts.add(node_ref.part_name)

            def write_checkpoint(current: int, total: int, reason: str) -> None:
                nonlocal checkpoint_seq
                every = int(self.settings.checkpoint_every or 0)
                if every <= 0:
                    return
                if not checkpoint_modified_parts:
                    return
                if current != total and current % every != 0:
                    return
                checkpoint_seq += 1
                tmp = checkpoint_path.with_name(f"{checkpoint_path.stem}._tmp_{checkpoint_seq:04d}{checkpoint_path.suffix}")
                replacements: dict[str, bytes] = {}
                for name in sorted(checkpoint_modified_parts):
                    tree = parts[name]
                    if baseline_hash[name] != structure_hash(
                        tree, text_qnames=text_qnames, attr_qnames=attr_qnames, attr_pairs=attr_pairs
                    ):
                        raise TranslationProtocolError(f"Non-text structure changed in {name}")
                    root = tree.getroot()
                    replacements[name] = etree.tostring(
                        root,
                        encoding="UTF-8",
                        xml_declaration=True,
                        standalone=standalone.get(name),
                    )
                try:
                    write_docx(zin, tmp, replacements=replacements)
                    os.replace(tmp, checkpoint_path)
                    p.info(f"Checkpoint updated: {checkpoint_path} ({current}/{total}) reason={reason}")
                except PermissionError:
                    snap = checkpoint_path.with_name(f"{checkpoint_path.stem}_snap_{current:04d}{checkpoint_path.suffix}")
                    try:
                        os.replace(tmp, snap)
                        p.info(f"Checkpoint busy; wrote snapshot: {snap} ({current}/{total}) reason={reason}")
                    except Exception as exc:  # noqa: BLE001
                        try:
                            if tmp.exists():
                                p.info(
                                    f"Checkpoint busy; keeping temp snapshot: {tmp} ({current}/{total}) reason={reason} "
                                    f"err={type(exc).__name__}: {exc}"
                                )
                            else:
                                p.info(f"Checkpoint write failed: {type(exc).__name__}: {exc}")
                        except Exception:  # noqa: BLE001
                            p.info(f"Checkpoint write failed: {type(exc).__name__}: {exc}")
                except Exception as exc:  # noqa: BLE001
                    p.info(f"Checkpoint write failed: {type(exc).__name__}: {exc}")
                    try:
                        if tmp.exists():
                            tmp.unlink()
                    except Exception:  # noqa: BLE001
                        pass

            def on_tu_done(tu: TranslationUnit, idx: int, total: int) -> None:
                try:
                    apply_tu_translation_to_xml(tu)
                except Exception as exc:  # noqa: BLE001
                    p.info(f"Checkpoint apply failed TU#{tu.tu_id}: {type(exc).__name__}: {exc}")
                    return
                write_checkpoint(idx, total, reason="translate")

            p.info(f"Loading translation model from: {self.settings.translategemma_gguf}")
            translate_model = self._load_translate_model()
            p.info(
                "Translate model ready: "
                f"model={translate_model.model_path.name} backend=llama.cpp n_ctx={translate_model.n_ctx} "
                f"n_threads={translate_model.n_threads} n_gpu_layers={translate_model.n_gpu_layers}"
            )

            translate_units(
                progress=p,
                model=translate_model,
                agent=agent_model,
                tus=tus,
                source_lang=source_lang,
                target_lang=target_lang,
                ctx=ctx,
                para_contexts=para_contexts,
                enable_agent=bool(self.settings.enable_decision),
                decision_min_chars=int(self.settings.decision_min_chars or 0),
                heartbeat_seconds=float(self.settings.heartbeat_seconds or 0.0),
                max_input_tokens=int(self.settings.max_input_tokens or 1800),
                max_new_tokens=int(self.settings.max_new_tokens or 1024),
                glossary_max_items_per_tu=int(self.settings.glossary_max_items_per_tu or 0),
                log_tu_every=int(self.settings.log_tu_every or 20),
                on_tu_done=on_tu_done,
            )
            write_checkpoint(len(tus), len(tus), reason="translate")

            def on_tu_revised(tu: TranslationUnit) -> None:
                try:
                    apply_tu_translation_to_xml(tu)
                except Exception as exc:  # noqa: BLE001
                    p.info(f"Checkpoint apply failed (review) TU#{tu.tu_id}: {type(exc).__name__}: {exc}")

            final_review_and_repair(
                progress=p,
                agent=agent_model,
                tus=tus,
                source_lang=source_lang,
                target_lang=target_lang,
                ctx=ctx,
                para_contexts=para_contexts,
                decision_min_chars=int(self.settings.decision_min_chars or 0),
                glossary_max_items_per_tu=int(self.settings.glossary_max_items_per_tu or 0),
                max_new_tokens=int(self.settings.max_new_tokens or 1024),
                repair_rounds=int(self.settings.hard_failure_repair_rounds or 0),
                log_tu_every=int(self.settings.log_tu_every or 20),
                on_tu_revised=on_tu_revised,
            )

            failures = scan_hard_failures(tus=tus, source_lang=source_lang, target_lang=target_lang, ctx=ctx)
            if failures:
                p.info(f"Validation still reports hard failures: {len(failures)} (writing best-effort output)")
                for item in failures[:12]:
                    tu = item.tu
                    p.info(
                        f"  - TU#{tu.tu_id} part={tu.part_name} type={_scope_type(tu.scope_key)} "
                        f"skip_reason={item.skip_reason} issues={','.join(hard_issues(item.issues)[:6])} "
                        f"protocol={item.protocol_error or '(none)'}"
                    )
                write_checkpoint(len(tus), len(tus), reason="validate_fail")
            else:
                write_checkpoint(len(tus), len(tus), reason="validate_ok")

            p.info("Projecting translations back into XML (format-preserving)")
            modified_parts: set[str] = set()
            for i, tu in enumerate(tus, start=1):
                final = tu.final_translation or tu.draft_translation
                if final is None:
                    continue
                span_slices = project_translation_to_spans(
                    spans=tu.spans,
                    source_surface=tu.frozen_surface,
                    target_surface=final,
                    nt_map=tu.nt_map,
                )
                for span_slice in span_slices:
                    for node_ref, node_text in distribute_span_text_to_nodes(span_slice.span, span_slice.text):
                        elem = node_ref.element
                        if node_ref.attr_name:
                            elem.set(node_ref.attr_name, node_text)
                        else:
                            elem.text = node_text
                            if node_text.startswith(" ") or node_text.endswith(" "):
                                elem.set("{http://www.w3.org/XML/1998/namespace}space", "preserve")
                        modified_parts.add(node_ref.part_name)
                if i == len(tus) or i % 50 == 0:
                    p.progress("Writing text nodes", i, len(tus))

            p.info("Verifying non-text structure unchanged")
            replacements: dict[str, bytes] = {}
            for i, name in enumerate(sorted(modified_parts), start=1):
                tree = parts[name]
                if baseline_hash[name] != structure_hash(tree, text_qnames=text_qnames, attr_qnames=attr_qnames, attr_pairs=attr_pairs):
                    raise TranslationProtocolError(f"Non-text structure changed in {name}")
                root = tree.getroot()
                replacements[name] = etree.tostring(
                    root,
                    encoding="UTF-8",
                    xml_declaration=True,
                    standalone=standalone.get(name),
                )
                p.progress("Serializing XML", i, max(len(modified_parts), 1))

            p.info("Writing output DOCX")
            if output_path.exists():
                p.info(f"Overwriting existing output: {output_path}")
            write_docx(zin, output_path, replacements=replacements)
            p.info(f"Done: {output_path}")
