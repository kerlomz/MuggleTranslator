use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::config::ResolvedBackend;
use crate::docx::pure_text::PureTextJson;
use crate::ir::TranslationUnit;
use crate::quality::validate_translation;

use super::{
    cleanup_model_text, load_model, parse_json_with_repair, render_template, ParaNotes,
    TranslatorPipeline,
};

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

impl TranslatorPipeline {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_stitch_audit_and_patch(
        &mut self,
        agent_backend: &ResolvedBackend,
        patch_backend: &ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        tus: &mut [TranslationUnit],
        notes: &HashMap<usize, ParaNotes>,
        text_final: &mut PureTextJson,
        slots_by_tu: &HashMap<usize, Vec<usize>>,
        mask_json: &Path,
        offsets_json: &Path,
        autosave_text_json: &Path,
        output: &Path,
    ) -> anyhow::Result<()> {
        for round in 1..=2 {
            self.progress.info(format!("Stitch audit round {round}/2"));
            let issues = self.run_stitch_audit_round(agent_backend, target_lang, tus, round)?;
            if issues.is_empty() {
                break;
            }

            self.progress
                .info(format!("Patch issues: {}", issues.len()));
            self.run_patch_round(
                patch_backend,
                source_lang,
                target_lang,
                tus,
                notes,
                text_final,
                slots_by_tu,
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
            let _ = self.write_progress_docx(
                mask_json,
                offsets_json,
                autosave_text_json,
                output,
                text_final,
                round,
                2,
            );
        }
        Ok(())
    }

    fn run_stitch_audit_round(
        &mut self,
        agent_backend: &ResolvedBackend,
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
        patch_backend: &ResolvedBackend,
        source_lang: &str,
        target_lang: &str,
        tus: &mut [TranslationUnit],
        notes: &HashMap<usize, ParaNotes>,
        text_final: &mut PureTextJson,
        slots_by_tu: &HashMap<usize, Vec<usize>>,
        issues: &[StitchIssue],
        round: usize,
    ) -> anyhow::Result<()> {
        let mut model = load_model(&self.cfg, patch_backend)?;
        let repair_tmpl = self.cfg.prompts.translate_repair.clone();
        let idx_by_id: HashMap<usize, usize> = tus
            .iter()
            .enumerate()
            .map(|(i, tu)| (tu.tu_id, i))
            .collect();

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
            let _ = self.trace.write_tu_text(
                tus[idx].tu_id,
                &format!("patch{round}"),
                "prompt",
                &prompt,
            );

            let raw = model.chat(None, &prompt, 1200, 0.2, 0.9, Some(40), Some(1.05), false)?;
            let mut out = cleanup_model_text(&raw);
            if validate_translation(&tus[idx], &out).is_err() {
                let must_keep_tokens = crate::sentinels::must_keep_tokens(&source);
                let validation_error = validate_translation(&tus[idx], &out)
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "patch_invalid".to_string());
                let nt_map = crate::freezer::render_nt_map_for_prompt(&tus[idx].nt_map);
                let repaired = self.repair_translation(
                    &mut model,
                    &repair_tmpl,
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
                continue;
            }

            let slots = slots_by_tu
                .get(&tus[idx].tu_id)
                .cloned()
                .unwrap_or_default();
            if slots.is_empty() {
                continue;
            }

            let mut applied = false;
            for attempt in 0..=1 {
                if validate_translation(&tus[idx], &out).is_err() {
                    if attempt == 0 {
                        let must_keep_tokens = crate::sentinels::must_keep_tokens(&source);
                        let validation_error = validate_translation(&tus[idx], &out)
                            .err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "patch_invalid".to_string());
                        let nt_map = crate::freezer::render_nt_map_for_prompt(&tus[idx].nt_map);
                        let repaired = self.repair_translation(
                            &mut model,
                            &repair_tmpl,
                            source_lang,
                            target_lang,
                            &source,
                            &out,
                            &must_keep_tokens,
                            &validation_error,
                            &nt_map,
                        )?;
                        out = repaired;
                        continue;
                    }
                    break;
                }
                match self.apply_slot_translation(text_final, &slots, &tus[idx], &out) {
                    Ok(()) => {
                        applied = true;
                        break;
                    }
                    Err(_) if attempt == 0 => {
                        let must_keep_tokens = crate::sentinels::must_keep_tokens(&source);
                        let reason = "slot_projection_failed".to_string();
                        let nt_map = crate::freezer::render_nt_map_for_prompt(&tus[idx].nt_map);
                        let repaired = self.repair_translation(
                            &mut model,
                            &repair_tmpl,
                            source_lang,
                            target_lang,
                            &source,
                            &out,
                            &must_keep_tokens,
                            &reason,
                            &nt_map,
                        )?;
                        out = repaired;
                    }
                    Err(_) => break,
                }
            }
            if !applied {
                continue;
            }

            tus[idx].final_translation = Some(out.clone());
        }

        Ok(())
    }
}

fn render_ctx_item(
    tus: &[TranslationUnit],
    notes: &HashMap<usize, ParaNotes>,
    idx: usize,
) -> String {
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
    block.push_str(&format!(
        "TU#{} A:\n{}\nTU#{} B:\n{}\n\n",
        tu.tu_id, a, tu.tu_id, b
    ));
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
