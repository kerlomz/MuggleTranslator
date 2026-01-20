use std::path::PathBuf;

use anyhow::Context;
use clap::{CommandFactory, Parser};

use muggle_translator::docx::package::DocxPackage;
use muggle_translator::docx::pure_text::{default_text_output_for, extract_pure_text_json};
use muggle_translator::docx::structure::{default_structure_output_for, extract_structure_json};
use muggle_translator::docx::xml::{parse_xml_part, write_xml_part};
use muggle_translator::docx::decompose::{
    default_outputs_for, extract_mask_json_and_offsets, merge_mask_json_and_offsets,
    verify_docx_roundtrip,
};
use muggle_translator::docx::filter::{filter_docx_with_rules, DocxFilterRules};
use muggle_translator::pipeline::{init_default_config, PipelineConfig, TranslatorPipeline};
use muggle_translator::progress::ConsoleProgress;

#[derive(Parser, Debug)]
#[command(name = "muggle-translator")]
#[command(about = "DOCX translator (LLM backends + agent loop) with format preservation", long_about = None)]
struct Args {
    /// Generate default config + prompt files, then exit
    #[arg(long)]
    init_config: bool,

    /// Directory to write config/prompt files (default: current directory)
    #[arg(long, value_name = "DIR")]
    init_config_dir: Option<PathBuf>,

    /// Overwrite existing config/prompt files when used with --init-config
    #[arg(long)]
    force: bool,

    /// Input .docx (drag-and-drop supported)
    #[arg(value_name = "DOCX")]
    input: Option<PathBuf>,

    /// Output .docx (default: <input_stem>_翻译.docx)
    #[arg(short, long, value_name = "DOCX")]
    output: Option<PathBuf>,

    /// Force source language code (e.g. en, zh)
    #[arg(long)]
    source_lang: Option<String>,

    /// Force target language code (e.g. zh, en)
    #[arg(long)]
    target_lang: Option<String>,

    /// Config file path (default: search for muggle-translator.toml upwards)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Translation backend name from config (e.g. hy_mt, translategemma_12b)
    #[arg(long)]
    translate_backend: Option<String>,

    /// Secondary translation backend name from config (version-B output)
    #[arg(long)]
    alt_translate_backend: Option<String>,

    /// Rewrite backend name from config (used for patch/repair/retranslate)
    #[arg(long, hide = true)]
    rewrite_backend: Option<String>,

    /// Polish backend name from config (used for global polish pass)
    #[arg(long, hide = true)]
    polish_backend: Option<String>,

    /// Optional decision backend name from config (planning/term extraction only)
    #[arg(long)]
    controller_backend: Option<String>,

    /// Translation model GGUF (overrides translate_backend)
    #[arg(long)]
    translate_model: Option<PathBuf>,

    /// Secondary translation model GGUF (overrides alt_translate_backend)
    #[arg(long)]
    alt_translate_model: Option<PathBuf>,

    /// Rewrite model GGUF (overrides rewrite_backend)
    #[arg(long, hide = true)]
    rewrite_model: Option<PathBuf>,

    /// Decision model GGUF (overrides controller_backend)
    #[arg(long, alias = "agent_model")]
    controller_model: Option<PathBuf>,

    /// Threads for llama.cpp (default: -1 = auto)
    #[arg(long)]
    threads: Option<i32>,

    /// GPU layers for llama.cpp (default: -1 = auto/offload as much as possible)
    #[arg(long)]
    gpu_layers: Option<i32>,

    /// Context size for translation model
    #[arg(long)]
    ctx_translate: Option<u32>,

    /// Context size for decision model
    #[arg(long)]
    ctx_controller: Option<u32>,

    /// Process at most N translation units (dev-only)
    #[arg(long)]
    max_tus: Option<usize>,

    /// Only parse + re-serialize DOCX (no translation)
    #[arg(long)]
    roundtrip_only: bool,

    /// Extract pure-text JSON (paragraphs + slot_texts; no LLM)
    #[arg(long, value_name = "JSON")]
    extract_text_json: Option<PathBuf>,

    /// Extract structure tree JSON (hierarchy/list-aware; no LLM)
    #[arg(long, value_name = "JSON")]
    extract_structure_json: Option<PathBuf>,

    /// Extract mask JSON (placeholders only, no LLM)
    #[arg(long, value_name = "JSON")]
    extract_mask_json: Option<PathBuf>,

    /// Extract offsets JSON (slot positions only; no LLM)
    #[arg(long, value_name = "JSON")]
    extract_offsets_json: Option<PathBuf>,

    /// Extract mask blobs binary (required when extracting mask JSON; no LLM)
    #[arg(long, value_name = "BIN")]
    extract_mask_blobs: Option<PathBuf>,

    /// Merge `--merge-mask-json` + `--merge-offsets-json` + `--merge-text-json` into `-o` (no LLM)
    #[arg(long, value_name = "JSON")]
    merge_mask_json: Option<PathBuf>,

    /// Merge `--merge-mask-json` + `--merge-offsets-json` + `--merge-text-json` into `-o` (no LLM)
    #[arg(long, value_name = "JSON")]
    merge_offsets_json: Option<PathBuf>,

    /// Merge input pure-text JSON (must match mask/offsets placeholder_prefix; no LLM)
    #[arg(long, value_name = "JSON")]
    merge_text_json: Option<PathBuf>,

    /// Verify extract->merge restores original (writes `<stem>.mask.json`, `<stem>.offsets.json`, `<stem>.text.json`)
    #[arg(long)]
    verify_extract_merge_json: bool,

    /// Filter DOCX XML (tag cleanup + optional run-merge) using `--filter-rules`, then exit (no LLM)
    #[arg(long)]
    filter_docx: bool,

    /// Filter rules TOML path (default: ./docx-filter-rules.toml)
    #[arg(long, value_name = "TOML")]
    filter_rules: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let progress = ConsoleProgress::new(true);

    if args.init_config {
        let dir = args
            .init_config_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let cfg_path = init_default_config(&dir, args.force).context("init default config")?;
        eprintln!("Wrote config: {}", cfg_path.display());
        return Ok(());
    }

    if let (Some(mask), Some(offsets), Some(text_json)) = (
        args.merge_mask_json.as_ref(),
        args.merge_offsets_json.as_ref(),
        args.merge_text_json.as_ref(),
    )
    {
        let output = args.output.clone().context("missing -o/--output for merge")?;
        merge_mask_json_and_offsets(mask, offsets, text_json, &output)?;
        return Ok(());
    } else if args.merge_mask_json.is_some()
        || args.merge_offsets_json.is_some()
        || args.merge_text_json.is_some()
    {
        return Err(anyhow::anyhow!(
            "merge mode requires: --merge-mask-json, --merge-offsets-json, --merge-text-json, and -o/--output"
        ));
    }

    let input = match args.input {
        Some(p) => p,
        None => {
            let mut cmd = Args::command();
            cmd.print_help().context("print help")?;
            eprintln!(
                "\n\nUSAGE:\n  muggle-translator.exe <input.docx>\n\nTIPS:\n  - You can drag a .docx file onto muggle-translator.exe to translate.\n  - Default config search: muggle-translator.toml (upwards), or set MUGGLE_TRANSLATOR_CONFIG.\n"
            );
            return Ok(());
        }
    };
    let output = match args.output {
        Some(p) => p,
        None => {
            let stem = input
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output")
                .to_string();
            input.with_file_name(format!("{stem}_翻译.docx"))
        }
    };

    if args.filter_docx {
        let rules_path = args
            .filter_rules
            .clone()
            .unwrap_or_else(|| PathBuf::from("docx-filter-rules.toml"));
        let rules = DocxFilterRules::from_toml_path(&rules_path)?;
        filter_docx_with_rules(&input, &output, &rules)?;
        return Ok(());
    }

    if args.extract_text_json.is_some()
        || args.extract_structure_json.is_some()
        || args.extract_mask_json.is_some()
        || args.extract_offsets_json.is_some()
        || args.extract_mask_blobs.is_some()
    {
        if args.extract_mask_blobs.is_some()
            && args.extract_mask_json.is_none()
            && args.extract_offsets_json.is_none()
        {
            return Err(anyhow::anyhow!(
                "--extract-mask-blobs requires --extract-mask-json and/or --extract-offsets-json"
            ));
        }
        if let Some(text_json) = args.extract_text_json.clone() {
            extract_pure_text_json(&input, &text_json)?;
        }
        if let Some(structure_json) = args.extract_structure_json.clone() {
            extract_structure_json(&input, &structure_json)?;
        }
        if args.extract_mask_json.is_some() || args.extract_offsets_json.is_some() {
            let defaults = default_outputs_for(&input);
            let mask_json = args.extract_mask_json.clone().unwrap_or(defaults.mask_json_path);
            let offsets_json = args
                .extract_offsets_json
                .clone()
                .unwrap_or(defaults.offsets_json_path);
            let blobs_bin = args
                .extract_mask_blobs
                .clone()
                .unwrap_or(defaults.blobs_bin_path);
            extract_mask_json_and_offsets(&input, &mask_json, &offsets_json, &blobs_bin)?;
        }
        return Ok(());
    }

    if args.verify_extract_merge_json {
        let mask_defaults = default_outputs_for(&input);
        let text_defaults = default_text_output_for(&input);
        let structure_defaults = default_structure_output_for(&input);
        extract_pure_text_json(&input, &text_defaults.text_json_path)?;
        extract_structure_json(&input, &structure_defaults.structure_json_path)?;
        extract_mask_json_and_offsets(
            &input,
            &mask_defaults.mask_json_path,
            &mask_defaults.offsets_json_path,
            &mask_defaults.blobs_bin_path,
        )?;
        merge_mask_json_and_offsets(
            &mask_defaults.mask_json_path,
            &mask_defaults.offsets_json_path,
            &text_defaults.text_json_path,
            &output,
        )?;
        verify_docx_roundtrip(&input, &output)?;
        return Ok(());
    }

    if args.roundtrip_only {
        let pkg = DocxPackage::read(&input)?;
        let mut replacements: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        for ent in pkg.xml_entries() {
            if ent.data.is_empty() {
                continue;
            }
            let part = parse_xml_part(&ent.name, &ent.data)
                .with_context(|| format!("parse xml: {}", ent.name))?;
            let bytes =
                write_xml_part(&part).with_context(|| format!("serialize xml: {}", ent.name))?;
            replacements.insert(ent.name.clone(), bytes);
        }
        pkg.write_with_replacements(&output, &replacements)?;
        return Ok(());
    }

    let cfg = PipelineConfig::from_paths_and_args(
        &input,
        &output,
        args.config,
        args.translate_backend,
        args.alt_translate_backend,
        args.rewrite_backend,
        args.polish_backend,
        args.controller_backend,
        args.translate_model,
        args.alt_translate_model,
        args.rewrite_model,
        args.controller_model,
        args.source_lang,
        args.target_lang,
        args.threads,
        args.gpu_layers,
        args.ctx_translate,
        args.ctx_controller,
        args.max_tus,
    )
    .context("build config")?;

    let mut pipeline = TranslatorPipeline::new(cfg, progress);
    pipeline.translate_docx(&input, &output)?;
    Ok(())
}
