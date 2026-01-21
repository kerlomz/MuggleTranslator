use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context};

use crate::docx::decompose::{
    extract_mask_json_and_offsets, merge_mask_json_and_offsets, OffsetsJson,
};
use crate::docx::filter::{filter_docx_with_rules, DocxFilterRules};
use crate::docx::pure_text::{extract_pure_text, PureTextJson};
use crate::docx::structure::extract_structure_json;
use crate::freezer::{freeze_text, normalize_nt_tokens, render_nt_map_for_prompt, unfreeze_text};
use crate::ir::TranslationUnit;
use crate::models::native::NativeChatModel;
use crate::quality::{quality_heuristics, validate_translation};
use crate::sentinels::{parse_segmented_output, seg_end, seg_start, ANY_SENTINEL_RE};
use crate::textutil::{auto_language_pair, is_trivial_sentinel_text, lang_label};

use super::super::docmap::build_para_slot_units;
use super::super::memory::{build_memory, write_memory_file, ParaNotes};

use super::{cleanup_model_text, load_model, render_template, TranslatorPipeline};

impl TranslatorPipeline {
    pub(super) fn translate_docx_basic(&mut self, input: &Path, output: &Path) -> anyhow::Result<()> {
        self.progress
            .info(format!("Pipeline mode: basic (translate_backend only)"));
        self.progress
            .info(format!("Read DOCX: {}", input.display()));
        fs::create_dir_all(self.trace.dir())
            .with_context(|| format!("create trace dir: {}", self.trace.dir().display()))?;

        let stem = output
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");

        let mut work_docx = input.to_path_buf();
        if let Some(rules_path) = self.cfg.docx_filter_rules.clone() {
            self.progress
                .info(format!("DOCX filter rules: {}", rules_path.display()));
            let rules = DocxFilterRules::from_toml_path(&rules_path)?;
            let filtered = self.trace.dir().join(format!("{stem}.filtered.docx"));
            filter_docx_with_rules(input, &filtered, &rules)?;
            work_docx = filtered;
        }

        let mask_json = self.trace.dir().join(format!("{stem}.mask.json"));
        let offsets_json = self.trace.dir().join(format!("{stem}.offsets.json"));
        let blobs_bin = self.trace.dir().join(format!("{stem}.mask.blobs.bin"));
        let text_source_json = self.trace.dir().join(format!("{stem}.source.text.json"));
        let structure_json = self.trace.dir().join(format!("{stem}.structure.json"));
        let autosave_text_json = self.trace.dir().join(format!("{stem}.autosave.text.json"));

        let source_text = extract_pure_text(&work_docx)?;
        fs::write(
            &text_source_json,
            serde_json::to_vec_pretty(&source_text).context("serialize source text json")?,
        )
        .with_context(|| format!("write source text json: {}", text_source_json.display()))?;
        let _ = extract_structure_json(&work_docx, &structure_json);
        extract_mask_json_and_offsets(&work_docx, &mask_json, &offsets_json, &blobs_bin)?;

        let offsets: OffsetsJson = serde_json::from_slice(
            &fs::read(&offsets_json)
                .with_context(|| format!("read offsets json: {}", offsets_json.display()))?,
        )
        .context("parse offsets json")?;

        let mut para_units = build_para_slot_units(&work_docx, &source_text, &offsets)?;
        if let Some(max_tus) = self.cfg.max_tus {
            let keep = max_tus.max(1).min(para_units.len());
            para_units.truncate(keep);
            self.progress.info(format!("Max TUs: {keep}"));
        }

        let (source_lang, target_lang) = self.resolve_lang_pair_from_pure_text(&source_text);
        self.progress
            .info(format!("Language: {source_lang} -> {target_lang}"));

        let translate_backend = self.cfg.translate_backend.clone();
        self.progress
            .info(format!("Translate backend: {}", translate_backend.name));
        let mut model = load_model(&self.cfg, &translate_backend)?;
        let prompt_translate_a = self.cfg.prompts.translate_a.clone();
        let prompt_translate_repair = self.cfg.prompts.translate_repair.clone();

        // A: translate slot_texts (used to render the output DOCX)
        let mut ordered_slot_ids: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        for u in &para_units {
            for &slot_id in &u.slot_ids {
                if slot_id == 0 {
                    continue;
                }
                if seen.insert(slot_id) {
                    ordered_slot_ids.push(slot_id);
                }
            }
        }
        self.progress
            .info(format!("Translatable slots: {}", ordered_slot_ids.len()));

        let mut tus_slots: Vec<TranslationUnit> = Vec::with_capacity(ordered_slot_ids.len());
        for &slot_id in &ordered_slot_ids {
            let idx = slot_id.saturating_sub(1);
            let src = source_text
                .slot_texts
                .get(idx)
                .cloned()
                .ok_or_else(|| anyhow!("slot_id_out_of_range: {slot_id}"))?;
            let fr = freeze_text(&src);
            tus_slots.push(TranslationUnit {
                tu_id: slot_id,
                part_name: String::new(),
                scope_key: format!("slot#{slot_id}"),
                para_style: None,
                atoms: Vec::new(),
                spans: Vec::new(),
                source_surface: src,
                frozen_surface: fr.text,
                nt_map: fr.nt_map,
                nt_mask: fr.mask,
                draft_translation: None,
                final_translation: None,
                alt_translation: None,
                draft_translation_model: None,
                alt_translation_model: None,
                qe_score: None,
                qe_flags: Vec::new(),
            });
        }

        let mut text_a: PureTextJson = source_text.clone();
        self.translate_slot_texts_segmented_basic(
            &mut model,
            &translate_backend,
            &source_lang,
            &target_lang,
            "translate_a(slot_texts)",
            &prompt_translate_a,
            &prompt_translate_repair,
            &mut tus_slots,
            &mut text_a,
            &mask_json,
            &offsets_json,
            &autosave_text_json,
            output,
        )?;

        let a_text_json_trace = self.trace.dir().join(format!("{stem}.A.text.json"));
        fs::write(
            &a_text_json_trace,
            serde_json::to_vec_pretty(&text_a).context("serialize A text json")?,
        )
        .with_context(|| format!("write A text json: {}", a_text_json_trace.display()))?;
        let a_text_json = output.with_extension("text.json");
        fs::write(
            &a_text_json,
            serde_json::to_vec_pretty(&text_a).context("serialize output text json")?,
        )
        .with_context(|| format!("write output text json: {}", a_text_json.display()))?;

        self.progress
            .info(format!("Write output: {}", output.display()));
        merge_mask_json_and_offsets(&mask_json, &offsets_json, &a_text_json, output)?;

        // B: translate paragraphs for review (not used for DOCX merge)
        let mut para_idx_by_id: HashMap<usize, usize> = HashMap::new();
        let mut tus_paras: Vec<TranslationUnit> = Vec::with_capacity(source_text.paragraphs.len());
        for (idx, p) in source_text.paragraphs.iter().enumerate() {
            para_idx_by_id.insert(p.para_id, idx);
            let fr = freeze_text(&p.text);
            tus_paras.push(TranslationUnit {
                tu_id: p.para_id,
                part_name: p.part_name.clone(),
                scope_key: p.scope_key.clone(),
                para_style: p.p_style.clone(),
                atoms: Vec::new(),
                spans: Vec::new(),
                source_surface: p.text.clone(),
                frozen_surface: fr.text,
                nt_map: fr.nt_map,
                nt_mask: fr.mask,
                draft_translation: None,
                final_translation: None,
                alt_translation: None,
                draft_translation_model: None,
                alt_translation_model: None,
                qe_score: None,
                qe_flags: Vec::new(),
            });
        }
        if let Some(max_tus) = self.cfg.max_tus {
            let keep = max_tus.max(1).min(tus_paras.len());
            tus_paras.truncate(keep);
        }
        let mut text_b: PureTextJson = source_text.clone();
        self.translate_units_segmented_basic(
            &mut model,
            &translate_backend,
            &source_lang,
            &target_lang,
            "translate_b(paragraphs)",
            &prompt_translate_a,
            &prompt_translate_repair,
            &mut tus_paras,
            &mut |tu, out_unfrozen, _processed, _total| {
                let Some(&pi) = para_idx_by_id.get(&tu.tu_id) else {
                    return Ok(());
                };
                text_b.paragraphs[pi].text = out_unfrozen.to_string();
                Ok(())
            },
        )?;

        let b_text_json_trace = self.trace.dir().join(format!("{stem}.B.text.json"));
        fs::write(
            &b_text_json_trace,
            serde_json::to_vec_pretty(&text_b).context("serialize B text json")?,
        )
        .with_context(|| format!("write B text json: {}", b_text_json_trace.display()))?;

        let mem = build_memory(
            &source_lang,
            &target_lang,
            &translate_backend.name,
            None,
            None,
            &tus_paras,
            &HashMap::<usize, ParaNotes>::new(),
        );
        let mem_path = self.trace.dir().join("paragraph_memory.basic.json");
        let _ = write_memory_file(&mem_path, &mem);

        self.progress.info("Done.".to_string());
        Ok(())
    }

    fn resolve_lang_pair_from_pure_text(&self, text: &PureTextJson) -> (String, String) {
        match (self.cfg.source_lang.clone(), self.cfg.target_lang.clone()) {
            (Some(s), Some(t)) => (s, t),
            _ => {
                let mut excerpts: Vec<String> = Vec::new();
                for p in text.paragraphs.iter().take(64) {
                    if p.text.trim().is_empty() {
                        continue;
                    }
                    excerpts.push(p.text.clone());
                    if excerpts.len() >= 20 {
                        break;
                    }
                }
                auto_language_pair(&excerpts)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn translate_units_segmented_basic(
        &mut self,
        model: &mut NativeChatModel,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        stage: &str,
        prompt_tmpl: &str,
        repair_tmpl: &str,
        tus: &mut [TranslationUnit],
        on_unit: &mut dyn FnMut(&TranslationUnit, &str, usize, usize) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let total = tus.len().max(1);
        let max_chars = (backend.ctx_size as usize)
            .saturating_mul(2)
            .saturating_sub(1800)
            .max(4000);
        let max_items = 64usize;

        let mut processed = 0usize;
        let mut chunk_indices: Vec<usize> = Vec::new();
        let mut used = 0usize;

        for idx in 0..tus.len() {
            self.progress.progress(stage, idx + 1, total);
            if is_trivial_sentinel_text(&tus[idx].frozen_surface) {
                let src = tus[idx].source_surface.clone();
                tus[idx].draft_translation = Some(src.clone());
                tus[idx].draft_translation_model = Some(backend.name.clone());
                processed += 1;
                on_unit(&tus[idx], &src, processed, total)?;
                continue;
            }

            let add = tus[idx].frozen_surface.len() + 64;
            if !chunk_indices.is_empty() && (used + add > max_chars || chunk_indices.len() >= max_items) {
                self.translate_chunk_recursive_basic(
                    model,
                    backend,
                    source_lang,
                    target_lang,
                    stage,
                    prompt_tmpl,
                    repair_tmpl,
                    tus,
                    &chunk_indices,
                    &mut processed,
                    total,
                    on_unit,
                )?;
                chunk_indices.clear();
                used = 0;
            }
            used += add;
            chunk_indices.push(idx);
        }
        if !chunk_indices.is_empty() {
            self.translate_chunk_recursive_basic(
                model,
                backend,
                source_lang,
                target_lang,
                stage,
                prompt_tmpl,
                repair_tmpl,
                tus,
                &chunk_indices,
                &mut processed,
                total,
                on_unit,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn translate_slot_texts_segmented_basic(
        &mut self,
        model: &mut NativeChatModel,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        stage: &str,
        prompt_tmpl: &str,
        repair_tmpl: &str,
        tus: &mut [TranslationUnit],
        text_variant: &mut PureTextJson,
        mask_json: &Path,
        offsets_json: &Path,
        autosave_text_json: &Path,
        output: &Path,
    ) -> anyhow::Result<()> {
        let total = tus.len().max(1);
        let max_chars = (backend.ctx_size as usize)
            .saturating_mul(2)
            .saturating_sub(1800)
            .max(4000);
        let max_items = 64usize;

        let mut processed = 0usize;
        let mut chunk_indices: Vec<usize> = Vec::new();
        let mut used = 0usize;

        for idx in 0..tus.len() {
            self.progress.progress(stage, idx + 1, total);
            if is_trivial_sentinel_text(&tus[idx].frozen_surface) {
                let src = tus[idx].source_surface.clone();
                tus[idx].draft_translation = Some(src.clone());
                tus[idx].draft_translation_model = Some(backend.name.clone());
                let slot_id = tus[idx].tu_id;
                if slot_id > 0 {
                    let sidx = slot_id.saturating_sub(1);
                    if sidx < text_variant.slot_texts.len() {
                        text_variant.slot_texts[sidx] = src;
                    }
                }
                processed += 1;
                if processed % self.cfg.autosave_every == 0 {
                    let _ = self.write_progress_docx(
                        mask_json,
                        offsets_json,
                        autosave_text_json,
                        output,
                        text_variant,
                        processed,
                        total,
                    );
                }
                continue;
            }

            let add = tus[idx].frozen_surface.len() + 64;
            if !chunk_indices.is_empty() && (used + add > max_chars || chunk_indices.len() >= max_items) {
                self.translate_slot_chunk_recursive_basic(
                    model,
                    backend,
                    source_lang,
                    target_lang,
                    stage,
                    prompt_tmpl,
                    repair_tmpl,
                    tus,
                    text_variant,
                    mask_json,
                    offsets_json,
                    autosave_text_json,
                    output,
                    &chunk_indices,
                    &mut processed,
                    total,
                )?;
                chunk_indices.clear();
                used = 0;
            }
            used += add;
            chunk_indices.push(idx);
        }
        if !chunk_indices.is_empty() {
            self.translate_slot_chunk_recursive_basic(
                model,
                backend,
                source_lang,
                target_lang,
                stage,
                prompt_tmpl,
                repair_tmpl,
                tus,
                text_variant,
                mask_json,
                offsets_json,
                autosave_text_json,
                output,
                &chunk_indices,
                &mut processed,
                total,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn translate_slot_chunk_recursive_basic(
        &mut self,
        model: &mut NativeChatModel,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        stage: &str,
        prompt_tmpl: &str,
        repair_tmpl: &str,
        tus: &mut [TranslationUnit],
        text_variant: &mut PureTextJson,
        mask_json: &Path,
        offsets_json: &Path,
        autosave_text_json: &Path,
        output: &Path,
        indices: &[usize],
        processed: &mut usize,
        total: usize,
    ) -> anyhow::Result<()> {
        if indices.is_empty() {
            return Ok(());
        }

        let first = tus[indices[0]].tu_id;
        let last = tus[*indices.last().unwrap_or(&indices[0])].tu_id;

        let mut expected_ids: Vec<usize> = Vec::with_capacity(indices.len());
        let mut tu_block = String::new();
        for &idx in indices {
            let tu = &tus[idx];
            expected_ids.push(tu.tu_id);
            tu_block.push_str(&seg_start(tu.tu_id));
            tu_block.push('\n');
            tu_block.push_str(&tu.frozen_surface);
            tu_block.push('\n');
            tu_block.push_str(&seg_end(tu.tu_id));
            tu_block.push_str("\n\n");
        }

        let source_lang_label = lang_label(source_lang);
        let target_lang_label = lang_label(target_lang);
        let prompt = render_template(
            prompt_tmpl,
            &[
                ("source_lang", &source_lang_label),
                ("target_lang", &target_lang_label),
                ("tu_block", &tu_block),
            ],
        );
        let _ = self.trace.write_named_text(
            &format!("{stage}.chunk.{first:06}-{last:06}.prompt.txt"),
            &prompt,
        );

        let max_tokens = backend.ctx_size.saturating_sub(256).clamp(512, 4096);
        let raw = model.chat(
            None,
            &prompt,
            max_tokens,
            0.12,
            0.9,
            Some(40),
            Some(1.05),
            false,
        )?;
        let cleaned = cleanup_model_text(&raw);
        let _ = self.trace.write_named_text(
            &format!("{stage}.chunk.{first:06}-{last:06}.output.raw.txt"),
            &cleaned,
        );

        let segs = match parse_segmented_output(&cleaned, &expected_ids) {
            Ok(v) => v,
            Err(_err) => {
                if indices.len() > 1 {
                    let mid = indices.len() / 2;
                    self.translate_slot_chunk_recursive_basic(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        stage,
                        prompt_tmpl,
                        repair_tmpl,
                        tus,
                        text_variant,
                        mask_json,
                        offsets_json,
                        autosave_text_json,
                        output,
                        &indices[..mid],
                        processed,
                        total,
                    )?;
                    self.translate_slot_chunk_recursive_basic(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        stage,
                        prompt_tmpl,
                        repair_tmpl,
                        tus,
                        text_variant,
                        mask_json,
                        offsets_json,
                        autosave_text_json,
                        output,
                        &indices[mid..],
                        processed,
                        total,
                    )?;
                    return Ok(());
                }
                let idx = indices[0];
                let tu_id = tus[idx].tu_id;
                let mut out = cleaned.clone();
                let sm = seg_start(tu_id);
                let em = seg_end(tu_id);
                if let Some(i) = out.find(&sm) {
                    out = out[i + sm.len()..].to_string();
                }
                if let Some(i) = out.find(&em) {
                    out = out[..i].to_string();
                }
                let out = cleanup_model_text(&out);
                let out_unfrozen = self.finalize_basic_output(
                    model,
                    backend,
                    source_lang,
                    target_lang,
                    repair_tmpl,
                    &mut tus[idx],
                    out,
                )?;
                apply_slot_text(text_variant, tu_id, &out_unfrozen)?;
                *processed += 1;
                if *processed % self.cfg.autosave_every == 0 {
                    let _ = self.write_progress_docx(
                        mask_json,
                        offsets_json,
                        autosave_text_json,
                        output,
                        text_variant,
                        *processed,
                        total,
                    );
                }
                return Ok(());
            }
        };

        for &idx in indices {
            let tu_id = tus[idx].tu_id;
            let out = segs.get(&tu_id).cloned().unwrap_or_default();
            let out_unfrozen = self.finalize_basic_output(
                model,
                backend,
                source_lang,
                target_lang,
                repair_tmpl,
                &mut tus[idx],
                cleanup_model_text(&out),
            )?;
            apply_slot_text(text_variant, tu_id, &out_unfrozen)?;
            *processed += 1;
            if *processed % self.cfg.autosave_every == 0 {
                let _ = self.write_progress_docx(
                    mask_json,
                    offsets_json,
                    autosave_text_json,
                    output,
                    text_variant,
                    *processed,
                    total,
                );
            }
        }
        Ok(())
    }

    fn finalize_basic_output(
        &mut self,
        model: &mut NativeChatModel,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        repair_tmpl: &str,
        tu: &mut TranslationUnit,
        mut out: String,
    ) -> anyhow::Result<String> {
        let source = tu.frozen_surface.clone();
        let must_keep_tokens = crate::sentinels::must_keep_tokens(&source);
        let nt_map = render_nt_map_for_prompt(&tu.nt_map);
        let mut repairs_done = 0usize;
        let mut max_repairs = 2usize;
        loop {
            out = normalize_nt_tokens(&source, &tu.nt_map, &out);
            let validation_error = validate_translation(tu, &out)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            let heur = quality_heuristics(tu, &out, source_lang, target_lang);
            let needs_repair = !validation_error.is_empty() || heur.wants_force_retranslate();
            if !needs_repair {
                break;
            }
            if validation_error.contains("sentinel_sequence_mismatch")
                || validation_error.contains("control_token_")
                || validation_error.contains("nt_token_")
                || validation_error.contains("unexpected_mt_token")
            {
                max_repairs = max_repairs.max(6);
            }
            if repairs_done >= max_repairs {
                break;
            }
            let mut reason = validation_error;
            if reason.is_empty() && (!heur.hard_flags.is_empty() || !heur.soft_flags.is_empty()) {
                let mut flags = Vec::new();
                flags.extend_from_slice(&heur.hard_flags);
                flags.extend_from_slice(&heur.soft_flags);
                reason = flags.join(" | ");
            }
            if reason.is_empty() {
                reason = "needs_repair".to_string();
            }
            out = self.repair_translation(
                model,
                repair_tmpl,
                source_lang,
                target_lang,
                &source,
                &out,
                &must_keep_tokens,
                &reason,
                &nt_map,
            )?;
            repairs_done += 1;
        }
        if let Err(err) = validate_translation(tu, &out) {
            let scope_tag = if tu.scope_key.starts_with("slot#") {
                "slot"
            } else if tu.scope_key.contains("#w:p") || tu.scope_key.contains("#a:p") {
                "para"
            } else {
                "tu"
            };
            let report = format!(
                "validate_error: {err}\n\nSOURCE_FROZEN:\n{source}\n\nOUTPUT_FROZEN:\n{out}\n"
            );
            let _ = self.trace.write_named_text(
                &format!("{scope_tag}_tu_{:06}.basic.validate_fail.txt", tu.tu_id),
                &report,
            );

            if let Ok(forced) =
                self.force_translate_preserving_tokens(model, backend, source_lang, target_lang, &source)
            {
                match validate_translation(tu, &forced) {
                    Ok(()) => out = forced,
                    Err(err2) => {
                        let report = format!(
                            "validate_error: {err2}\n\nSOURCE_FROZEN:\n{source}\n\nFORCED_OUTPUT_FROZEN:\n{forced}\n"
                        );
                        let _ = self.trace.write_named_text(
                            &format!(
                                "{scope_tag}_tu_{:06}.basic.forced_validate_fail.txt",
                                tu.tu_id
                            ),
                            &report,
                        );
                        out = source;
                    }
                }
            } else {
                out = source;
            }
        }
        let out_unfrozen = unfreeze_text(&out, &tu.nt_map);
        tu.draft_translation = Some(out_unfrozen.clone());
        tu.draft_translation_model = Some(backend.name.clone());
        Ok(out_unfrozen)
    }

    fn force_translate_preserving_tokens(
        &mut self,
        model: &mut NativeChatModel,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        source_frozen: &str,
    ) -> anyhow::Result<String> {
        #[derive(Clone, Debug)]
        enum Item {
            Plain(String),
            Token(String),
        }

        const HOLE: &str = "[[MT_HOLE]]";

        let mut items: Vec<Item> = Vec::new();
        let mut plain_positions: Vec<usize> = Vec::new();
        let mut cursor = 0usize;
        for m in ANY_SENTINEL_RE.find_iter(source_frozen) {
            if m.start() > cursor {
                let plain = source_frozen[cursor..m.start()].to_string();
                if !plain.trim().is_empty() {
                    plain_positions.push(items.len());
                }
                items.push(Item::Plain(plain));
            } else if m.start() == cursor {
                items.push(Item::Plain(String::new()));
            }
            items.push(Item::Token(m.as_str().to_string()));
            cursor = m.end();
        }
        if cursor < source_frozen.len() {
            let plain = source_frozen[cursor..].to_string();
            if !plain.trim().is_empty() {
                plain_positions.push(items.len());
            }
            items.push(Item::Plain(plain));
        }

        if plain_positions.is_empty() {
            return Ok(source_frozen.to_string());
        }

        let source_lang_label = lang_label(source_lang);
        let target_lang_label = lang_label(target_lang);

        let mut translations: Vec<String> = Vec::with_capacity(plain_positions.len());
        for &pos in &plain_positions {
            let plain = match items.get(pos).cloned().unwrap_or(Item::Plain(String::new())) {
                Item::Plain(s) => s,
                Item::Token(_) => String::new(),
            };

            let prev_is_token = pos > 0 && matches!(items.get(pos - 1), Some(Item::Token(_)));
            let next_is_token = matches!(items.get(pos + 1), Some(Item::Token(_)));
            let mut decorated = plain.clone();
            if prev_is_token {
                decorated = format!("{HOLE}{decorated}");
            }
            if next_is_token {
                decorated = format!("{decorated}{HOLE}");
            }

            let max_tokens = ((decorated.len() as u32) / 2)
                .clamp(256, backend.ctx_size.saturating_sub(256).clamp(512, 4096));
            let prompt = if target_lang_label.to_ascii_lowercase().contains("chinese") {
                format!(
                    "请将下面的{source_lang_label}翻译成{target_lang_label}。\n规则：\n- 逐句完整翻译，不要省略或总结，不要使用“……”或“...”占位。\n- 标记{HOLE}表示紧邻位置将插入不可翻译标记，请在输出中原样保留该标记（不要翻译、不要删除、不要添加空格）。\n- 保持所有数字0-9不变。\n仅输出译文：\n{decorated}\n"
                )
            } else {
                format!(
                    "Translate the following {source_lang_label} into {target_lang_label}.\nRules:\n- Translate fully; do not omit or summarize; do not use ellipsis placeholders like … or ....\n- The marker {HOLE} indicates a non-translatable token will be inserted adjacent to it; keep {HOLE} unchanged (do not translate, delete, or add spaces).\n- Preserve all digits 0-9 exactly.\nOutput ONLY the translation:\n{decorated}\n"
                )
            };
            let raw = model.chat(None, &prompt, max_tokens, 0.0, 0.9, Some(40), Some(1.05), false)?;
            let cleaned = cleanup_model_text(&raw);
            let cleaned = crate::sentinels::ANY_MT_TOKEN_RE
                .replace_all(&cleaned, "")
                .into_owned();
            let cleaned = cleaned.replace(HOLE, "");
            translations.push(cleaned.trim().to_string());
        }

        let mut out = String::new();
        let mut tr_it = translations.into_iter();
        for item in items.into_iter() {
            match item {
                Item::Token(t) => out.push_str(&t),
                Item::Plain(s) => {
                    if s.trim().is_empty() {
                        out.push_str(&s);
                        continue;
                    }
                    let t = tr_it.next().unwrap_or_default();
                    out.push_str(&t);
                }
            }
        }

        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn translate_chunk_recursive_basic(
        &mut self,
        model: &mut NativeChatModel,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        stage: &str,
        prompt_tmpl: &str,
        repair_tmpl: &str,
        tus: &mut [TranslationUnit],
        indices: &[usize],
        processed: &mut usize,
        total: usize,
        on_unit: &mut dyn FnMut(&TranslationUnit, &str, usize, usize) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if indices.is_empty() {
            return Ok(());
        }

        let first = tus[indices[0]].tu_id;
        let last = tus[*indices.last().unwrap_or(&indices[0])].tu_id;

        let mut expected_ids: Vec<usize> = Vec::with_capacity(indices.len());
        let mut tu_block = String::new();
        for &idx in indices {
            let tu = &tus[idx];
            expected_ids.push(tu.tu_id);
            tu_block.push_str(&seg_start(tu.tu_id));
            tu_block.push('\n');
            tu_block.push_str(&tu.frozen_surface);
            tu_block.push('\n');
            tu_block.push_str(&seg_end(tu.tu_id));
            tu_block.push_str("\n\n");
        }

        let source_lang_label = lang_label(source_lang);
        let target_lang_label = lang_label(target_lang);
        let prompt = render_template(
            prompt_tmpl,
            &[
                ("source_lang", &source_lang_label),
                ("target_lang", &target_lang_label),
                ("tu_block", &tu_block),
            ],
        );
        let _ = self.trace.write_named_text(
            &format!("{stage}.chunk.{first:06}-{last:06}.prompt.txt"),
            &prompt,
        );

        let max_tokens = backend.ctx_size.saturating_sub(256).clamp(512, 4096);
        let raw = model.chat(
            None,
            &prompt,
            max_tokens,
            0.12,
            0.9,
            Some(40),
            Some(1.05),
            false,
        )?;
        let cleaned = cleanup_model_text(&raw);
        let _ = self.trace.write_named_text(
            &format!("{stage}.chunk.{first:06}-{last:06}.output.raw.txt"),
            &cleaned,
        );

        let segs = match parse_segmented_output(&cleaned, &expected_ids) {
            Ok(v) => v,
            Err(err) => {
                if indices.len() > 1 {
                    let mid = indices.len() / 2;
                    self.translate_chunk_recursive_basic(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        stage,
                    prompt_tmpl,
                    repair_tmpl,
                    tus,
                    &indices[..mid],
                    processed,
                    total,
                    on_unit,
                )?;
                    self.translate_chunk_recursive_basic(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        stage,
                        prompt_tmpl,
                        repair_tmpl,
                        tus,
                        &indices[mid..],
                        processed,
                        total,
                        on_unit,
                    )?;
                    return Ok(());
                }
                let idx = indices[0];
                let tu_id = tus[idx].tu_id;
                let mut out = cleaned.clone();
                let sm = seg_start(tu_id);
                let em = seg_end(tu_id);
                if let Some(i) = out.find(&sm) {
                    out = out[i + sm.len()..].to_string();
                }
                if let Some(i) = out.find(&em) {
                    out = out[..i].to_string();
                }
                let out = cleanup_model_text(&out);
                return self
                    .apply_basic_tu(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        repair_tmpl,
                        &mut tus[idx],
                        out,
                        processed,
                        total,
                        on_unit,
                    )
                    .with_context(|| format!("fallback segmented parse failed: {err}"));
            }
        };

        for &idx in indices {
            let tu_id = tus[idx].tu_id;
            let out = segs.get(&tu_id).cloned().unwrap_or_default();
            self.apply_basic_tu(
                model,
                backend,
                source_lang,
                target_lang,
                repair_tmpl,
                &mut tus[idx],
                cleanup_model_text(&out),
                processed,
                total,
                on_unit,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_basic_tu(
        &mut self,
        model: &mut NativeChatModel,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        repair_tmpl: &str,
        tu: &mut TranslationUnit,
        out: String,
        processed: &mut usize,
        total: usize,
        on_unit: &mut dyn FnMut(&TranslationUnit, &str, usize, usize) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let out_unfrozen = self.finalize_basic_output(
            model,
            backend,
            source_lang,
            target_lang,
            repair_tmpl,
            tu,
            out,
        )?;
        *processed += 1;
        on_unit(tu, &out_unfrozen, *processed, total)?;
        Ok(())
    }
}

fn apply_slot_text(text_json: &mut PureTextJson, slot_id: usize, translated: &str) -> anyhow::Result<()> {
    if slot_id == 0 {
        return Ok(());
    }
    let idx = slot_id.saturating_sub(1);
    if idx >= text_json.slot_texts.len() {
        return Err(anyhow!(
            "slot_id_out_of_range slot_id={} slot_texts_len={}",
            slot_id,
            text_json.slot_texts.len()
        ));
    }
    text_json.slot_texts[idx] = translated.to_string();
    Ok(())
}
