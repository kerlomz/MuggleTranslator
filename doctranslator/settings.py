from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path


def _find_project_root(start: Path) -> Path | None:
    start = start.resolve()
    if start.is_file():
        start = start.parent
    for parent in (start, *start.parents):
        if any(parent.glob("*.gguf")):
            return parent
    return None


def _auto_workdir() -> Path:
    candidates = [
        Path.cwd(),
        Path(__file__).resolve().parent,
    ]
    for start in candidates:
        root = _find_project_root(start)
        if root is not None:
            return root
    return Path.cwd()


def _resolve_path(env_value: str, workdir: Path) -> Path:
    raw = Path(env_value)
    if raw.is_absolute() or raw.exists():
        return raw
    alt = workdir / raw
    if alt.exists():
        return alt
    return raw


def _int_env(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None:
        return default
    try:
        return int(str(raw).strip())
    except Exception:  # noqa: BLE001
        return default


def _bool_env(name: str, default: bool) -> bool:
    raw = os.getenv(name)
    if raw is None:
        return default
    return str(raw).strip().lower() not in {"0", "false", "no", "off"}


def _float_env(name: str, default: float) -> float:
    raw = os.getenv(name)
    if raw is None:
        return default
    try:
        return float(str(raw).strip())
    except Exception:  # noqa: BLE001
        return default


@dataclass(frozen=True)
class Settings:
    workdir: Path
    translategemma_gguf: Path
    hy_mt_gguf: Path | None
    gemma3_12b_gguf: Path | None
    gemma3_1b_gguf: Path | None
    embedding_gguf: Path | None
    source_lang_code: str | None
    target_lang_code: str | None
    target_style: str | None
    enable_style_guide: bool
    enable_decision: bool
    decision_min_chars: int
    enable_second_translator: bool
    second_translator_mode: str
    enable_embeddings: bool
    embedding_use_for_rerank: bool
    embedding_use_for_context: bool
    embedding_top_k: int
    embedding_max_chars: int
    embedding_min_sim: float
    llama_n_ctx: int
    llama_n_ctx_translate: int
    llama_n_ctx_gemma: int
    llama_n_ctx_hy_mt: int
    llama_n_ctx_embed: int
    llama_n_threads: int
    llama_n_gpu_layers: int
    llama_n_gpu_layers_embed: int
    llama_seed: int
    llama_verbose: bool
    llama_chat_format: str | None
    llama_chat_format_translate: str | None
    llama_chat_format_gemma: str | None
    llama_chat_format_hy_mt: str | None
    max_input_tokens: int
    max_new_tokens: int
    enable_ape: bool
    progress: bool
    log_tu_samples: bool
    log_tu_max_chars: int
    heartbeat_seconds: float
    log_tu_every: int
    enable_context: bool
    context_max_excerpts: int
    glossary_max_terms: int
    glossary_max_items_per_tu: int
    checkpoint_every: int
    hard_failure_repair_rounds: int
    max_tus: int

    @staticmethod
    def from_env() -> "Settings":
        workdir_env = os.getenv("DOC_TRANSLATOR_WORKDIR")
        workdir = Path(workdir_env) if workdir_env else _auto_workdir()

        # Primary translation model (GGUF).
        #
        # Compatibility:
        # - DOC_TRANSLATOR_TRANSLATEGEMMA_GGUF: legacy env name (kept)
        # - DOC_TRANSLATOR_TRANSLATE_GGUF: preferred env name (new)
        translate_gguf_env = os.getenv("DOC_TRANSLATOR_TRANSLATE_GGUF") or os.getenv(
            "DOC_TRANSLATOR_TRANSLATEGEMMA_GGUF"
        )

        hy_mt_q8 = workdir / "HY-MT1.5-1.8B-Q8_0.gguf"
        hy_mt_q4 = workdir / "HY-MT1.5-1.8B-Q4_K_M.gguf"

        if translate_gguf_env:
            translategemma_gguf = _resolve_path(translate_gguf_env, workdir)
        else:
            # Default: prefer HY-MT if present in repo root; otherwise fall back to TranslateGemma.
            if hy_mt_q8.exists():
                translategemma_gguf = hy_mt_q8
            elif hy_mt_q4.exists():
                translategemma_gguf = hy_mt_q4
            else:
                translategemma_gguf = workdir / "translategemma-12b-it.i1-Q5_K_M.gguf"

        hy_mt_gguf_env = os.getenv("DOC_TRANSLATOR_HY_MT_GGUF")
        hy_mt_gguf = _resolve_path(hy_mt_gguf_env, workdir) if hy_mt_gguf_env else None
        if hy_mt_gguf and not hy_mt_gguf.exists():
            hy_mt_gguf = None
        if hy_mt_gguf is None:
            # Auto-detect a secondary HY-MT model for optional dual-translator setups.
            for cand in (hy_mt_q8, hy_mt_q4):
                if cand.exists() and cand != translategemma_gguf:
                    hy_mt_gguf = cand
                    break

        gemma3_12b_gguf_env = os.getenv("DOC_TRANSLATOR_GEMMA3_12B_GGUF")
        gemma3_12b_gguf = (
            _resolve_path(gemma3_12b_gguf_env, workdir)
            if gemma3_12b_gguf_env
            else (workdir / "gemma-3-12b-it-q4_0.gguf")
        )
        if gemma3_12b_gguf and not gemma3_12b_gguf.exists():
            gemma3_12b_gguf = None

        # Gemma3-1B is optional. Default: disabled unless explicitly configured via env.
        gemma3_1b_gguf_env = os.getenv("DOC_TRANSLATOR_GEMMA3_1B_GGUF")
        gemma3_1b_gguf = _resolve_path(gemma3_1b_gguf_env, workdir) if gemma3_1b_gguf_env else None
        if gemma3_1b_gguf and not gemma3_1b_gguf.exists():
            gemma3_1b_gguf = None

        embedding_gguf_env = os.getenv("DOC_TRANSLATOR_EMBEDDING_GGUF")
        embedding_gguf = _resolve_path(embedding_gguf_env, workdir) if embedding_gguf_env else None
        if embedding_gguf and not embedding_gguf.exists():
            embedding_gguf = None

        source_lang_code = os.getenv("DOC_TRANSLATOR_SOURCE_LANG") or None
        target_lang_code = os.getenv("DOC_TRANSLATOR_TARGET_LANG") or None

        target_style = os.getenv("DOC_TRANSLATOR_TARGET_STYLE")
        target_style = target_style.strip() if isinstance(target_style, str) and target_style.strip() else None
        enable_style_guide = _bool_env("DOC_TRANSLATOR_ENABLE_STYLE_GUIDE", True)
        enable_decision = _bool_env("DOC_TRANSLATOR_ENABLE_DECISION", True)
        decision_min_chars = _int_env("DOC_TRANSLATOR_DECISION_MIN_CHARS", 220)
        if decision_min_chars < 0:
            decision_min_chars = 0

        # Default: single translator (TranslateGemma) + one agent model (Gemma-3-12B).
        enable_second_translator = _bool_env("DOC_TRANSLATOR_ENABLE_SECOND_TRANSLATOR", False)
        second_translator_mode = (os.getenv("DOC_TRANSLATOR_SECOND_TRANSLATOR_MODE") or "off").strip().lower()
        if second_translator_mode not in {"off", "auto", "always"}:
            second_translator_mode = "off"

        # Embeddings are optional and expensive to init on 12B; default off.
        enable_embeddings = _bool_env("DOC_TRANSLATOR_ENABLE_EMBEDDINGS", False)
        embedding_use_for_rerank = _bool_env("DOC_TRANSLATOR_EMBEDDING_RERANK", False)
        embedding_use_for_context = _bool_env("DOC_TRANSLATOR_EMBEDDING_CONTEXT", False)
        embedding_top_k = _int_env("DOC_TRANSLATOR_EMBEDDING_TOP_K", 4)
        if embedding_top_k < 0:
            embedding_top_k = 0
        embedding_max_chars = _int_env("DOC_TRANSLATOR_EMBEDDING_MAX_CHARS", 360)
        if embedding_max_chars < 0:
            embedding_max_chars = 0
        embedding_min_sim = _float_env("DOC_TRANSLATOR_EMBEDDING_MIN_SIM", 0.0)
        if embedding_min_sim < 0:
            embedding_min_sim = 0.0

        llama_n_ctx = _int_env("DOC_TRANSLATOR_LLAMA_N_CTX", 2048)
        if llama_n_ctx < 256:
            llama_n_ctx = 2048

        llama_n_threads = _int_env("DOC_TRANSLATOR_LLAMA_N_THREADS", os.cpu_count() or 8)
        if llama_n_threads < 1:
            llama_n_threads = 1

        llama_n_gpu_layers = _int_env("DOC_TRANSLATOR_LLAMA_N_GPU_LAYERS", -1)
        llama_seed = _int_env("DOC_TRANSLATOR_LLAMA_SEED", 0)
        llama_verbose = _bool_env("DOC_TRANSLATOR_LLAMA_VERBOSE", False)
        llama_chat_format_raw = os.getenv("DOC_TRANSLATOR_LLAMA_CHAT_FORMAT")
        llama_chat_format = (llama_chat_format_raw or "gemma").strip() or None
        if llama_chat_format and llama_chat_format.lower() in {"auto", "none"}:
            llama_chat_format = None

        llama_n_ctx_translate = _int_env("DOC_TRANSLATOR_LLAMA_N_CTX_TRANSLATE", llama_n_ctx)
        if llama_n_ctx_translate < 256:
            llama_n_ctx_translate = llama_n_ctx

        llama_n_ctx_gemma = _int_env("DOC_TRANSLATOR_LLAMA_N_CTX_GEMMA", llama_n_ctx)
        if llama_n_ctx_gemma < 256:
            llama_n_ctx_gemma = llama_n_ctx

        llama_n_ctx_hy_mt = _int_env("DOC_TRANSLATOR_LLAMA_N_CTX_HY_MT", llama_n_ctx_translate)
        if llama_n_ctx_hy_mt < 256:
            llama_n_ctx_hy_mt = llama_n_ctx_translate

        llama_n_ctx_embed = _int_env("DOC_TRANSLATOR_LLAMA_N_CTX_EMBED", 2048)
        if llama_n_ctx_embed < 256:
            llama_n_ctx_embed = 2048

        llama_chat_format_translate_raw = os.getenv("DOC_TRANSLATOR_LLAMA_CHAT_FORMAT_TRANSLATE")
        if llama_chat_format_translate_raw is not None:
            llama_chat_format_translate = llama_chat_format_translate_raw.strip() or None
        else:
            # Default: HY-MT uses its own official chat template (tokenizer.pre=hunyuan);
            # other Gemma-family translation models use the Gemma chat template.
            # Users can override with DOC_TRANSLATOR_LLAMA_CHAT_FORMAT_TRANSLATE=auto/none if they prefer metadata-based guessing.
            is_hy_mt_primary = "hy-mt" in translategemma_gguf.name.lower()
            llama_chat_format_translate = "hunyuan" if is_hy_mt_primary else (llama_chat_format_raw.strip() if llama_chat_format_raw else "gemma")
        if llama_chat_format_translate and llama_chat_format_translate.lower() in {"auto", "none"}:
            llama_chat_format_translate = None

        llama_chat_format_gemma_raw = os.getenv("DOC_TRANSLATOR_LLAMA_CHAT_FORMAT_GEMMA")
        llama_chat_format_gemma = (llama_chat_format_gemma_raw or llama_chat_format_raw or "gemma").strip() or None
        if llama_chat_format_gemma and llama_chat_format_gemma.lower() in {"auto", "none"}:
            llama_chat_format_gemma = None

        llama_chat_format_hy_mt_raw = os.getenv("DOC_TRANSLATOR_LLAMA_CHAT_FORMAT_HY_MT")
        if llama_chat_format_hy_mt_raw is not None:
            llama_chat_format_hy_mt = llama_chat_format_hy_mt_raw.strip() or None
        else:
            # HY-MT uses its own official chat template (tokenizer.pre=hunyuan); default to that template.
            llama_chat_format_hy_mt = "hunyuan" if hy_mt_gguf is not None else None
        if llama_chat_format_hy_mt and llama_chat_format_hy_mt.lower() in {"auto", "none"}:
            llama_chat_format_hy_mt = None

        llama_n_gpu_layers_embed = _int_env("DOC_TRANSLATOR_LLAMA_N_GPU_LAYERS_EMBED", 0)

        max_input_tokens = _int_env("DOC_TRANSLATOR_MAX_INPUT_TOKENS", 1800)
        max_new_tokens = _int_env("DOC_TRANSLATOR_MAX_NEW_TOKENS", 1024)

        # Default off: we use Gemma-3-12B agent review/merge instead of a separate APE pass.
        enable_ape = _bool_env("DOC_TRANSLATOR_ENABLE_APE", False)
        progress = _bool_env("DOC_TRANSLATOR_PROGRESS", True)
        log_tu_samples = _bool_env("DOC_TRANSLATOR_LOG_TU_SAMPLES", True)
        log_tu_max_chars = _int_env("DOC_TRANSLATOR_LOG_TU_MAX_CHARS", 120)

        try:
            heartbeat_seconds = float(os.getenv("DOC_TRANSLATOR_HEARTBEAT_SECONDS", "8"))
        except Exception:  # noqa: BLE001
            heartbeat_seconds = 8.0
        if heartbeat_seconds < 0:
            heartbeat_seconds = 0.0
        log_tu_every = _int_env("DOC_TRANSLATOR_LOG_TU_EVERY", 20)
        if log_tu_every < 1:
            log_tu_every = 1

        enable_context = _bool_env("DOC_TRANSLATOR_ENABLE_CONTEXT", True)
        context_max_excerpts = _int_env("DOC_TRANSLATOR_CONTEXT_MAX_EXCERPTS", 40)
        if context_max_excerpts < 0:
            context_max_excerpts = 0

        glossary_max_terms = _int_env("DOC_TRANSLATOR_GLOSSARY_MAX_TERMS", 40)
        if glossary_max_terms < 0:
            glossary_max_terms = 0

        glossary_max_items_per_tu = _int_env("DOC_TRANSLATOR_GLOSSARY_MAX_ITEMS_PER_TU", 16)
        if glossary_max_items_per_tu < 0:
            glossary_max_items_per_tu = 0

        checkpoint_every = _int_env("DOC_TRANSLATOR_CHECKPOINT_EVERY", 50)
        if checkpoint_every < 0:
            checkpoint_every = 0

        hard_failure_repair_rounds = _int_env("DOC_TRANSLATOR_HARD_FAILURE_REPAIR_ROUNDS", 6)
        if hard_failure_repair_rounds < 0:
            hard_failure_repair_rounds = 0

        max_tus = _int_env("DOC_TRANSLATOR_MAX_TUS", 0)
        if max_tus < 0:
            max_tus = 0

        return Settings(
            workdir=workdir,
            translategemma_gguf=translategemma_gguf,
            hy_mt_gguf=hy_mt_gguf,
            gemma3_12b_gguf=gemma3_12b_gguf,
            gemma3_1b_gguf=gemma3_1b_gguf,
            embedding_gguf=embedding_gguf,
            source_lang_code=source_lang_code,
            target_lang_code=target_lang_code,
            target_style=target_style,
            enable_style_guide=enable_style_guide,
            enable_decision=enable_decision,
            decision_min_chars=decision_min_chars,
            enable_second_translator=enable_second_translator,
            second_translator_mode=second_translator_mode,
            enable_embeddings=enable_embeddings,
            embedding_use_for_rerank=embedding_use_for_rerank,
            embedding_use_for_context=embedding_use_for_context,
            embedding_top_k=embedding_top_k,
            embedding_max_chars=embedding_max_chars,
            embedding_min_sim=embedding_min_sim,
            llama_n_ctx=llama_n_ctx,
            llama_n_ctx_translate=llama_n_ctx_translate,
            llama_n_ctx_gemma=llama_n_ctx_gemma,
            llama_n_ctx_hy_mt=llama_n_ctx_hy_mt,
            llama_n_ctx_embed=llama_n_ctx_embed,
            llama_n_threads=llama_n_threads,
            llama_n_gpu_layers=llama_n_gpu_layers,
            llama_n_gpu_layers_embed=llama_n_gpu_layers_embed,
            llama_seed=llama_seed,
            llama_verbose=llama_verbose,
            llama_chat_format=llama_chat_format,
            llama_chat_format_translate=llama_chat_format_translate,
            llama_chat_format_gemma=llama_chat_format_gemma,
            llama_chat_format_hy_mt=llama_chat_format_hy_mt,
            max_input_tokens=max_input_tokens,
            max_new_tokens=max_new_tokens,
            enable_ape=enable_ape,
            progress=progress,
            log_tu_samples=log_tu_samples,
            log_tu_max_chars=log_tu_max_chars,
            heartbeat_seconds=heartbeat_seconds,
            log_tu_every=log_tu_every,
            enable_context=enable_context,
            context_max_excerpts=context_max_excerpts,
            glossary_max_terms=glossary_max_terms,
            glossary_max_items_per_tu=glossary_max_items_per_tu,
            checkpoint_every=checkpoint_every,
            hard_failure_repair_rounds=hard_failure_repair_rounds,
            max_tus=max_tus,
        )
