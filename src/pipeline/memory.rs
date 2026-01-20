use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde::Serialize;

use crate::freezer::unfreeze_text;
use crate::ir::{FreezeMaskSpan, TranslationUnit};

#[derive(Clone, Debug, Default)]
pub struct ParaNotes {
    pub understanding: Option<String>,
    pub proper_nouns: Vec<String>,
    pub terms: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ParagraphMemoryFile {
    #[serde(rename = "schema")]
    pub schema_version: String,
    #[serde(rename = "source_lang")]
    pub source_lang: String,
    #[serde(rename = "target_lang")]
    pub target_lang: String,
    #[serde(rename = "model_a")]
    pub model_a: String,
    #[serde(rename = "model_b")]
    pub model_b: Option<String>,
    #[serde(rename = "agent_model")]
    pub agent_model: Option<String>,
    #[serde(rename = "paragraphs")]
    pub paragraphs: Vec<ParagraphRecord>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ParagraphRecord {
    #[serde(rename = "tu_id")]
    pub tu_id: usize,
    #[serde(rename = "part_name")]
    pub part_name: String,
    #[serde(rename = "scope_key")]
    pub scope_key: String,
    #[serde(rename = "para_style")]
    pub para_style: Option<String>,
    #[serde(rename = "tu_kind")]
    pub tu_kind: String,

    #[serde(rename = "原文")]
    pub source_surface: String,
    #[serde(rename = "冻结原文")]
    pub frozen_surface: String,
    #[serde(rename = "不可翻译映射")]
    pub nt_map: HashMap<String, String>,
    #[serde(rename = "不可翻译mask")]
    pub nt_mask: Vec<FreezeMaskSpan>,

    #[serde(rename = "上下文理解")]
    pub understanding: Option<String>,
    #[serde(rename = "专有名词")]
    pub proper_nouns: Vec<String>,
    #[serde(rename = "术语")]
    pub terms: Vec<String>,

    #[serde(rename = "译文A")]
    pub translation_a: Option<String>,
    #[serde(rename = "译文B")]
    pub translation_b: Option<String>,
    #[serde(rename = "最终译文")]
    pub final_translation: Option<String>,
}

pub fn build_memory(
    source_lang: &str,
    target_lang: &str,
    model_a: &str,
    model_b: Option<&str>,
    agent_model: Option<&str>,
    tus: &[TranslationUnit],
    notes: &HashMap<usize, ParaNotes>,
) -> ParagraphMemoryFile {
    let paragraphs = tus
        .iter()
        .map(|tu| {
            let kind = if tu.scope_key.contains("#w:p") || tu.scope_key.contains("#a:p") {
                "paragraph"
            } else {
                "other"
            };

            let n = notes.get(&tu.tu_id).cloned().unwrap_or_default();
            ParagraphRecord {
                tu_id: tu.tu_id,
                part_name: tu.part_name.clone(),
                scope_key: tu.scope_key.clone(),
                para_style: tu.para_style.clone(),
                tu_kind: kind.to_string(),

                source_surface: tu.source_surface.clone(),
                frozen_surface: tu.frozen_surface.clone(),
                nt_map: tu.nt_map.clone(),
                nt_mask: tu.nt_mask.clone(),

                understanding: n.understanding,
                proper_nouns: n.proper_nouns,
                terms: n.terms,

                translation_a: tu
                    .draft_translation
                    .as_deref()
                    .map(|t| unfreeze_text(t, &tu.nt_map)),
                translation_b: tu
                    .alt_translation
                    .as_deref()
                    .map(|t| unfreeze_text(t, &tu.nt_map)),
                final_translation: tu
                    .final_translation
                    .as_deref()
                    .map(|t| unfreeze_text(t, &tu.nt_map)),
            }
        })
        .collect();

    ParagraphMemoryFile {
        schema_version: "mt.paragraph_memory.v1".to_string(),
        source_lang: source_lang.to_string(),
        target_lang: target_lang.to_string(),
        model_a: model_a.to_string(),
        model_b: model_b.map(|s| s.to_string()),
        agent_model: agent_model.map(|s| s.to_string()),
        paragraphs,
    }
}

pub fn write_memory_file(path: &Path, mem: &ParagraphMemoryFile) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(mem).context("serialize paragraph memory")?;
    let mut buf = String::new();
    buf.push('\u{FEFF}');
    buf.push_str(&json);
    std::fs::write(path, buf).with_context(|| format!("write memory: {}", path.display()))?;
    Ok(())
}

