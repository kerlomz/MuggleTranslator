use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use crate::config::{AppConfig, PromptsSection};

pub const DEFAULT_PROMPTS_DIR: &str = "prompts";

pub const DEFAULT_TRANSLATE_A: &str = "translate_a.txt";
pub const DEFAULT_TRANSLATE_B: &str = "translate_b.txt";
pub const DEFAULT_TRANSLATE_REPAIR: &str = "translate_repair.txt";
pub const DEFAULT_PARA_NOTES: &str = "para_notes.json.txt";
pub const DEFAULT_JSON_REPAIR: &str = "json_repair.txt";
pub const DEFAULT_FUSE_AB: &str = "fuse_ab.txt";
pub const DEFAULT_STITCH_AUDIT: &str = "stitch_audit.json.txt";
pub const DEFAULT_PATCH: &str = "patch.txt";

#[derive(Clone, Debug)]
pub struct PromptSet {
    pub translate_a: String,
    pub translate_b: String,
    pub translate_repair: String,
    pub para_notes: String,
    pub json_repair: String,
    pub fuse_ab: String,
    pub stitch_audit: String,
    pub patch: String,
}

impl PromptSet {
    pub fn load(config_path: &Path, cfg: &AppConfig) -> anyhow::Result<Self> {
        let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        let p = cfg.prompts.clone();
        Ok(Self {
            translate_a: read_prompt(config_dir, &p, "translate_a", DEFAULT_TRANSLATE_A)?,
            translate_b: read_prompt(config_dir, &p, "translate_b", DEFAULT_TRANSLATE_B)?,
            translate_repair: read_prompt(
                config_dir,
                &p,
                "translate_repair",
                DEFAULT_TRANSLATE_REPAIR,
            )?,
            para_notes: read_prompt(config_dir, &p, "para_notes", DEFAULT_PARA_NOTES)?,
            json_repair: read_prompt(config_dir, &p, "json_repair", DEFAULT_JSON_REPAIR)?,
            fuse_ab: read_prompt(config_dir, &p, "fuse_ab", DEFAULT_FUSE_AB)?,
            stitch_audit: read_prompt(config_dir, &p, "stitch_audit", DEFAULT_STITCH_AUDIT)?,
            patch: read_prompt(config_dir, &p, "patch", DEFAULT_PATCH)?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PromptCatalog {
    default: PromptSet,
    by_backend: HashMap<String, PromptSet>,
}

impl PromptCatalog {
    pub fn load(
        config_path: &Path,
        cfg: &AppConfig,
        backend_names: &[String],
    ) -> anyhow::Result<Self> {
        let default = PromptSet::load(config_path, cfg).context("load default prompts")?;
        let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

        let mut by_backend: HashMap<String, PromptSet> = HashMap::new();
        for name in backend_names {
            if by_backend.contains_key(name) {
                continue;
            }
            let Some(backend) = cfg.models.backends.get(name) else {
                continue;
            };
            let overrides = &backend.prompts;
            if prompts_section_is_empty(overrides) {
                continue;
            }
            let mut set = default.clone();
            apply_prompt_overrides(config_dir, name, &mut set, overrides)?;
            by_backend.insert(name.clone(), set);
        }

        Ok(Self {
            default,
            by_backend,
        })
    }

    pub fn for_backend(&self, name: &str) -> &PromptSet {
        self.by_backend.get(name).unwrap_or(&self.default)
    }
}

fn read_prompt(
    config_dir: &Path,
    p: &PromptsSection,
    key: &str,
    default_filename: &str,
) -> anyhow::Result<String> {
    let rel = format!("{DEFAULT_PROMPTS_DIR}/{default_filename}");
    let path = match key {
        "translate_a" => p.translate_a.clone().unwrap_or(rel),
        "translate_b" => p.translate_b.clone().unwrap_or(rel),
        "translate_repair" => p.translate_repair.clone().unwrap_or(rel),
        "para_notes" => p.para_notes.clone().unwrap_or(rel),
        "json_repair" => p.json_repair.clone().unwrap_or(rel),
        "fuse_ab" => p.fuse_ab.clone().unwrap_or(rel),
        "stitch_audit" => p.stitch_audit.clone().unwrap_or(rel),
        "patch" => p.patch.clone().unwrap_or(rel),
        other => return Err(anyhow!("unknown prompt key: {other}")),
    };

    let mut p = PathBuf::from(path);
    if p.is_relative() {
        p = config_dir.join(&p);
    }
    if !p.exists() {
        return Err(anyhow!(
            "prompt file not found for {key}: {} (run: muggle-translator --init-config)",
            p.display()
        ));
    }
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("read prompt: {}", p.display()))?;
    Ok(text)
}

fn read_prompt_path(config_dir: &Path, path: &str, key: &str) -> anyhow::Result<String> {
    let mut p = PathBuf::from(path);
    if p.is_relative() {
        p = config_dir.join(&p);
    }
    if !p.exists() {
        return Err(anyhow!(
            "prompt file not found for {key}: {} (run: muggle-translator --init-config)",
            p.display()
        ));
    }
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("read prompt: {}", p.display()))?;
    Ok(text)
}

fn apply_prompt_overrides(
    config_dir: &Path,
    backend_name: &str,
    out: &mut PromptSet,
    overrides: &PromptsSection,
) -> anyhow::Result<()> {
    let apply = |key: &str, value: &Option<String>, field: &mut String| -> anyhow::Result<()> {
        let Some(path) = value.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        *field = read_prompt_path(config_dir, path, key)
            .with_context(|| format!("backend {backend_name} prompt override for {key}"))?;
        Ok(())
    };

    apply("translate_a", &overrides.translate_a, &mut out.translate_a)?;
    apply("translate_b", &overrides.translate_b, &mut out.translate_b)?;
    apply(
        "translate_repair",
        &overrides.translate_repair,
        &mut out.translate_repair,
    )?;
    apply("para_notes", &overrides.para_notes, &mut out.para_notes)?;
    apply("json_repair", &overrides.json_repair, &mut out.json_repair)?;
    apply("fuse_ab", &overrides.fuse_ab, &mut out.fuse_ab)?;
    apply(
        "stitch_audit",
        &overrides.stitch_audit,
        &mut out.stitch_audit,
    )?;
    apply("patch", &overrides.patch, &mut out.patch)?;

    Ok(())
}

fn prompts_section_is_empty(p: &PromptsSection) -> bool {
    p.translate_a.as_deref().unwrap_or("").trim().is_empty()
        && p.translate_b.as_deref().unwrap_or("").trim().is_empty()
        && p.translate_repair
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        && p.para_notes.as_deref().unwrap_or("").trim().is_empty()
        && p.json_repair.as_deref().unwrap_or("").trim().is_empty()
        && p.fuse_ab.as_deref().unwrap_or("").trim().is_empty()
        && p.stitch_audit.as_deref().unwrap_or("").trim().is_empty()
        && p.patch.as_deref().unwrap_or("").trim().is_empty()
}

pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        let pat = format!("{{{{{k}}}}}");
        out = out.replace(&pat, v);
    }
    out
}

pub fn default_prompt_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (DEFAULT_TRANSLATE_A, DEFAULT_TRANSLATE_A_TEXT),
        (DEFAULT_TRANSLATE_B, DEFAULT_TRANSLATE_B_TEXT),
        (DEFAULT_TRANSLATE_REPAIR, DEFAULT_TRANSLATE_REPAIR_TEXT),
        (DEFAULT_PARA_NOTES, DEFAULT_PARA_NOTES_TEXT),
        (DEFAULT_JSON_REPAIR, DEFAULT_JSON_REPAIR_TEXT),
        (DEFAULT_FUSE_AB, DEFAULT_FUSE_AB_TEXT),
        (DEFAULT_STITCH_AUDIT, DEFAULT_STITCH_AUDIT_TEXT),
        (DEFAULT_PATCH, DEFAULT_PATCH_TEXT),
    ]
}

pub const DEFAULT_TRANSLATE_A_TEXT: &str = r#"Translate from {{source_lang}} to {{target_lang}}.

Rules:
- Do NOT omit content; do NOT summarize.
- Do NOT use ellipsis placeholders like … or ... to skip content.
- Keep ALL tokens like <<MT_...>> unchanged.
- Preserve all digits (0-9) exactly.
- Output ONLY the translated segments, in the same order.
- For each TU id, output EXACTLY:
  <<MT_SEG:000123>>
  ...translation...
  <<MT_END:000123>>
- Do NOT add any other text.

INPUT:
{{tu_block}}"#;

pub const DEFAULT_TRANSLATE_B_TEXT: &str = r#"Translate from {{source_lang}} to {{target_lang}}.

Rules:
- Do NOT omit content; do NOT summarize.
- Do NOT use ellipsis placeholders like … or ... to skip content.
- Keep ALL tokens like <<MT_...>> unchanged.
- Preserve all digits (0-9) exactly.
- Output ONLY the translated segments, in the same order.
- For each TU id, output EXACTLY:
  <<MT_SEG:000123>>
  ...translation...
  <<MT_END:000123>>
- Do NOT add any other text.

INPUT:
{{tu_block}}"#;

pub const DEFAULT_TRANSLATE_REPAIR_TEXT: &str = r#"Fix the translation to satisfy ALL constraints.
Return ONLY the fixed translation.

Constraints:
- Do NOT omit content; do NOT summarize.
- Do NOT add new information.
- Do NOT use ellipsis placeholders like … or ... to skip content.
- Preserve ALL tokens like <<MT_...>> EXACTLY and keep their order unchanged.
- Preserve all digits (0-9) exactly.
- Ensure the output is in {{target_lang}} (do not leave it in {{source_lang}}).

Must-keep tokens (copy exactly; keep order):
{{must_keep_tokens}}

NT map (token = original; you may copy originals, but do NOT translate them):
{{nt_map}}

Validation error (previous output failed):
{{validation_error}}

Language: {{source_lang}} -> {{target_lang}}

SOURCE (frozen):
{{source}}

BAD_OUTPUT:
{{bad}}"#;

pub const DEFAULT_PARA_NOTES_TEXT: &str = r#"Return STRICT JSON only (one JSON object).
For each TU paragraph, output:
- tu_id
- understanding (1 concise sentence in {{target_lang}})
- proper_nouns (strings)
- terms (strings)

Schema:
{"paragraphs":[{"tu_id":1,"understanding":"...","proper_nouns":["..."],"terms":["..."]}]}

PARAGRAPHS:
{{tu_block}}"#;

pub const DEFAULT_JSON_REPAIR_TEXT: &str = r#"You are a JSON repair tool.
Return STRICT JSON only (one JSON object). No markdown. No extra text.
Do not add new facts.
If required keys are missing, add them with empty defaults.

BROKEN_OUTPUT:
{{raw}}"#;

pub const DEFAULT_FUSE_AB_TEXT: &str = r#"You are a translation reviewer.
For each TU segment in INPUT, output ONE best final translation in {{target_lang}}.

Rules:
- Keep ALL tokens like <<MT_...>> unchanged.
- Output ONLY the translated segments, in the same order.
- For each TU id, output EXACTLY:
  <<MT_SEG:000123>>
  ...final translation...
  <<MT_END:000123>>
- Inside each segment: output ONLY the final translation (no labels).
- Do NOT add any other text.

INPUT:
{{tu_block}}"#;

pub const DEFAULT_STITCH_AUDIT_TEXT: &str = r#"Return STRICT JSON only (one JSON object).
Task: Find paragraphs that feel stitched/unnatural or inconsistent with nearby context.
Output issues for TU ids that should be rewritten.

Schema:
{"issues":[{"tu_id":1,"problem":"...","rewrite_instructions":"..."}]}

INPUT:
{{tu_block}}"#;

pub const DEFAULT_PATCH_TEXT: &str = r#"Rewrite the translation of CURRENT paragraph to be natural and consistent.
Keep ALL tokens like <<MT_...>> unchanged.
Return ONLY the rewritten translation for CURRENT.

Language: {{source_lang}} -> {{target_lang}}

INSTRUCTIONS:
{{instructions}}

CONTEXT_BEFORE:
{{before}}

CURRENT_SOURCE:
{{source}}

CURRENT_TRANSLATION:
{{current}}

CONTEXT_AFTER:
{{after}}"#;
