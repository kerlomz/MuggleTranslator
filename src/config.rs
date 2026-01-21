use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::Deserialize;

use crate::models::native::find_file_upwards;

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub pipeline: PipelineSection,
    #[serde(default)]
    pub prompts: PromptsSection,
    #[serde(default)]
    pub models: ModelsSection,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PipelineSection {
    /// Pipeline mode: "basic" or "full".
    ///
    /// - basic: translate slot_texts (+ optional paragraph preview); fastest, minimal models.
    /// - full: enable multi-model + controller + patch loops.
    #[serde(default)]
    pub mode: Option<String>,

    #[serde(default)]
    pub translate_backend: Option<String>,
    #[serde(default)]
    pub alt_translate_backend: Option<String>,
    #[serde(default)]
    pub rewrite_backend: Option<String>,
    #[serde(default)]
    pub polish_backend: Option<String>,
    #[serde(default)]
    pub controller_backend: Option<String>,

    #[serde(default)]
    pub threads: Option<i32>,
    #[serde(default)]
    pub gpu_layers: Option<i32>,

    #[serde(default)]
    pub autosave_every: Option<usize>,
    #[serde(default)]
    pub autosave_suffix: Option<String>,

    #[serde(default)]
    pub trace_dir: Option<String>,
    #[serde(default)]
    pub trace_prompts: Option<bool>,
    #[serde(default)]
    pub log_max_chars: Option<usize>,

    /// Optional DOCX filter rules TOML. When set, the input DOCX is normalized (non-visual tags
    /// stripped + adjacent runs merged) before extraction/translation, to reduce fragmentation.
    #[serde(default)]
    pub docx_filter_rules: Option<String>,

    /// Optional dev-only limiter: process at most N translation units.
    #[serde(default)]
    pub max_tus: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ModelsSection {
    #[serde(default)]
    pub backends: HashMap<String, ModelBackend>,

    /// Preferred directory to locate model files when backend paths are relative.
    /// Can be absolute (recommended) or relative to the config file directory.
    #[serde(default)]
    pub model_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ModelBackend {
    pub path: PathBuf,
    #[serde(default)]
    pub template_hint: Option<String>,
    #[serde(default)]
    pub ctx_size: Option<u32>,
    #[serde(default)]
    pub threads: Option<i32>,
    #[serde(default)]
    pub gpu_layers: Option<i32>,
    #[serde(default)]
    pub batch_size: Option<u32>,
    #[serde(default)]
    pub ubatch_size: Option<u32>,
    #[serde(default)]
    pub offload_kqv: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ResolvedBackend {
    pub name: String,
    pub model_path: PathBuf,
    pub template_hint: Option<String>,
    pub ctx_size: u32,
    pub threads: Option<i32>,
    pub gpu_layers: Option<i32>,
    pub batch_size: Option<u32>,
    pub ubatch_size: Option<u32>,
    pub offload_kqv: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PromptsSection {
    #[serde(default)]
    pub translate_a: Option<String>,
    #[serde(default)]
    pub translate_b: Option<String>,
    #[serde(default)]
    pub translate_repair: Option<String>,
    #[serde(default)]
    pub para_notes: Option<String>,
    #[serde(default)]
    pub json_repair: Option<String>,
    #[serde(default)]
    pub fuse_ab: Option<String>,
    #[serde(default)]
    pub stitch_audit: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
}

pub fn find_default_config(workdir: &Path, filename: &str) -> Option<PathBuf> {
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(p) = find_file_upwards(&cwd, filename, 8) {
            return Some(p);
        }
    }
    if let Some(p) = find_file_upwards(workdir, filename, 8) {
        return Some(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(p) = find_file_upwards(dir, filename, 10) {
                return Some(p);
            }
        }
    }
    None
}

pub fn load_config(path: &Path) -> anyhow::Result<AppConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read config: {}", path.display()))?;
    let cfg: AppConfig = toml::from_str(&text).context("parse config toml")?;
    Ok(cfg)
}

pub fn resolve_backend(
    cfg: &AppConfig,
    config_path: &Path,
    name: &str,
    fallback_search_dir: &Path,
    fallback_filenames: &[&str],
    default_ctx: u32,
    default_template_hint: Option<&str>,
) -> anyhow::Result<ResolvedBackend> {
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    let mut search_dirs: Vec<PathBuf> = Vec::new();
    if let Some(md) = cfg.models.model_dir.as_ref() {
        let mut p = md.clone();
        if p.is_relative() {
            p = config_dir.join(&p);
        }
        search_dirs.push(p);
    }
    if let Ok(cwd) = std::env::current_dir() {
        search_dirs.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            search_dirs.push(dir.to_path_buf());
        }
    }
    search_dirs.push(config_dir.to_path_buf());
    search_dirs.push(fallback_search_dir.to_path_buf());

    let mut seen_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    search_dirs.retain(|d| d.is_dir() && seen_dirs.insert(d.clone()));

    if let Some(b) = cfg.models.backends.get(name) {
        let mut path = b.path.clone();
        if path.is_relative() {
            let mut resolved: Option<PathBuf> = None;
            for dir in &search_dirs {
                let cand = dir.join(&path);
                if cand.exists() {
                    resolved = Some(cand);
                    break;
                }
            }
            path = resolved.ok_or_else(|| {
                anyhow!(
                    "backend {} model not found: {} (searched: {}) (config={})",
                    name,
                    b.path.display(),
                    search_dirs
                        .iter()
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join("; "),
                    config_path.display()
                )
            })?;
        } else if !path.exists() {
            return Err(anyhow!(
                "backend {} model not found: {} (config={})",
                name,
                path.display(),
                config_path.display()
            ));
        }
        let ctx_size = b.ctx_size.unwrap_or(default_ctx);
        let template_hint = b
            .template_hint
            .as_deref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| default_template_hint.map(|s| s.to_string()));
        return Ok(ResolvedBackend {
            name: name.to_string(),
            model_path: path,
            template_hint,
            ctx_size,
            threads: b.threads,
            gpu_layers: b.gpu_layers,
            batch_size: b.batch_size,
            ubatch_size: b.ubatch_size,
            offload_kqv: b.offload_kqv,
        });
    }

    // Fallback: search by known filenames upwards from fallback dir.
    for fname in fallback_filenames {
        for dir in &search_dirs {
            if let Some(p) = find_file_upwards(dir, fname, 8) {
                return Ok(ResolvedBackend {
                    name: name.to_string(),
                    model_path: p,
                    template_hint: default_template_hint.map(|s| s.to_string()),
                    ctx_size: default_ctx,
                    threads: None,
                    gpu_layers: None,
                    batch_size: None,
                    ubatch_size: None,
                    offload_kqv: None,
                });
            }
        }
    }

    Err(anyhow!("backend not configured and not found: {}", name))
}
