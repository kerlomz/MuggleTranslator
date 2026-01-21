use std::collections::HashMap;

use anyhow::Context;
use serde::Deserialize;

use crate::config::ResolvedBackend;
use crate::ir::TranslationUnit;
use crate::models::native::NativeChatModel;

use super::{load_model, parse_json_with_repair, render_template, ParaNotes, TranslatorPipeline};

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

impl TranslatorPipeline {
    pub(super) fn run_para_notes(
        &mut self,
        agent_backend: &ResolvedBackend,
        target_lang: &str,
        tus: &[TranslationUnit],
        notes: &mut HashMap<usize, ParaNotes>,
    ) -> anyhow::Result<()> {
        let mut model = load_model(&self.cfg, agent_backend)?;
        let (para_notes_tmpl, json_repair_tmpl) = {
            let prompts = self.cfg.prompts.for_backend(&agent_backend.name);
            (prompts.para_notes.clone(), prompts.json_repair.clone())
        };

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
                self.run_para_notes_chunk(
                    &mut model,
                    &para_notes_tmpl,
                    &json_repair_tmpl,
                    target_lang,
                    &chunk,
                    notes,
                )?;
                chunk.clear();
                used = 0;
            }
            used += add;
            chunk.push(tu);
        }
        if !chunk.is_empty() {
            self.run_para_notes_chunk(
                &mut model,
                &para_notes_tmpl,
                &json_repair_tmpl,
                target_lang,
                &chunk,
                notes,
            )?;
        }
        Ok(())
    }

    fn run_para_notes_chunk(
        &mut self,
        model: &mut NativeChatModel,
        para_notes_tmpl: &str,
        json_repair_tmpl: &str,
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
            para_notes_tmpl,
            &[("target_lang", target_lang), ("tu_block", &tu_block)],
        );
        let _ = self.trace.write_named_text(
            &format!("para_notes.{first:06}-{last:06}.prompt.txt"),
            &prompt,
        );

        let max_tokens = ((chunk.len() as u32) * 140).clamp(900, 3600);
        let raw = model.chat(
            None,
            &prompt,
            max_tokens,
            0.15,
            0.9,
            Some(40),
            Some(1.05),
            true,
        )?;
        let _ = self.trace.write_named_text(
            &format!("para_notes.{first:06}-{last:06}.output.raw.txt"),
            &raw,
        );

        let parsed = match parse_json_with_repair(model, json_repair_tmpl, &raw, 1800) {
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
}
