use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use once_cell::sync::Lazy;

use crate::docx::decompose::{
    extract_mask_json_and_offsets, merge_mask_json_and_offsets, OffsetsJson,
};
use crate::docx::filter::{filter_docx_with_rules, DocxFilterRules};
use crate::docx::pure_text::{extract_pure_text, PureTextJson};
use crate::docx::structure::extract_structure_json;
use crate::freezer::{freeze_text, unfreeze_text};
use crate::ir::TranslationUnit;
use crate::models::native::{NativeChatModel, NativeModelConfig};
use crate::progress::ConsoleProgress;
use crate::quality::must_extract_json_obj;
use crate::sentinels::parse_slot_output;
use crate::textutil::{auto_language_pair, is_trivial_sentinel_text, lang_label};
use llama_cpp_2::llama_backend::LlamaBackend;

use super::config::PipelineMode;
use super::docmap::build_para_slot_units;
use super::memory::{build_memory, write_memory_file, ParaNotes};
use super::prompts::render_template;
use super::trace::TraceWriter;
use super::PipelineConfig;

mod basic;
mod notes;
mod segmented;
mod stitch;

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
        Self {
            cfg,
            progress,
            trace,
        }
    }

    pub fn translate_docx(&mut self, input: &Path, output: &Path) -> anyhow::Result<()> {
        match self.cfg.mode {
            PipelineMode::Basic => self.translate_docx_basic(input, output),
            PipelineMode::Full => self.translate_docx_full(input, output),
        }
    }

    fn translate_docx_full(&mut self, input: &Path, output: &Path) -> anyhow::Result<()> {
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

        let para_units = build_para_slot_units(&work_docx, &source_text, &offsets)?;
        let mut tus: Vec<TranslationUnit> = Vec::with_capacity(para_units.len());
        let mut slots_by_tu: HashMap<usize, Vec<usize>> = HashMap::new();
        for p in para_units {
            slots_by_tu.insert(p.tu_id, p.slot_ids.clone());
            let fr = freeze_text(&p.source_surface);
            tus.push(TranslationUnit {
                tu_id: p.tu_id,
                part_name: p.part_name,
                scope_key: p.scope_key,
                para_style: p.para_style,
                atoms: Vec::new(),
                spans: Vec::new(),
                source_surface: p.source_surface,
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

        self.progress
            .info(format!("Extracted {} paragraphs", tus.len()));
        if let Some(max_tus) = self.cfg.max_tus {
            let keep = max_tus.max(1).min(tus.len());
            tus.truncate(keep);
            let max_id = tus.last().map(|t| t.tu_id).unwrap_or(0);
            slots_by_tu.retain(|id, _| *id <= max_id);
            self.progress.info(format!("Max TUs: {keep}"));
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
        let mut text_a: PureTextJson = source_text.clone();
        self.translate_stage(
            &translate_backend,
            &source_lang,
            &target_lang,
            &prompt_translate_a,
            &prompt_translate_repair,
            &mut tus,
            TranslationSlot::A,
            &mut text_a,
            &slots_by_tu,
            &mask_json,
            &offsets_json,
            &autosave_text_json,
            output,
        )?;
        let a_text_json = self.trace.dir().join(format!("{stem}.A.text.json"));
        fs::write(
            &a_text_json,
            serde_json::to_vec_pretty(&text_a).context("serialize A text json")?,
        )
        .with_context(|| format!("write A text json: {}", a_text_json.display()))?;
        let _ = write_variant_docx(&mask_json, &offsets_json, &a_text_json, output, "A");
        self.write_memory_snapshot("afterA", &source_lang, &target_lang, &tus, &notes);

        // Translate B
        if let Some(alt) = self.cfg.alt_translate_backend.clone() {
            let prompt_translate_b = self.cfg.prompts.translate_b.clone();
            let prompt_translate_repair = self.cfg.prompts.translate_repair.clone();
            self.progress.info(format!("Translate B: {}", alt.name));
            let mut text_b: PureTextJson = source_text.clone();
            self.translate_stage(
                &alt,
                &source_lang,
                &target_lang,
                &prompt_translate_b,
                &prompt_translate_repair,
                &mut tus,
                TranslationSlot::B,
                &mut text_b,
                &slots_by_tu,
                &mask_json,
                &offsets_json,
                &autosave_text_json,
                output,
            )?;
            let b_text_json = self.trace.dir().join(format!("{stem}.B.text.json"));
            fs::write(
                &b_text_json,
                serde_json::to_vec_pretty(&text_b).context("serialize B text json")?,
            )
            .with_context(|| format!("write B text json: {}", b_text_json.display()))?;
            let _ = write_variant_docx(&mask_json, &offsets_json, &b_text_json, output, "B");
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

        // Apply final into slot_texts.
        let mut text_final: PureTextJson = source_text;
        for tu in &tus {
            let slots = slots_by_tu.get(&tu.tu_id).cloned().unwrap_or_default();
            if slots.is_empty() {
                continue;
            }
            let t = tu
                .final_translation
                .as_deref()
                .or(tu.draft_translation.as_deref())
                .unwrap_or(&tu.frozen_surface);
            self.apply_slot_translation(&mut text_final, &slots, tu, t)
                .with_context(|| format!("apply final tu_id={}", tu.tu_id))?;
        }
        self.write_progress_docx(
            &mask_json,
            &offsets_json,
            &autosave_text_json,
            output,
            &text_final,
            1,
            1,
        )?;

        // Global stitch audit + patch (2 rounds max)
        if let (Some(agent), Some(rewrite_backend)) = (
            self.cfg.controller_backend.clone(),
            self.cfg.rewrite_backend.clone(),
        ) {
            self.run_stitch_audit_and_patch(
                &agent,
                &rewrite_backend,
                &source_lang,
                &target_lang,
                &mut tus,
                &notes,
                &mut text_final,
                &slots_by_tu,
                &mask_json,
                &offsets_json,
                &autosave_text_json,
                output,
            )?;
        }

        // Write final output
        self.progress
            .info(format!("Write output: {}", output.display()));
        let final_text_json = self.trace.dir().join(format!("{stem}.final.text.json"));
        fs::write(
            &final_text_json,
            serde_json::to_vec_pretty(&text_final).context("serialize final text json")?,
        )
        .with_context(|| format!("write final text json: {}", final_text_json.display()))?;
        merge_mask_json_and_offsets(&mask_json, &offsets_json, &final_text_json, output)?;

        self.write_memory_snapshot("final", &source_lang, &target_lang, &tus, &notes);
        self.progress.info("Done.".to_string());
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
            self.cfg
                .alt_translate_backend
                .as_ref()
                .map(|b| b.name.as_str()),
            self.cfg
                .controller_backend
                .as_ref()
                .map(|b| b.name.as_str()),
            tus,
            notes,
        );
        let path = self
            .trace
            .dir()
            .join(format!("paragraph_memory.{stage}.json"));
        let _ = write_memory_file(&path, &mem);
    }

    #[allow(clippy::too_many_arguments)]
    fn translate_stage(
        &mut self,
        backend: &crate::config::ResolvedBackend,
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

            let tu_id = tus[idx].tu_id;
            let slots = slots_by_tu.get(&tu_id).cloned().unwrap_or_default();
            if is_skip {
                let txt = tus[idx].frozen_surface.clone();
                set_translation_slot(&mut tus[idx], slot, txt.clone(), &backend.name);
                if !slots.is_empty() {
                    self.apply_slot_translation(text_variant, &slots, &tus[idx], &txt)
                        .with_context(|| format!("apply {} tu_id={}", slot.stage_name(), tu_id))?;
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
                    text_variant,
                    slots_by_tu,
                    mask_json,
                    offsets_json,
                    autosave_text_json,
                    output,
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
                text_variant,
                slots_by_tu,
                mask_json,
                offsets_json,
                autosave_text_json,
                output,
                &chunk_indices,
                &mut processed,
            )?;
        }
        Ok(())
    }

    fn apply_slot_translation(
        &self,
        text_json: &mut PureTextJson,
        slot_ids: &[usize],
        tu: &TranslationUnit,
        translated: &str,
    ) -> anyhow::Result<()> {
        if slot_ids.is_empty() {
            return Ok(());
        }

        let mut expected: Vec<usize> = Vec::with_capacity(slot_ids.len() + 1);
        expected.extend_from_slice(slot_ids);
        expected.push(0);

        let segs = parse_slot_output(translated, &expected)
            .with_context(|| format!("tu_id={} parse slot output", tu.tu_id))?;
        let tail = segs.get(&0).map(|s| s.as_str()).unwrap_or("");
        let tail_trim = tail.trim();
        if !tail_trim.is_empty() {
            let preview: String = tail_trim.chars().take(self.cfg.log_max_chars).collect();
            return Err(anyhow!(
                "slot_terminator_has_content tu_id={} tail={}",
                tu.tu_id,
                preview
            ));
        }

        for &slot_id in slot_ids {
            if slot_id == 0 {
                continue;
            }
            let idx = slot_id.saturating_sub(1);
            if idx >= text_json.slot_texts.len() {
                return Err(anyhow!(
                    "slot_id_out_of_range tu_id={} slot_id={} slot_texts_len={}",
                    tu.tu_id,
                    slot_id,
                    text_json.slot_texts.len()
                ));
            }
            let seg = segs.get(&slot_id).cloned().unwrap_or_default();
            let seg = unfreeze_text(&seg, &tu.nt_map);
            text_json.slot_texts[idx] = seg;
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
        must_keep_tokens: &str,
        validation_error: &str,
        nt_map: &str,
    ) -> anyhow::Result<String> {
        let source_lang_label = lang_label(source_lang);
        let target_lang_label = lang_label(target_lang);
        let prompt = render_template(
            repair_tmpl,
            &[
                ("source_lang", &source_lang_label),
                ("target_lang", &target_lang_label),
                ("source", source_frozen),
                ("bad", bad),
                ("must_keep_tokens", must_keep_tokens),
                ("validation_error", validation_error),
                ("nt_map", nt_map),
            ],
        );
        let max_tokens = ((source_frozen.len() as u32) / 2).clamp(512, 4096);
        let out = model.chat(None, &prompt, max_tokens, 0.1, 0.9, Some(40), Some(1.05), false)?;
        Ok(cleanup_model_text(&out))
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

    fn write_progress_docx(
        &self,
        mask_json: &Path,
        offsets_json: &Path,
        autosave_text_json: &Path,
        output: &Path,
        text: &PureTextJson,
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
        let progress_text_json = progress_path.with_extension("text.json");
        fs::write(
            autosave_text_json,
            serde_json::to_vec_pretty(text).context("serialize autosave text json")?,
        )
        .with_context(|| format!("write autosave text json: {}", autosave_text_json.display()))?;
        let _ = fs::write(
            &progress_text_json,
            serde_json::to_vec_pretty(text).context("serialize progress text json")?,
        );
        merge_mask_json_and_offsets(mask_json, offsets_json, autosave_text_json, &progress_path)
    }
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

fn load_model(
    cfg: &PipelineConfig,
    backend: &crate::config::ResolvedBackend,
) -> anyhow::Result<NativeChatModel> {
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
        let out = model.chat(
            None,
            &prompt,
            max_tokens,
            0.1,
            0.9,
            Some(40),
            Some(1.05),
            true,
        )?;
        if let Ok(v) = must_extract_json_obj(&out) {
            return Ok(v);
        }
        last = out;
    }

    must_extract_json_obj(raw)
}

fn write_variant_docx(
    mask_json: &Path,
    offsets_json: &Path,
    text_json: &Path,
    output: &Path,
    variant: &str,
) -> anyhow::Result<PathBuf> {
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
    merge_mask_json_and_offsets(mask_json, offsets_json, text_json, &out_path)?;
    Ok(out_path)
}
