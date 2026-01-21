use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::config::{
    find_default_config, load_config, resolve_backend, AppConfig, ResolvedBackend,
};
use crate::pipeline::prompts::{default_prompt_files, PromptCatalog, DEFAULT_PROMPTS_DIR};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipelineMode {
    Basic,
    Full,
}

impl PipelineMode {
    pub fn parse(s: Option<&str>) -> Self {
        match s.unwrap_or("basic").trim().to_ascii_lowercase().as_str() {
            "full" => Self::Full,
            _ => Self::Basic,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub workdir: PathBuf,
    pub config_path: PathBuf,

    pub mode: PipelineMode,

    pub translate_backend: ResolvedBackend,
    pub alt_translate_backend: Option<ResolvedBackend>,
    pub rewrite_backend: Option<ResolvedBackend>,
    pub controller_backend: Option<ResolvedBackend>,

    pub threads: i32,
    pub gpu_layers: i32,
    pub source_lang: Option<String>,
    pub target_lang: Option<String>,

    pub autosave_every: usize,
    pub autosave_suffix: String,
    pub trace_dir: PathBuf,
    pub trace_prompts: bool,
    pub log_max_chars: usize,
    pub max_tus: Option<usize>,

    pub docx_filter_rules: Option<PathBuf>,

    pub prompts: PromptCatalog,
}

impl PipelineConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn from_paths_and_args(
        input: &Path,
        output: &Path,
        config_path: Option<PathBuf>,
        translate_backend: Option<String>,
        alt_translate_backend: Option<String>,
        rewrite_backend: Option<String>,
        _polish_backend: Option<String>,
        controller_backend: Option<String>,
        translate_model: Option<PathBuf>,
        alt_translate_model: Option<PathBuf>,
        rewrite_model: Option<PathBuf>,
        controller_model: Option<PathBuf>,
        source_lang: Option<String>,
        target_lang: Option<String>,
        threads: Option<i32>,
        gpu_layers: Option<i32>,
        _ctx_translate: Option<u32>,
        _ctx_controller: Option<u32>,
        max_tus: Option<usize>,
    ) -> anyhow::Result<Self> {
        let workdir = input
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let workdir = workdir.canonicalize().unwrap_or(workdir);

        let cfg_file = config_path
            .clone()
            .or_else(|| {
                std::env::var("MUGGLE_TRANSLATOR_CONFIG")
                    .ok()
                    .map(PathBuf::from)
            })
            .or_else(|| find_default_config(&workdir, "muggle-translator.toml"));

        let mut file_cfg = AppConfig::default();
        if let Some(p) = cfg_file.as_ref() {
            if p.exists() {
                file_cfg = load_config(p)?;
            }
        }
        let cfg_path = cfg_file
            .clone()
            .unwrap_or_else(|| workdir.join("muggle-translator.toml"));

        let mode = PipelineMode::parse(file_cfg.pipeline.mode.as_deref());

        let translate_backend_name = translate_backend
            .or_else(|| file_cfg.pipeline.translate_backend.clone())
            .unwrap_or_else(|| "translategemma_4b".to_string());
        let alt_translate_backend_name = if mode == PipelineMode::Full {
            alt_translate_backend
                .or_else(|| file_cfg.pipeline.alt_translate_backend.clone())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        let rewrite_backend_name = if mode == PipelineMode::Full {
            Some(
                rewrite_backend
                    .or_else(|| file_cfg.pipeline.rewrite_backend.clone())
                    .unwrap_or_else(|| "translategemma_12b".to_string()),
            )
        } else {
            None
        };
        let controller_backend_name = if mode == PipelineMode::Full {
            controller_backend
                .or_else(|| file_cfg.pipeline.controller_backend.clone())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };

        let output_dir = output
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| workdir.clone());
        let trace_dir = file_cfg
            .pipeline
            .trace_dir
            .clone()
            .unwrap_or_else(|| "_trace".to_string());
        let trace_dir = if Path::new(&trace_dir).is_absolute() {
            PathBuf::from(trace_dir)
        } else {
            output_dir.join(trace_dir)
        };
        let trace_prompts = file_cfg.pipeline.trace_prompts.unwrap_or(true);
        let log_max_chars = file_cfg.pipeline.log_max_chars.unwrap_or(240);
        let autosave_every = file_cfg.pipeline.autosave_every.unwrap_or(10).max(1);
        let autosave_suffix = file_cfg
            .pipeline
            .autosave_suffix
            .clone()
            .unwrap_or_else(|| "_进度.docx".to_string());
        let max_tus = max_tus.or(file_cfg.pipeline.max_tus).filter(|n| *n > 0);

        let docx_filter_rules = file_cfg
            .pipeline
            .docx_filter_rules
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .map(|p| {
                if p.is_relative() {
                    cfg_path.parent().unwrap_or_else(|| Path::new(".")).join(p)
                } else {
                    p
                }
            });

        let threads = threads.or(file_cfg.pipeline.threads).unwrap_or(-1);
        let gpu_layers = gpu_layers.or(file_cfg.pipeline.gpu_layers).unwrap_or(-1);

        let model_dir = file_cfg
            .models
            .model_dir
            .clone()
            .unwrap_or_else(|| workdir.clone());

        let resolve_with_override = |name: &str, override_path: Option<PathBuf>, default_ctx| {
            if let Some(p) = override_path {
                return Ok(ResolvedBackend {
                    name: name.to_string(),
                    model_path: p,
                    template_hint: None,
                    ctx_size: default_ctx,
                    threads: None,
                    gpu_layers: None,
                    batch_size: None,
                    ubatch_size: None,
                    offload_kqv: None,
                });
            }
            resolve_backend(
                &file_cfg,
                &cfg_path,
                name,
                &model_dir,
                &[],
                default_ctx,
                None,
            )
        };

        let translate_backend =
            resolve_with_override(&translate_backend_name, translate_model, 8192)?;
        let alt_translate_backend = match alt_translate_backend_name.as_deref() {
            Some(n) => Some(resolve_with_override(n, alt_translate_model, 4096)?),
            None => None,
        };
        let rewrite_backend = match rewrite_backend_name.as_deref() {
            Some(n) => Some(resolve_with_override(n, rewrite_model, 8192)?),
            None => None,
        };
        let controller_backend = match controller_backend_name.as_deref() {
            Some(n) => Some(resolve_with_override(n, controller_model, 16384)?),
            None => None,
        };

        let mut prompt_backends: Vec<String> = Vec::new();
        prompt_backends.push(translate_backend.name.clone());
        if let Some(b) = alt_translate_backend.as_ref() {
            prompt_backends.push(b.name.clone());
        }
        if let Some(b) = rewrite_backend.as_ref() {
            prompt_backends.push(b.name.clone());
        }
        if let Some(b) = controller_backend.as_ref() {
            prompt_backends.push(b.name.clone());
        }
        prompt_backends.sort();
        prompt_backends.dedup();
        let prompts =
            PromptCatalog::load(&cfg_path, &file_cfg, &prompt_backends).context("load prompts")?;

        Ok(Self {
            workdir,
            config_path: cfg_path,
            mode,
            translate_backend,
            alt_translate_backend,
            rewrite_backend,
            controller_backend,
            threads,
            gpu_layers,
            source_lang,
            target_lang,
            autosave_every,
            autosave_suffix,
            trace_dir,
            trace_prompts,
            log_max_chars,
            max_tus,
            docx_filter_rules,
            prompts,
        })
    }
}

pub fn init_default_config(dir: &Path, force: bool) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create config dir: {}", dir.display()))?;
    let cfg_path = dir.join("muggle-translator.toml");

    let prompts_dir = dir.join(DEFAULT_PROMPTS_DIR);
    std::fs::create_dir_all(&prompts_dir)
        .with_context(|| format!("create prompts dir: {}", prompts_dir.display()))?;

    for (fname, body) in default_prompt_files() {
        let p = prompts_dir.join(fname);
        if p.exists() && !force {
            continue;
        }
        std::fs::write(&p, body).with_context(|| format!("write prompt: {}", p.display()))?;
    }

    // Optional per-backend prompt templates (referenced by commented sections in the config).
    let backend_prompts: [(&str, &str); 4] = [
        (
            "backends/hy_mt/translate_a.txt",
            include_str!("../../prompts/backends/hy_mt/translate_a.txt"),
        ),
        (
            "backends/hy_mt/translate_repair.txt",
            include_str!("../../prompts/backends/hy_mt/translate_repair.txt"),
        ),
        (
            "backends/translategemma/translate_a.txt",
            include_str!("../../prompts/backends/translategemma/translate_a.txt"),
        ),
        (
            "backends/translategemma/translate_repair.txt",
            include_str!("../../prompts/backends/translategemma/translate_repair.txt"),
        ),
    ];
    for (rel, body) in backend_prompts {
        let p = prompts_dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create prompts dir: {}", parent.display()))?;
        }
        if p.exists() && !force {
            continue;
        }
        std::fs::write(&p, body).with_context(|| format!("write prompt: {}", p.display()))?;
    }

    let filter_rules_path = dir.join("docx-filter-rules.toml");
    if !filter_rules_path.exists() || force {
        std::fs::write(&filter_rules_path, DEFAULT_DOCX_FILTER_RULES_TOML)
            .with_context(|| format!("write docx filter rules: {}", filter_rules_path.display()))?;
    }

    if cfg_path.exists() && !force {
        return Ok(cfg_path);
    }

    let cfg_text = r#"[pipeline]
mode = "basic"
 translate_backend = "hy_mt"
# In "basic" mode, ONLY translate_backend is used.
# Switch to mode="full" to enable these additional stages:
# alt_translate_backend = "hy_mt"
# rewrite_backend = "translategemma_12b"
# controller_backend = "gemma3_4b"

threads = -1
gpu_layers = -1

autosave_every = 10
autosave_suffix = "_进度.docx"

trace_dir = "_trace"
trace_prompts = true
log_max_chars = 240
docx_filter_rules = "docx-filter-rules.toml"

[prompts]
translate_a = "prompts/translate_a.txt"
translate_b = "prompts/translate_b.txt"
translate_repair = "prompts/translate_repair.txt"
para_notes = "prompts/para_notes.json.txt"
json_repair = "prompts/json_repair.txt"
fuse_ab = "prompts/fuse_ab.txt"
stitch_audit = "prompts/stitch_audit.json.txt"
patch = "prompts/patch.txt"

[models]
model_dir = "."

[models.backends.hy_mt]
path = "HY-MT1.5-1.8B-Q8_0.gguf"
template_hint = "hunyuan-dense"
ctx_size = 4096
gpu_layers = -1
batch_size = 512
ubatch_size = 512
offload_kqv = true

# Optional: bind prompts to this backend (different models follow different prompt styles).
# [models.backends.hy_mt.prompts]
# translate_a = "prompts/backends/hy_mt/translate_a.txt"
# translate_b = "prompts/backends/hy_mt/translate_a.txt"
# translate_repair = "prompts/backends/hy_mt/translate_repair.txt"

[models.backends.translategemma_4b]
path = "translategemma-4b-it.i1-Q5_K_S.gguf"
template_hint = "gemma"
ctx_size = 8192
gpu_layers = -1
batch_size = 512
ubatch_size = 512
offload_kqv = true

# Optional:
# [models.backends.translategemma_4b.prompts]
# translate_a = "prompts/backends/translategemma/translate_a.txt"
# translate_b = "prompts/backends/translategemma/translate_a.txt"
# translate_repair = "prompts/backends/translategemma/translate_repair.txt"

[models.backends.translategemma_12b]
path = "translategemma-12b-it.i1-Q6_K.gguf"
template_hint = "gemma"
ctx_size = 8192
gpu_layers = -1
batch_size = 512
ubatch_size = 512
offload_kqv = true

[models.backends.gemma3_4b]
path = "gemma-3-4b-it.Q6_K.gguf"
template_hint = "gemma"
ctx_size = 16384
gpu_layers = -1
batch_size = 512
ubatch_size = 512
offload_kqv = true
"#;

    std::fs::write(&cfg_path, cfg_text)
        .with_context(|| format!("write config: {}", cfg_path.display()))?;
    Ok(cfg_path)
}

const DEFAULT_DOCX_FILTER_RULES_TOML: &str = r#"version = 1

# ---------------------
# Revision/meta cleanup
# ---------------------
# These attributes affect change-tracking/session metadata only.
# Removing them does not change visible layout.
strip_attributes = [
  "w14:paraId",
  "w14:textId",
  "w:rsidR",
  "w:rsidRDefault",
  "w:rsidP",
  "w:rsidRPr",
  "w:rsidDel",
  "w:rsidSect",
]

# ----------------
# Non-visual marks
# ----------------
drop_elements = [
  # Proofing/spellcheck markers (non-visual)
  "w:proofErr",
]

# ---------------------------
# Run-level micro-typography
# ---------------------------
# These run properties often exist only to fine-tune spacing (e.g., each word/space becomes a run),
# which makes slot_texts extremely fragmented. We drop them and then merge adjacent runs that have
# the same remaining rPr fingerprint.
drop_run_properties = [
  # Character spacing (CT_SignedTwipsMeasure)
  "w:spacing",
  # Text scale (percentage; CT_TextScale)
  "w:w",
  # Kerning (CT_HpsMeasure)
  "w:kern",
]

# Remove whitespace-only XML text nodes unless inside these elements.
# This strips pretty-printing/newlines in XML parts like [Content_Types].xml and styles.xml.
preserve_whitespace_text_in = [
  "w:t",
  "w:delText",
  "w:instrText",
  "a:t",
]

# Merge adjacent <w:r> runs (only when they are simple text runs: rPr? + t)
# after applying drop_run_properties. This reduces fragmentation while keeping major formatting.
merge_adjacent_runs = true

# Which parts should run-merge be applied to.
# (We still apply attribute/whitespace cleanup to all XML parts.)
merge_run_parts = [
  "word/document.xml",
  "word/header*.xml",
  "word/footer*.xml",
]
"#;
