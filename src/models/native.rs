use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use encoding_rs::UTF_8;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel, Special};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::DecodeError;

const JSON_GBNF: &str = include_str!("json.gbnf");

#[derive(Clone, Debug)]
pub struct NativeModelConfig {
    pub name: String,
    pub model_path: PathBuf,
    pub template_hint: Option<String>,
    pub ctx_size: u32,
    pub threads: i32,
    pub gpu_layers: i32,
    pub batch_size: Option<u32>,
    pub ubatch_size: Option<u32>,
    pub offload_kqv: Option<bool>,
    pub seed: u32,
}

pub struct NativeChatModel {
    pub name: String,
    pub model_path: PathBuf,
    pub ctx_size: u32,
    pub ctx_train: u32,
    pub threads: i32,
    pub gpu_layers: i32,
    model: Option<Box<LlamaModel>>,
    ctx: Option<LlamaContext<'static>>,
    template: LlamaChatTemplate,
    seed: u32,
}

impl NativeChatModel {
    pub fn load(backend: &LlamaBackend, cfg: NativeModelConfig) -> anyhow::Result<Self> {
        if !cfg.model_path.exists() {
            return Err(anyhow!(
                "{} model not found: {}",
                cfg.name,
                cfg.model_path.display()
            ));
        }

        let mut model_params = LlamaModelParams::default();
        if cfg.gpu_layers == -1 {
            // -1 means "offload as many layers as possible" (llama.cpp treats values > n_layer as all layers).
            model_params = model_params.with_n_gpu_layers(9999);
        } else if cfg.gpu_layers >= 0 {
            model_params = model_params.with_n_gpu_layers(cfg.gpu_layers as u32);
        }

        let model = Box::new(
            LlamaModel::load_from_file(backend, &cfg.model_path, &model_params)
                .with_context(|| format!("load model {}", cfg.model_path.display()))?,
        );
        // Self-referential: `LlamaContext` borrows `LlamaModel`. We keep the model in a `Box`
        // (stable address) and extend the lifetime to `'static` for the context.
        // SAFETY:
        // - The model allocation remains valid as long as `self.model` is `Some`.
        // - We drop `ctx` before `model` in `Drop`.
        let model_ptr: *const LlamaModel = &*model;
        let model_ref: &'static LlamaModel = unsafe { &*model_ptr };

        let ctx_train = model_ref.n_ctx_train();
        let mut ctx_size = cfg.ctx_size;
        if ctx_size == 0 {
            ctx_size = ctx_train.max(4096);
        }
        if ctx_train > 0 && ctx_size > ctx_train {
            ctx_size = ctx_train;
        }
        if ctx_size < 256 {
            ctx_size = 256;
        }

        let mut ctx_params =
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(ctx_size));
        let n_batch: u32 = cfg.batch_size.unwrap_or(512).clamp(8, 65536);
        let mut n_ubatch: u32 = cfg.ubatch_size.unwrap_or(n_batch).clamp(1, 65536);
        if n_ubatch > n_batch {
            n_ubatch = n_batch;
        }
        ctx_params = ctx_params.with_n_batch(n_batch).with_n_ubatch(n_ubatch);
        if let Some(offload) = cfg.offload_kqv {
            ctx_params = ctx_params.with_offload_kqv(offload);
        }
        if cfg.threads > 0 {
            ctx_params = ctx_params.with_n_threads(cfg.threads);
            ctx_params = ctx_params.with_n_threads_batch(cfg.threads);
        }
        let ctx = model_ref
            .new_context(backend, ctx_params)
            .context("create model context")?;

        let template = match model_ref.chat_template(None) {
            Ok(t) => t,
            Err(_) => {
                let hint = cfg.template_hint.as_deref().unwrap_or("chatml");
                LlamaChatTemplate::new(hint).context("build fallback chat template")?
            }
        };

        Ok(Self {
            name: cfg.name,
            model_path: cfg.model_path,
            ctx_size,
            ctx_train,
            threads: cfg.threads,
            gpu_layers: cfg.gpu_layers,
            model: Some(model),
            ctx: Some(ctx),
            template,
            seed: cfg.seed,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn chat(
        &mut self,
        system_prompt: Option<&str>,
        user_prompt: &str,
        max_tokens: u32,
        temperature: f32,
        top_p: f32,
        top_k: Option<u32>,
        repeat_penalty: Option<f32>,
        json_mode: bool,
    ) -> anyhow::Result<String> {
        let mut chat: Vec<LlamaChatMessage> = Vec::new();
        if let Some(s) = system_prompt {
            if !s.trim().is_empty() {
                chat.push(
                    LlamaChatMessage::new("system".to_string(), s.to_string())
                        .context("build system message")?,
                );
            }
        }
        chat.push(
            LlamaChatMessage::new("user".to_string(), user_prompt.to_string())
                .context("build user message")?,
        );

        let prompt = self
            .model_ref()
            .apply_chat_template(&self.template, &chat, true)
            .context("apply chat template")?;

        self.generate_from_prompt(
            &prompt,
            max_tokens,
            temperature,
            top_p,
            top_k,
            repeat_penalty,
            json_mode,
        )
    }

    fn generate_from_prompt(
        &mut self,
        prompt: &str,
        max_tokens: u32,
        temperature: f32,
        top_p: f32,
        top_k: Option<u32>,
        repeat_penalty: Option<f32>,
        json_mode: bool,
    ) -> anyhow::Result<String> {
        self.ctx_mut().clear_kv_cache();

        let add_bos = decide_add_bos(prompt);
        let prompt_tokens = self
            .model_ref()
            .str_to_token(prompt, add_bos)
            .context("tokenize prompt")?;
        if prompt_tokens.is_empty() {
            return Err(anyhow!("empty prompt tokens"));
        }

        let n_ctx = self.ctx_ref().n_ctx() as usize;
        if prompt_tokens.len() + 1 >= n_ctx {
            return Err(anyhow!(
                "prompt_too_long: prompt_tokens={} n_ctx={}",
                prompt_tokens.len(),
                n_ctx
            ));
        }

        let mut max_tokens = max_tokens as usize;
        let available = n_ctx.saturating_sub(prompt_tokens.len() + 1);
        if available == 0 {
            return Err(anyhow!(
                "no room for generation: prompt_tokens={} n_ctx={}",
                prompt_tokens.len(),
                n_ctx
            ));
        }
        max_tokens = max_tokens.min(available);

        let n_batch = self.ctx_ref().n_batch() as usize;
        if n_batch == 0 {
            return Err(anyhow!("invalid n_batch=0"));
        }

        let last_index = prompt_tokens.len() - 1;
        let mut chunk_start = 0;
        while chunk_start < prompt_tokens.len() {
            let chunk_end = (chunk_start + n_batch).min(prompt_tokens.len());
            let chunk = &prompt_tokens[chunk_start..chunk_end];

            let mut batch = LlamaBatch::new(chunk.len().max(512), 1);
            for (i, token) in chunk.iter().copied().enumerate() {
                let pos = (chunk_start + i) as i32;
                let is_last = (chunk_start + i) == last_index;
                batch
                    .add(token, pos, &[0], is_last)
                    .context("batch.add(prompt)")?;
            }

            self.decode_checked(&mut batch, "decode prompt")?;
            chunk_start = chunk_end;
        }

        let mut samplers: Vec<LlamaSampler> = Vec::new();
        let mut use_json_grammar = json_mode;
        if json_mode {
            match LlamaSampler::grammar(self.model_ref(), JSON_GBNF, "root") {
                Ok(s) => samplers.push(s),
                Err(err) => {
                    use_json_grammar = false;
                    eprintln!(
                        "[warn] {}: json grammar unavailable ({err}); continuing without grammar",
                        self.name
                    );
                }
            }
        }
        if let Some(rp) = repeat_penalty {
            samplers.push(LlamaSampler::penalties(64, rp, 0.0, 0.0));
        }
        samplers.push(LlamaSampler::temp(temperature));
        if let Some(k) = top_k {
            samplers.push(LlamaSampler::top_k(k as i32));
        }
        samplers.push(LlamaSampler::top_p(top_p, 1));
        samplers.push(if temperature <= 0.0 {
            LlamaSampler::greedy()
        } else {
            LlamaSampler::dist(self.seed)
        });
        let mut sampler = LlamaSampler::chain_simple(samplers);
        if !use_json_grammar {
            sampler.accept_many(&prompt_tokens);
        }

        let mut decoder = UTF_8.new_decoder();
        let mut out = String::new();

        let mut batch = LlamaBatch::new(512, 1);
        let mut n_cur: i32 = prompt_tokens.len() as i32;
        for _ in 0..max_tokens {
            let token = sampler.sample(self.ctx_ref(), -1);

            if self.model_ref().is_eog_token(token) {
                break;
            }

            let bytes = self
                .model_ref()
                .token_to_bytes(token, Special::Tokenize)
                .context("token_to_bytes")?;
            let mut piece = String::with_capacity(32);
            let _ = decoder.decode_to_string(&bytes, &mut piece, false);
            out.push_str(&piece);

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .context("batch.add(gen)")?;
            n_cur += 1;
            self.decode_checked(&mut batch, "decode(gen)")?;
        }

        // Flush decoder state.
        let mut tail = String::new();
        let _ = decoder.decode_to_string(&[], &mut tail, true);
        out.push_str(&tail);

        Ok(out.trim().to_string())
    }

    fn decode_checked(&mut self, batch: &mut LlamaBatch, stage: &str) -> anyhow::Result<()> {
        self.ctx_mut().decode(batch).map_err(|err| match err {
            DecodeError::Unknown(-2) => anyhow!(
                "llama_decode threw a foreign exception (likely OOM) (model={}, stage={})",
                self.name,
                stage
            ),
            other => anyhow!(other),
        })?;
        Ok(())
    }

    fn model_ref(&self) -> &LlamaModel {
        self.ctx_ref().model
    }

    fn ctx_ref(&self) -> &LlamaContext<'static> {
        self.ctx
            .as_ref()
            .expect("NativeChatModel ctx missing (use-after-drop)")
    }

    fn ctx_mut(&mut self) -> &mut LlamaContext<'static> {
        self.ctx
            .as_mut()
            .expect("NativeChatModel ctx missing (use-after-drop)")
    }
}

impl Drop for NativeChatModel {
    fn drop(&mut self) {
        // `LlamaContext` holds a reference to `LlamaModel`.
        // Drop the context first, then the model.
        let _ = self.ctx.take();
        let _ = self.model.take();
    }
}

fn decide_add_bos(prompt: &str) -> AddBos {
    let p = prompt.trim_start();
    // Heuristic: if the template already starts with a BOS-like special token, don't add another.
    if p.starts_with("<s>")
        || p.starts_with("<|begin_of_text|>")
        || p.starts_with("<bos>")
        || p.starts_with("<BOS>")
        || p.starts_with("<|startoftext|>")
    {
        AddBos::Never
    } else {
        AddBos::Always
    }
}

pub fn find_file_upwards(start_dir: &Path, filename: &str, max_levels: usize) -> Option<PathBuf> {
    let mut dir = start_dir;
    for _ in 0..=max_levels {
        let candidate = dir.join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
    None
}
