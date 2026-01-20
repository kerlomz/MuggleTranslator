use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use once_cell::sync::Lazy;
use serde::Deserialize;

use crate::docx::apply::apply_translation_unit;
use crate::docx::extract::extract_translation_units;
use crate::docx::package::DocxPackage;
use crate::docx::xml::{parse_xml_part, verify_structure_unchanged, write_xml_part, XmlPart};
use crate::freezer::freeze_text;
use crate::ir::TranslationUnit;
use crate::models::native::{NativeChatModel, NativeModelConfig};
use crate::progress::ConsoleProgress;
use crate::quality::{must_extract_json_obj, validate_translation};
use crate::textutil::{auto_language_pair, is_trivial_sentinel_text};
use llama_cpp_2::llama_backend::LlamaBackend;

use super::memory::{build_memory, write_memory_file, ParaNotes};
use super::prompts::render_template;
use super::trace::TraceWriter;
use super::PipelineConfig;

mod segmented;

static LLAMA_BACKEND: Lazy<LlamaBackend> =
    Lazy::new(|| LlamaBackend::init().expect("init llama backend"));

pub struct TranslatorPipeline {
    cfg: PipelineConfig,
    progress: ConsoleProgress,
    trace: TraceWriter,
}

impl TranslatorPipeline {
    pub fn new(cfg: PipelineConfig, progress: ConsoleProgress) -> Self {
        let trace = TraceWriter::new(cfg.trace_dir.clone(), cfg.trace_prompts)
            .unwrap_or_else(|_| TraceWriter::new(cfg.trace_dir.clone(), false).expect("trace"));
        Self { cfg, progress, trace }
    }

    pub fn translate_docx(&mut self, input: &Path, output: &Path) -> anyhow::Result<()> {
        self.progress.info(format!("Read DOCX: {}", input.display()));
        let pkg = DocxPackage::read(input)?;
        let parts_original = load_xml_parts(&pkg)?;

        let (mut tus, next_id) = extract_all_units(&parts_original)?;
        self.progress
            .info(format!("Extracted {} translation units", tus.len()));
        if let Some(max_tus) = self.cfg.max_tus {
            let keep = max_tus.max(1).min(tus.len());
            tus.truncate(keep);
            self.progress.info(format!("Max TUs: {keep}"));
        }

        for tu in &mut tus {
            let fr = freeze_text(&tu.source_surface);
            tu.frozen_surface = fr.text;
            tu.nt_map = fr.nt_map;
            tu.nt_mask = fr.mask;
        }

        let (source_lang, target_lang) = self.resolve_lang_pair(&tus);
        self.progress
            .info(format!("Language: {source_lang} -> {target_lang}"));

        let mut notes: HashMap<usize, ParaNotes> = HashMap::new();
        if let Some(agent) = self.cfg.controller_backend.clone() {
            self.progress.info(format!("Notes model: {}", agent.name));
            self.run_para_notes(&agent, &target_lang, &tus, &mut notes)?;
        }
        self.write_memory_snapshot("stage0", &source_lang, &target_lang, &tus, &notes);

        // Translate A
        let translate_backend = self.cfg.translate_backend.clone();
        let prompt_translate_a = self.cfg.prompts.translate_a.clone();
        let prompt_translate_repair = self.cfg.prompts.translate_repair.clone();
        self.progress
            .info(format!("Translate A: {}", translate_backend.name));
        let mut parts_a = parts_original.clone();
        self.translate_stage(
            &pkg,
            &translate_backend,
            &source_lang,
            &target_lang,
            &prompt_translate_a,
            &prompt_translate_repair,
            &mut tus,
            TranslationSlot::A,
            &mut parts_a,
            output,
        )?;
        let _ = write_variant_docx(&pkg, &parts_a, output, "A");
        self.write_memory_snapshot("afterA", &source_lang, &target_lang, &tus, &notes);

        // Translate B
        if let Some(alt) = self.cfg.alt_translate_backend.clone() {
            let prompt_translate_b = self.cfg.prompts.translate_b.clone();
            let prompt_translate_repair = self.cfg.prompts.translate_repair.clone();
            self.progress.info(format!("Translate B: {}", alt.name));
            let mut parts_b = parts_original.clone();
            self.translate_stage(
                &pkg,
                &alt,
                &source_lang,
                &target_lang,
                &prompt_translate_b,
                &prompt_translate_repair,
                &mut tus,
                TranslationSlot::B,
                &mut parts_b,
                output,
            )?;
            let _ = write_variant_docx(&pkg, &parts_b, output, "B");
            self.write_memory_snapshot("afterB", &source_lang, &target_lang, &tus, &notes);
        }

        // Fuse AB via agent (paragraphs only). Others default to A.
        if let Some(agent) = self.cfg.controller_backend.clone() {
            self.progress.info(format!("Fuse AB via: {}", agent.name));
            self.run_fuse_stage(&agent, &source_lang, &target_lang, &mut tus, &notes)?;
        } else {
            for tu in &mut tus {
                if tu.final_translation.is_none() {
                    tu.final_translation = tu.draft_translation.clone();
                }
            }
        }
        self.write_memory_snapshot("afterFuse", &source_lang, &target_lang, &tus, &notes);

        // Apply final
        let mut parts_final = parts_original.clone();
        for tu in &tus {
            let t = tu
                .final_translation
                .as_deref()
                .or(tu.draft_translation.as_deref())
                .unwrap_or(&tu.frozen_surface);
            apply_translation_unit(&mut parts_final, tu, t)
                .with_context(|| format!("apply final tu_id={}", tu.tu_id))?;
        }
        self.write_progress_docx(&pkg, output, &parts_final, 1, 1)?;

        // Global stitch audit + patch (2 rounds max)
        if let Some(agent) = self.cfg.controller_backend.clone() {
            let rewrite_backend = self.cfg.rewrite_backend.clone();
            self.run_stitch_audit_and_patch(
                &pkg,
                &agent,
                &rewrite_backend,
                &source_lang,
                &target_lang,
                &mut tus,
                &notes,
                &mut parts_final,
                output,
            )?;
        }

        // Write final output
        self.progress.info(format!("Write output: {}", output.display()));
        write_docx_with_parts(&pkg, &parts_final, output)?;

        self.write_memory_snapshot("final", &source_lang, &target_lang, &tus, &notes);
        self.progress.info(format!("Done. next_tu_id={next_id}"));
        Ok(())
    }

    fn resolve_lang_pair(&self, tus: &[TranslationUnit]) -> (String, String) {
        match (self.cfg.source_lang.clone(), self.cfg.target_lang.clone()) {
            (Some(s), Some(t)) => (s, t),
            _ => {
                let mut excerpts: Vec<String> = Vec::new();
                for tu in tus.iter().take(64) {
                    if tu.source_surface.trim().is_empty() {
                        continue;
                    }
                    excerpts.push(tu.source_surface.clone());
                    if excerpts.len() >= 20 {
                        break;
                    }
                }
                auto_language_pair(&excerpts)
            }
        }
    }

    fn write_memory_snapshot(
        &self,
        stage: &str,
        source_lang: &str,
        target_lang: &str,
        tus: &[TranslationUnit],
        notes: &HashMap<usize, ParaNotes>,
    ) {
        let mem = build_memory(
            source_lang,
            target_lang,
            &self.cfg.translate_backend.name,
            self.cfg.alt_translate_backend.as_ref().map(|b| b.name.as_str()),
            self.cfg.controller_backend.as_ref().map(|b| b.name.as_str()),
            tus,
            notes,
        );
        let path = self
            .trace
            .dir()
            .join(format!("paragraph_memory.{stage}.json"));
        let _ = write_memory_file(&path, &mem);
    }

    fn translate_stage(
        &mut self,
        pkg: &DocxPackage,
        backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        prompt_tmpl: &str,
        repair_tmpl: &str,
        tus: &mut [TranslationUnit],
        slot: TranslationSlot,
        parts: &mut HashMap<String, XmlPart>,
        output: &Path,
    ) -> anyhow::Result<()> {
        let mut model = load_model(&self.cfg, backend)?;
        let total = tus.len().max(1);
        let max_chars = (backend.ctx_size as usize)
            .saturating_mul(2)
            .saturating_sub(1800)
            .max(4000);
        let max_items = 32usize;

        let mut chunk_indices: Vec<usize> = Vec::new();
        let mut used = 0usize;
        let mut processed = 0usize;

        for idx in 0..tus.len() {
            self.progress.progress(slot.stage_name(), idx + 1, total);
            let is_skip = {
                let tu = &tus[idx];
                tu.frozen_surface.trim().is_empty() || is_trivial_sentinel_text(&tu.source_surface)
            };

            if is_skip {
                let tu_id = tus[idx].tu_id;
                let txt = tus[idx].frozen_surface.clone();
                set_translation_slot(&mut tus[idx], slot, txt.clone(), &backend.name);
                apply_translation_unit(parts, &tus[idx], &txt)
                    .with_context(|| format!("apply {} tu_id={}", slot.stage_name(), tu_id))?;
                processed += 1;
                if processed % self.cfg.autosave_every == 0 {
                    let _ = self.write_progress_docx(pkg, output, parts, processed, total);
                }
                continue;
            }

            let add = tus[idx].frozen_surface.len() + 96;
            if !chunk_indices.is_empty()
                && (used + add > max_chars || chunk_indices.len() >= max_items)
            {
                self.translate_chunk_recursive(
                    &mut model,
                    backend,
                    source_lang,
                    target_lang,
                    prompt_tmpl,
                    repair_tmpl,
                    tus,
                    slot,
                    parts,
                    output,
                    pkg,
                    &chunk_indices,
                    &mut processed,
                )?;
                chunk_indices.clear();
                used = 0;
            }
            used += add;
            chunk_indices.push(idx);
        }

        if !chunk_indices.is_empty() {
            self.translate_chunk_recursive(
                &mut model,
                backend,
                source_lang,
                target_lang,
                prompt_tmpl,
                repair_tmpl,
                tus,
                slot,
                parts,
                output,
                pkg,
                &chunk_indices,
                &mut processed,
            )?;
        }
        Ok(())
    }

    fn repair_translation(
        &mut self,
        model: &mut NativeChatModel,
        repair_tmpl: &str,
        source_lang: &str,
        target_lang: &str,
        source_frozen: &str,
        bad: &str,
    ) -> anyhow::Result<String> {
        let prompt = render_template(
            repair_tmpl,
            &[
                ("source_lang", source_lang),
                ("target_lang", target_lang),
                ("source", source_frozen),
                ("bad", bad),
            ],
        );
        let out = model.chat(None, &prompt, 900, 0.12, 0.9, Some(40), Some(1.05), false)?;
        Ok(cleanup_model_text(&out))
    }

    fn run_para_notes(
        &mut self,
        agent_backend: &crate::config::ResolvedBackend,
        target_lang: &str,
        tus: &[TranslationUnit],
        notes: &mut HashMap<usize, ParaNotes>,
    ) -> anyhow::Result<()> {
        let mut model = load_model(&self.cfg, agent_backend)?;

        let paras: Vec<&TranslationUnit> = tus
            .iter()
            .filter(|tu| tu.scope_key.contains("#w:p") || tu.scope_key.contains("#a:p"))
            .collect();
        if paras.is_empty() {
            return Ok(());
        }

        let max_chars = agent_backend.ctx_size.saturating_sub(1400) as usize;
        let max_items = 24usize;
        let mut chunk: Vec<&TranslationUnit> = Vec::new();
        let mut used = 0usize;

        for tu in paras {
            let add = tu.frozen_surface.len() + 64;
            if !chunk.is_empty() && (used + add > max_chars || chunk.len() >= max_items) {
                self.run_para_notes_chunk(&mut model, target_lang, &chunk, notes)?;
                chunk.clear();
                used = 0;
            }
            used += add;
            chunk.push(tu);
        }
        if !chunk.is_empty() {
            self.run_para_notes_chunk(&mut model, target_lang, &chunk, notes)?;
        }
        Ok(())
    }

    fn run_para_notes_chunk(
        &mut self,
        model: &mut NativeChatModel,
        target_lang: &str,
        chunk: &[&TranslationUnit],
        notes: &mut HashMap<usize, ParaNotes>,
    ) -> anyhow::Result<()> {
        let first = chunk.first().map(|t| t.tu_id).unwrap_or(0);
        let last = chunk.last().map(|t| t.tu_id).unwrap_or(0);

        let tu_block = chunk
            .iter()
            .map(|tu| format!("TU#{}:\n{}\n", tu.tu_id, tu.frozen_surface))
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = render_template(
            &self.cfg.prompts.para_notes,
            &[("target_lang", target_lang), ("tu_block", &tu_block)],
        );
        let _ = self
            .trace
            .write_named_text(&format!("para_notes.{first:06}-{last:06}.prompt.txt"), &prompt);

        let max_tokens = ((chunk.len() as u32) * 140).clamp(900, 3600);
        let raw = model.chat(None, &prompt, max_tokens, 0.15, 0.9, Some(40), Some(1.05), true)?;
        let _ = self.trace.write_named_text(
            &format!("para_notes.{first:06}-{last:06}.output.raw.txt"),
            &raw,
        );

        let parsed = match parse_json_with_repair(model, &self.cfg.prompts.json_repair, &raw, 1800)
        {
            Ok(v) => v,
            Err(err) => {
                let _ = self.trace.write_named_text(
                    &format!("para_notes.{first:06}-{last:06}.error.txt"),
                    &format!("{err:#}"),
                );
                self.progress.info(format!(
                    "[warn] para_notes parse failed (TU#{first}-{last}): {err}"
                ));
                return Ok(());
            }
        };
        let resp: ParaNotesChunkResponse =
            serde_json::from_value(parsed).context("parse para_notes json")?;

        for item in resp.paragraphs {
            notes.insert(
                item.tu_id,
                ParaNotes {
                    understanding: Some(item.understanding),
                    proper_nouns: item.proper_nouns,
                    terms: item.terms,
                },
            );
        }
        Ok(())
    }

    fn run_fuse_stage(
        &mut self,
        agent_backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        tus: &mut [TranslationUnit],
        notes: &HashMap<usize, ParaNotes>,
    ) -> anyhow::Result<()> {
        let mut model = load_model(&self.cfg, agent_backend)?;
        let repair_tmpl = self.cfg.prompts.translate_repair.clone();

        // Default non-paragraph to A.
        for tu in tus.iter_mut() {
            if !(tu.scope_key.contains("#w:p") || tu.scope_key.contains("#a:p")) {
                tu.final_translation = tu.draft_translation.clone();
            }
        }

        let para_indices: Vec<usize> = tus
            .iter()
            .enumerate()
            .filter(|(_, tu)| tu.scope_key.contains("#w:p") || tu.scope_key.contains("#a:p"))
            .map(|(i, _)| i)
            .collect();
        if para_indices.is_empty() {
            return Ok(());
        }

        let max_chars = (agent_backend.ctx_size as usize)
            .saturating_mul(2)
            .saturating_sub(2200)
            .max(6000);
        let max_items = 20usize;

        let mut chunk: Vec<usize> = Vec::new();
        let mut used = 0usize;

        for idx in para_indices {
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
            let add = tu.frozen_surface.len() + a.len() + b.len() + note.len() + 160;
            if !chunk.is_empty() && (used + add > max_chars || chunk.len() >= max_items) {
                self.fuse_chunk_recursive(
                    &mut model,
                    &repair_tmpl,
                    source_lang,
                    target_lang,
                    tus,
                    notes,
                    &chunk,
                )?;
                chunk.clear();
                used = 0;
            }
            used += add;
            chunk.push(idx);
        }

        if !chunk.is_empty() {
            self.fuse_chunk_recursive(
                &mut model,
                &repair_tmpl,
                source_lang,
                target_lang,
                tus,
                notes,
                &chunk,
            )?;
        }

        Ok(())
    }

    fn run_stitch_audit_and_patch(
        &mut self,
        pkg: &DocxPackage,
        agent_backend: &crate::config::ResolvedBackend,
        patch_backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        tus: &mut [TranslationUnit],
        notes: &HashMap<usize, ParaNotes>,
        parts_final: &mut HashMap<String, XmlPart>,
        output: &Path,
    ) -> anyhow::Result<()> {
        for round in 1..=2 {
            self.progress.info(format!("Stitch audit round {round}/2"));
            let issues = self.run_stitch_audit_round(agent_backend, target_lang, tus, round)?;
            if issues.is_empty() {
                break;
            }

            self.progress.info(format!("Patch issues: {}", issues.len()));
            self.run_patch_round(
                patch_backend,
                source_lang,
                target_lang,
                tus,
                notes,
                parts_final,
                &issues,
                round,
            )?;
            self.write_memory_snapshot(
                &format!("afterPatch{round}"),
                source_lang,
                target_lang,
                tus,
                notes,
            );
            let _ = self.write_progress_docx(pkg, output, parts_final, round, 2);
        }
        Ok(())
    }

    fn run_stitch_audit_round(
        &mut self,
        agent_backend: &crate::config::ResolvedBackend,
        target_lang: &str,
        tus: &[TranslationUnit],
        round: usize,
    ) -> anyhow::Result<Vec<StitchIssue>> {
        let paras: Vec<&TranslationUnit> = tus
            .iter()
            .filter(|tu| tu.scope_key.contains("#w:p") || tu.scope_key.contains("#a:p"))
            .collect();
        if paras.is_empty() {
            return Ok(vec![]);
        }

        let max_chars = agent_backend.ctx_size.saturating_sub(1800) as usize;
        let mut chunks: Vec<Vec<&TranslationUnit>> = Vec::new();
        let mut cur: Vec<&TranslationUnit> = Vec::new();
        let mut used = 0usize;

        for tu in paras {
            let cur_text = tu
                .final_translation
                .as_deref()
                .or(tu.draft_translation.as_deref())
                .unwrap_or(&tu.frozen_surface);
            let add = tu.frozen_surface.len() + cur_text.len() + 96;
            if !cur.is_empty() && used + add > max_chars {
                chunks.push(cur);
                cur = Vec::new();
                used = 0;
            }
            used += add;
            cur.push(tu);
        }
        if !cur.is_empty() {
            chunks.push(cur);
        }

        let mut model = load_model(&self.cfg, agent_backend)?;
        let mut all: Vec<StitchIssue> = Vec::new();

        for (ci, chunk) in chunks.iter().enumerate() {
            let first = chunk.first().map(|t| t.tu_id).unwrap_or(0);
            let last = chunk.last().map(|t| t.tu_id).unwrap_or(0);

            let tu_block = chunk
                .iter()
                .map(|tu| {
                    let cur = tu
                        .final_translation
                        .as_deref()
                        .or(tu.draft_translation.as_deref())
                        .unwrap_or(&tu.frozen_surface);
                    format!(
                        "TU#{} SOURCE:\n{}\nTU#{} CURRENT:\n{}\n",
                        tu.tu_id, tu.frozen_surface, tu.tu_id, cur
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            let prompt = render_template(
                &self.cfg.prompts.stitch_audit,
                &[("tu_block", &tu_block), ("target_lang", target_lang)],
            );
            let _ = self.trace.write_named_text(
                &format!("stitch_audit.round{round}.chunk{ci}.{first:06}-{last:06}.prompt.txt"),
                &prompt,
            );
            let raw = model.chat(None, &prompt, 2400, 0.15, 0.9, Some(40), Some(1.05), true)?;
            let _ = self.trace.write_named_text(
                &format!("stitch_audit.round{round}.chunk{ci}.{first:06}-{last:06}.output.raw.txt"),
                &raw,
            );

            let parsed =
                parse_json_with_repair(&mut model, &self.cfg.prompts.json_repair, &raw, 1600)?;
            let resp: StitchAuditResponse =
                serde_json::from_value(parsed).context("parse stitch_audit json")?;
            all.extend(resp.issues);
        }

        Ok(all)
    }

    fn run_patch_round(
        &mut self,
        patch_backend: &crate::config::ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        tus: &mut [TranslationUnit],
        notes: &HashMap<usize, ParaNotes>,
        parts_final: &mut HashMap<String, XmlPart>,
        issues: &[StitchIssue],
        round: usize,
    ) -> anyhow::Result<()> {
        let mut model = load_model(&self.cfg, patch_backend)?;
        let repair_tmpl = self.cfg.prompts.translate_repair.clone();
        let idx_by_id: HashMap<usize, usize> =
            tus.iter().enumerate().map(|(i, tu)| (tu.tu_id, i)).collect();

        for issue in issues {
            let Some(&idx) = idx_by_id.get(&issue.tu_id) else {
                continue;
            };
            let before = collect_neighbor_block(tus, notes, idx, -1);
            let after = collect_neighbor_block(tus, notes, idx, 1);

            let source = tus[idx].frozen_surface.clone();
            let current = tus[idx]
                .final_translation
                .clone()
                .or_else(|| tus[idx].draft_translation.clone())
                .unwrap_or_else(|| source.clone());

            let prompt = render_template(
                &self.cfg.prompts.patch,
                &[
                    ("source_lang", source_lang),
                    ("target_lang", target_lang),
                    ("instructions", &issue.rewrite_instructions),
                    ("before", &before),
                    ("source", &source),
                    ("current", &current),
                    ("after", &after),
                ],
            );
            let _ = self
                .trace
                .write_tu_text(tus[idx].tu_id, &format!("patch{round}"), "prompt", &prompt);

            let raw = model.chat(None, &prompt, 1200, 0.2, 0.9, Some(40), Some(1.05), false)?;
            let mut out = cleanup_model_text(&raw);
            if validate_translation(&tus[idx], &out).is_err() {
                let repaired = self.repair_translation(
                    &mut model,
                    &repair_tmpl,
                    source_lang,
                    target_lang,
                    &source,
                    &out,
                )?;
                out = repaired;
            }
            if validate_translation(&tus[idx], &out).is_err() {
                continue;
            }

            tus[idx].final_translation = Some(out.clone());
            apply_translation_unit(parts_final, &tus[idx], &out)
                .with_context(|| format!("apply patched tu_id={}", tus[idx].tu_id))?;
        }

        Ok(())
    }

    fn write_progress_docx(
        &self,
        pkg: &DocxPackage,
        output: &Path,
        parts: &HashMap<String, XmlPart>,
        done: usize,
        total: usize,
    ) -> anyhow::Result<()> {
        let stem = output
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        let mut suffix = self.cfg.autosave_suffix.clone();
        if !suffix.to_ascii_lowercase().ends_with(".docx") {
            suffix.push_str(".docx");
        }
        let progress_path = output.with_file_name(format!("{stem}{suffix}"));
        self.progress.info(format!(
            "Autosave {done}/{total}: {}",
            progress_path.display()
        ));
        write_docx_with_parts(pkg, parts, &progress_path)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ParaNotesChunkResponse {
    #[serde(default)]
    paragraphs: Vec<ParaNotesItem>,
}

#[derive(Clone, Debug, Deserialize)]
struct ParaNotesItem {
    tu_id: usize,
    #[serde(default)]
    understanding: String,
    #[serde(default)]
    proper_nouns: Vec<String>,
    #[serde(default)]
    terms: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct StitchAuditResponse {
    #[serde(default)]
    issues: Vec<StitchIssue>,
}

#[derive(Clone, Debug, Deserialize)]
struct StitchIssue {
    tu_id: usize,
    #[serde(default)]
    problem: String,
    #[serde(default)]
    rewrite_instructions: String,
}

#[derive(Clone, Copy, Debug)]
enum TranslationSlot {
    A,
    B,
}

impl TranslationSlot {
    fn stage_name(&self) -> &'static str {
        match self {
            TranslationSlot::A => "translate_a",
            TranslationSlot::B => "translate_b",
        }
    }
}

fn set_translation_slot(
    tu: &mut TranslationUnit,
    slot: TranslationSlot,
    text: String,
    model: &str,
) {
    match slot {
        TranslationSlot::A => {
            tu.draft_translation = Some(text);
            tu.draft_translation_model = Some(model.to_string());
        }
        TranslationSlot::B => {
            tu.alt_translation = Some(text);
            tu.alt_translation_model = Some(model.to_string());
        }
    }
}

fn load_xml_parts(pkg: &DocxPackage) -> anyhow::Result<HashMap<String, XmlPart>> {
    let mut parts: HashMap<String, XmlPart> = HashMap::new();
    for ent in pkg.xml_entries() {
        let part =
            parse_xml_part(&ent.name, &ent.data).with_context(|| format!("parse xml: {}", ent.name))?;
        parts.insert(ent.name.clone(), part);
    }
    Ok(parts)
}

fn extract_all_units(parts: &HashMap<String, XmlPart>) -> anyhow::Result<(Vec<TranslationUnit>, usize)> {
    let mut names: Vec<String> = parts.keys().cloned().collect();
    names.sort();
    let mut tus: Vec<TranslationUnit> = Vec::new();
    let mut next_id = 1usize;
    for name in names {
        let part = parts.get(&name).expect("part");
        let (mut v, next) =
            extract_translation_units(part, next_id).with_context(|| format!("extract units: {}", name))?;
        next_id = next;
        tus.append(&mut v);
    }
    Ok((tus, next_id))
}

fn load_model(cfg: &PipelineConfig, backend: &crate::config::ResolvedBackend) -> anyhow::Result<NativeChatModel> {
    let threads = backend.threads.unwrap_or(cfg.threads);
    let gpu_layers = backend.gpu_layers.unwrap_or(cfg.gpu_layers);
    NativeChatModel::load(
        &LLAMA_BACKEND,
        NativeModelConfig {
            name: backend.name.clone(),
            model_path: backend.model_path.clone(),
            template_hint: backend.template_hint.clone(),
            ctx_size: backend.ctx_size,
            threads,
            gpu_layers,
            batch_size: backend.batch_size,
            ubatch_size: backend.ubatch_size,
            offload_kqv: backend.offload_kqv,
            seed: 42,
        },
    )
}

fn cleanup_model_text(text: &str) -> String {
    let mut s = text.trim().to_string();
    if s.starts_with("```") {
        if let Some(i) = s.find('\n') {
            s = s[i + 1..].to_string();
        }
        if let Some(end) = s.rfind("```") {
            s = s[..end].to_string();
        }
    }
    s.trim().trim_matches('"').trim().to_string()
}

fn parse_json_with_repair(
    model: &mut NativeChatModel,
    repair_tmpl: &str,
    raw: &str,
    max_tokens: u32,
) -> anyhow::Result<serde_json::Value> {
    if let Ok(v) = must_extract_json_obj(raw) {
        return Ok(v);
    }

    let mut last = raw.to_string();
    for _ in 0..2 {
        let head: String = last.chars().take(8000).collect();
        let prompt = render_template(repair_tmpl, &[("raw", &head)]);
        let out =
            model.chat(None, &prompt, max_tokens, 0.1, 0.9, Some(40), Some(1.05), true)?;
        if let Ok(v) = must_extract_json_obj(&out) {
            return Ok(v);
        }
        last = out;
    }

    must_extract_json_obj(raw)
}

fn neighbor_context_block(
    tus: &[TranslationUnit],
    notes: &HashMap<usize, ParaNotes>,
    idx: usize,
    radius: usize,
) -> String {
    let mut out = String::new();
    if idx >= radius {
        out.push_str(&render_ctx_item(tus, notes, idx - radius));
    }
    if idx + radius < tus.len() {
        out.push_str(&render_ctx_item(tus, notes, idx + radius));
    }
    out
}

fn render_ctx_item(tus: &[TranslationUnit], notes: &HashMap<usize, ParaNotes>, idx: usize) -> String {
    let tu = &tus[idx];
    if !(tu.scope_key.contains("#w:p") || tu.scope_key.contains("#a:p")) {
        return String::new();
    }
    let a = tu
        .draft_translation
        .as_deref()
        .unwrap_or(&tu.frozen_surface);
    let b = tu.alt_translation.as_deref().unwrap_or(a);
    let n = notes.get(&tu.tu_id).cloned().unwrap_or_default();
    let mut block = String::new();
    block.push_str(&format!("TU#{} SOURCE:\n{}\n", tu.tu_id, tu.frozen_surface));
    if let Some(u) = n.understanding.as_ref() {
        let u = u.trim();
        if !u.is_empty() {
            block.push_str(&format!("TU#{} NOTE:\n{}\n", tu.tu_id, u));
        }
    }
    block.push_str(&format!("TU#{} A:\n{}\nTU#{} B:\n{}\n\n", tu.tu_id, a, tu.tu_id, b));
    block
}

fn collect_neighbor_block(
    tus: &[TranslationUnit],
    notes: &HashMap<usize, ParaNotes>,
    idx: usize,
    dir: i32,
) -> String {
    let j = if dir < 0 {
        idx.checked_sub(1)
    } else {
        idx.checked_add(1).filter(|&x| x < tus.len())
    };
    let Some(j) = j else {
        return String::new();
    };
    render_ctx_item(tus, notes, j)
}

fn write_docx_with_parts(pkg: &DocxPackage, parts: &HashMap<String, XmlPart>, output: &Path) -> anyhow::Result<()> {
    let mut replacements: HashMap<String, Vec<u8>> = HashMap::new();
    for (name, part) in parts.iter() {
        verify_structure_unchanged(part).with_context(|| format!("verify structure: {}", name))?;
        let bytes = write_xml_part(part).with_context(|| format!("serialize xml: {}", name))?;
        replacements.insert(name.clone(), bytes);
    }
    pkg.write_with_replacements(output, &replacements)?;
    Ok(())
}

fn write_variant_docx(pkg: &DocxPackage, parts: &HashMap<String, XmlPart>, output: &Path, variant: &str) -> anyhow::Result<PathBuf> {
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let suffix = match variant {
        "A" => "_A.docx",
        "B" => "_B.docx",
        other => return Err(anyhow!("unknown variant: {other}")),
    };
    let out_path = output.with_file_name(format!("{stem}{suffix}"));
    write_docx_with_parts(pkg, parts, &out_path)?;
    Ok(out_path)
}
