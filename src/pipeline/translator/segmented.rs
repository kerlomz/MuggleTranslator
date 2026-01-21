use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;

use crate::config::ResolvedBackend;
use crate::docx::pure_text::PureTextJson;
use crate::ir::TranslationUnit;
use crate::models::native::NativeChatModel;
use crate::quality::{quality_heuristics, validate_translation};
use crate::sentinels::{parse_segmented_output, seg_end, seg_start};
use crate::textutil::lang_label;

use super::{
    cleanup_model_text, render_template, set_translation_slot, ParaNotes, TranslationSlot,
    TranslatorPipeline,
};

impl TranslatorPipeline {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn translate_chunk_recursive(
        &mut self,
        model: &mut NativeChatModel,
        backend: &ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        prompt_tmpl: &str,
        repair_tmpl: &str,
        tus: &mut [TranslationUnit],
        slot: TranslationSlot,
        text_variant: &mut PureTextJson,
        slots_by_tu: &HashMap<usize, Vec<usize>>,
        mask_json: &Path,
        offsets_json: &Path,
        autosave_text_json: &Path,
        output: &Path,
        indices: &[usize],
        processed: &mut usize,
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
            &format!(
                "{}.chunk.{first:06}-{last:06}.prompt.txt",
                slot.stage_name()
            ),
            &prompt,
        );

        let max_tokens = backend.ctx_size.saturating_sub(256).max(512);
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
            &format!(
                "{}.chunk.{first:06}-{last:06}.output.raw.txt",
                slot.stage_name()
            ),
            &cleaned,
        );

        let segs = match parse_segmented_output(&cleaned, &expected_ids) {
            Ok(v) => v,
            Err(err) => {
                if indices.len() > 1 {
                    let mid = indices.len() / 2;
                    self.translate_chunk_recursive(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        prompt_tmpl,
                        repair_tmpl,
                        tus,
                        slot,
                        text_variant,
                        slots_by_tu,
                        mask_json,
                        offsets_json,
                        autosave_text_json,
                        output,
                        &indices[..mid],
                        processed,
                    )?;
                    self.translate_chunk_recursive(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        prompt_tmpl,
                        repair_tmpl,
                        tus,
                        slot,
                        text_variant,
                        slots_by_tu,
                        mask_json,
                        offsets_json,
                        autosave_text_json,
                        output,
                        &indices[mid..],
                        processed,
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
                    .apply_translated_tu(
                        model,
                        backend,
                        source_lang,
                        target_lang,
                        repair_tmpl,
                        tus,
                        slot,
                        text_variant,
                        slots_by_tu,
                        mask_json,
                        offsets_json,
                        autosave_text_json,
                        output,
                        idx,
                        out,
                        processed,
                    )
                    .with_context(|| format!("fallback segmented parse failed: {err}"));
            }
        };

        for &idx in indices {
            let tu_id = tus[idx].tu_id;
            let out = segs.get(&tu_id).cloned().unwrap_or_default();
            self.apply_translated_tu(
                model,
                backend,
                source_lang,
                target_lang,
                repair_tmpl,
                tus,
                slot,
                text_variant,
                slots_by_tu,
                mask_json,
                offsets_json,
                autosave_text_json,
                output,
                idx,
                cleanup_model_text(&out),
                processed,
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_translated_tu(
        &mut self,
        model: &mut NativeChatModel,
        backend: &ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        repair_tmpl: &str,
        tus: &mut [TranslationUnit],
        slot: TranslationSlot,
        text_variant: &mut PureTextJson,
        slots_by_tu: &HashMap<usize, Vec<usize>>,
        mask_json: &Path,
        offsets_json: &Path,
        autosave_text_json: &Path,
        output: &Path,
        idx: usize,
        mut out: String,
        processed: &mut usize,
    ) -> anyhow::Result<()> {
        let tu_id = tus[idx].tu_id;
        let source = tus[idx].frozen_surface.clone();
        let must_keep_tokens = crate::sentinels::must_keep_tokens(&source);
        let nt_map = crate::freezer::render_nt_map_for_prompt(&tus[idx].nt_map);
        let mut validation_error = validate_translation(&tus[idx], &out)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        if validate_translation(&tus[idx], &out).is_err()
            || quality_heuristics(&tus[idx], &out, source_lang, target_lang)
                .wants_force_retranslate()
        {
            if validation_error.is_empty() {
                validation_error = "quality_force_retranslate".to_string();
            }
            let repaired = self.repair_translation(
                model,
                repair_tmpl,
                source_lang,
                target_lang,
                &source,
                &out,
                &must_keep_tokens,
                &validation_error,
                &nt_map,
            )?;
            out = repaired;
        }
        if validate_translation(&tus[idx], &out).is_err() {
            out = source.clone();
        }

        let slots = slots_by_tu.get(&tu_id).cloned().unwrap_or_default();
        if !slots.is_empty() {
            if self
                .apply_slot_translation(text_variant, &slots, &tus[idx], &out)
                .is_err()
            {
                let reason = "slot_projection_failed".to_string();
                let repaired = self.repair_translation(
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
                out = repaired;
                if validate_translation(&tus[idx], &out).is_err()
                    || self
                        .apply_slot_translation(text_variant, &slots, &tus[idx], &out)
                        .is_err()
                {
                    out = source.clone();
                    let _ = self.apply_slot_translation(text_variant, &slots, &tus[idx], &out);
                }
            }
        }

        set_translation_slot(&mut tus[idx], slot, out.clone(), &backend.name);

        *processed += 1;
        if *processed % self.cfg.autosave_every == 0 {
            let total = tus.len().max(1);
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
        Ok(())
    }

    pub(super) fn fuse_chunk_recursive(
        &mut self,
        model: &mut NativeChatModel,
        fuse_tmpl: &str,
        repair_tmpl: &str,
        source_lang: &str,
        target_lang: &str,
        tus: &mut [TranslationUnit],
        notes: &HashMap<usize, ParaNotes>,
        indices: &[usize],
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
            let a = tu
                .draft_translation
                .as_deref()
                .unwrap_or(&tu.frozen_surface);
            let b = tu.alt_translation.as_deref().unwrap_or(a);
            let note = notes
                .get(&tu.tu_id)
                .and_then(|n| n.understanding.as_ref())
                .map(|s| s.as_str())
                .unwrap_or("");

            expected_ids.push(tu.tu_id);
            tu_block.push_str(&seg_start(tu.tu_id));
            tu_block.push('\n');
            tu_block.push_str("SOURCE:\n");
            tu_block.push_str(&tu.frozen_surface);
            tu_block.push_str("\n\nA:\n");
            tu_block.push_str(a);
            tu_block.push_str("\n\nB:\n");
            tu_block.push_str(b);
            if !note.trim().is_empty() {
                tu_block.push_str("\n\nNOTE:\n");
                tu_block.push_str(note.trim());
            }
            tu_block.push('\n');
            tu_block.push_str(&seg_end(tu.tu_id));
            tu_block.push_str("\n\n");
        }

        let target_lang_label = lang_label(target_lang);
        let prompt = render_template(
            fuse_tmpl,
            &[("target_lang", &target_lang_label), ("tu_block", &tu_block)],
        );
        let _ = self.trace.write_named_text(
            &format!("fuse.chunk.{first:06}-{last:06}.prompt.txt"),
            &prompt,
        );

        let raw = model.chat(None, &prompt, 2600, 0.2, 0.9, Some(40), Some(1.05), false)?;
        let cleaned = cleanup_model_text(&raw);
        let _ = self.trace.write_named_text(
            &format!("fuse.chunk.{first:06}-{last:06}.output.raw.txt"),
            &cleaned,
        );

        let segs = match parse_segmented_output(&cleaned, &expected_ids) {
            Ok(v) => v,
            Err(err) => {
                if indices.len() > 1 {
                    let mid = indices.len() / 2;
                    self.fuse_chunk_recursive(
                        model,
                        fuse_tmpl,
                        repair_tmpl,
                        source_lang,
                        target_lang,
                        tus,
                        notes,
                        &indices[..mid],
                    )?;
                    self.fuse_chunk_recursive(
                        model,
                        fuse_tmpl,
                        repair_tmpl,
                        source_lang,
                        target_lang,
                        tus,
                        notes,
                        &indices[mid..],
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
                    .apply_fused_tu(model, repair_tmpl, source_lang, target_lang, tus, idx, out)
                    .with_context(|| format!("fallback segmented parse failed: {err}"));
            }
        };

        for &idx in indices {
            let tu_id = tus[idx].tu_id;
            let out = segs.get(&tu_id).cloned().unwrap_or_default();
            self.apply_fused_tu(
                model,
                repair_tmpl,
                source_lang,
                target_lang,
                tus,
                idx,
                cleanup_model_text(&out),
            )?;
        }

        Ok(())
    }

    fn apply_fused_tu(
        &mut self,
        model: &mut NativeChatModel,
        repair_tmpl: &str,
        source_lang: &str,
        target_lang: &str,
        tus: &mut [TranslationUnit],
        idx: usize,
        mut out: String,
    ) -> anyhow::Result<()> {
        let source = tus[idx].frozen_surface.clone();
        let must_keep_tokens = crate::sentinels::must_keep_tokens(&source);
        let nt_map = crate::freezer::render_nt_map_for_prompt(&tus[idx].nt_map);
        let mut validation_error = validate_translation(&tus[idx], &out)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        let a = tus[idx]
            .draft_translation
            .clone()
            .unwrap_or_else(|| source.clone());

        if validate_translation(&tus[idx], &out).is_err()
            || quality_heuristics(&tus[idx], &out, source_lang, target_lang)
                .wants_force_retranslate()
        {
            if validation_error.is_empty() {
                validation_error = "quality_force_retranslate".to_string();
            }
            let repaired = self.repair_translation(
                model,
                repair_tmpl,
                source_lang,
                target_lang,
                &source,
                &out,
                &must_keep_tokens,
                &validation_error,
                &nt_map,
            )?;
            out = repaired;
        }
        if validate_translation(&tus[idx], &out).is_err() {
            out = a;
        }

        tus[idx].final_translation = Some(out);
        Ok(())
    }
}
