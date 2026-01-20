from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from doctranslator.errors import ModelLoadError

_DEFAULT_REPEAT_PENALTY = 1.10


def _ensure_custom_chat_formats_registered() -> None:
    """
    Register additional chat formats that are not built into llama-cpp-python.

    HY-MT (Tencent Hunyuan translation) provides an official HF chat template in GGUF metadata and
    expects Hunyuan special tokens (e.g. <|startoftext|>, <|extra_0|>, <|eos|>).

    We register a minimal equivalent formatter as `chat_format="hunyuan"` so callers do not need
    to manually configure per-model templates.
    """

    try:
        import llama_cpp.llama_chat_format as cf  # type: ignore[import-not-found]
    except Exception:  # noqa: BLE001
        return

    try:
        reg = cf.LlamaChatCompletionHandlerRegistry()
        if "hunyuan" in getattr(reg, "_chat_handlers", {}):
            return

        def format_hunyuan(messages: list[dict[str, Any]], **_kwargs: Any) -> Any:
            # Mirrors the tokenizer.chat_template shipped with tencent/HY-MT1.5-*:
            # - First system message (if present) is wrapped with <|startoftext|> ... <|extra_4|>
            # - User messages are wrapped with <|startoftext|> ... <|extra_0|>, except when they
            #   immediately follow the head system message (then omit the extra start token).
            # - Assistant messages (if present) are suffixed with <|eos|>
            has_head = True
            parts: list[str] = []
            for i, msg in enumerate(messages):
                role = str(msg.get("role") or "")
                content = msg.get("content") or ""
                if not isinstance(content, str):
                    content = str(content)

                if i == 0:
                    if content == "":
                        has_head = False
                    elif role == "system":
                        content = "<|startoftext|>" + content + "<|extra_4|>"

                if role == "user":
                    if i == 1 and has_head:
                        content = content + "<|extra_0|>"
                    else:
                        content = "<|startoftext|>" + content + "<|extra_0|>"
                elif role == "assistant":
                    content = content + "<|eos|>"

                parts.append(content)

            prompt = "".join(parts)
            return cf.ChatFormatterResponse(prompt=prompt, stop=["<|eos|>"], added_special=True)

        handler = cf.chat_formatter_to_chat_completion_handler(format_hunyuan)
        reg.register_chat_completion_handler("hunyuan", handler)
    except Exception:  # noqa: BLE001
        # Never fail model loading due to optional chat-format registration.
        return


def _import_llama() -> Any:
    try:
        from llama_cpp import Llama  # type: ignore[import-not-found]
    except Exception as exc:  # noqa: BLE001
        raise ModelLoadError(
            "llama-cpp-python is required for GGUF inference. Install it with: pip install llama-cpp-python"
        ) from exc
    _ensure_custom_chat_formats_registered()
    return Llama


def _init_llama(llama_cls: Any, kwargs: dict[str, Any]) -> Any:
    try:
        return llama_cls(**kwargs)
    except TypeError:
        # Compatibility shim for llama-cpp-python version differences.
        pruned = dict(kwargs)
        for key in ("seed", "n_gpu_layers", "n_threads", "n_ctx", "verbose", "chat_format", "embeddings", "embedding"):
            if key in pruned:
                try:
                    probe = dict(pruned)
                    probe.pop(key, None)
                    return llama_cls(**probe)
                except TypeError:
                    pruned.pop(key, None)
                    continue
        raise


def _safe_tokenize(llm: Any, text: str) -> list[int]:
    if not text:
        return []
    data = text.encode("utf-8")
    for kwargs in (
        {"add_bos": False, "special": True},
        {"add_bos": False},
        {},
    ):
        try:
            toks = llm.tokenize(data, **kwargs)
            if isinstance(toks, list):
                return toks
        except TypeError:
            continue
        except Exception:  # noqa: BLE001
            break
    return []


def _llama_chat(
    llm: Any,
    prompt: str,
    *,
    max_tokens: int,
    temperature: float,
    top_p: float,
    top_k: int | None = None,
    stop: list[str] | None = None,
    repeat_penalty: float | None = None,
) -> str | None:
    if not hasattr(llm, "create_chat_completion"):
        return None
    try:
        kwargs: dict[str, Any] = {
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": max_tokens,
            "temperature": temperature,
            "top_p": top_p,
        }
        if top_k is not None:
            kwargs["top_k"] = int(top_k)
        if stop is not None:
            kwargs["stop"] = stop
        if repeat_penalty is not None:
            kwargs["repeat_penalty"] = float(repeat_penalty)
        try:
            out = llm.create_chat_completion(**kwargs)
        except TypeError:
            kwargs.pop("repeat_penalty", None)
            try:
                out = llm.create_chat_completion(**kwargs)
            except TypeError:
                kwargs.pop("stop", None)
                out = llm.create_chat_completion(**kwargs)
        choice0 = (out.get("choices") or [{}])[0]
        msg = choice0.get("message") or {}
        content = msg.get("content")
        return content.strip() if isinstance(content, str) else None
    except Exception:  # noqa: BLE001
        return None


def _llama_complete(
    llm: Any,
    prompt: str,
    *,
    max_tokens: int,
    temperature: float,
    top_p: float,
    top_k: int | None = None,
    stop: list[str] | None = None,
    repeat_penalty: float | None = None,
) -> str:
    kwargs: dict[str, Any] = {"max_tokens": max_tokens, "temperature": temperature, "top_p": top_p}
    if top_k is not None:
        kwargs["top_k"] = int(top_k)
    if stop is not None:
        kwargs["stop"] = stop
    if repeat_penalty is not None:
        kwargs["repeat_penalty"] = float(repeat_penalty)
    try:
        out = llm(prompt, **kwargs)
    except TypeError:
        kwargs.pop("repeat_penalty", None)
        try:
            out = llm(prompt, **kwargs)
        except TypeError:
            out = llm(prompt, max_tokens=max_tokens)
    choice0 = (out.get("choices") or [{}])[0]
    text = choice0.get("text")
    if isinstance(text, str):
        return text.strip()
    return str(text or "").strip()


def _llama_embed(llm: Any, text: str) -> list[float] | None:
    if not text:
        return []
    if hasattr(llm, "create_embedding"):
        for kwargs in (
            {"input": text},
            {"input": [text]},
        ):
            try:
                out = llm.create_embedding(**kwargs)
                data = out.get("data") if isinstance(out, dict) else None
                if isinstance(data, list) and data:
                    emb = data[0].get("embedding") if isinstance(data[0], dict) else None
                    if isinstance(emb, list) and emb:
                        return [float(x) for x in emb]
            except TypeError:
                continue
            except Exception:  # noqa: BLE001
                break
        try:
            out = llm.create_embedding(text)
            data = out.get("data") if isinstance(out, dict) else None
            if isinstance(data, list) and data:
                emb = data[0].get("embedding") if isinstance(data[0], dict) else None
                if isinstance(emb, list) and emb:
                    return [float(x) for x in emb]
        except Exception:  # noqa: BLE001
            return None
    if hasattr(llm, "embed"):
        try:
            emb = llm.embed(text)
            if isinstance(emb, list) and emb:
                return [float(x) for x in emb]
        except Exception:  # noqa: BLE001
            return None
    return None


@dataclass
class ChatModel:
    model_path: Path
    llm: Any
    n_ctx: int
    n_threads: int
    n_gpu_layers: int
    seed: int
    verbose: bool
    chat_format: str | None = None

    @staticmethod
    def load(
        model_path: Path,
        *,
        n_ctx: int,
        n_threads: int,
        n_gpu_layers: int,
        seed: int,
        verbose: bool,
        chat_format: str | None = None,
    ) -> "ChatModel":
        if not model_path.exists():
            raise ModelLoadError(f"Model file not found: {model_path}")
        Llama = _import_llama()
        kwargs: dict[str, Any] = {
            "model_path": str(model_path),
            "n_ctx": int(n_ctx),
            "n_threads": int(n_threads),
            "n_gpu_layers": int(n_gpu_layers),
            "seed": int(seed),
            "verbose": bool(verbose),
        }
        if chat_format:
            kwargs["chat_format"] = str(chat_format)
        try:
            llm = _init_llama(Llama, kwargs)
        except Exception as exc:  # noqa: BLE001
            raise ModelLoadError(f"Failed to load GGUF model from {model_path}: {exc}") from exc
        return ChatModel(
            model_path=model_path,
            llm=llm,
            n_ctx=int(n_ctx),
            n_threads=int(n_threads),
            n_gpu_layers=int(n_gpu_layers),
            seed=int(seed),
            verbose=bool(verbose),
            chat_format=str(chat_format) if chat_format else None,
        )

    def count_tokens(self, text: str) -> int:
        toks = _safe_tokenize(self.llm, text)
        return len(toks) if toks else 0

    def generate(self, prompt: str, max_new_tokens: int, do_sample: bool = False) -> str:
        temperature = 0.0 if not do_sample else 0.7
        top_p = 1.0
        stop = ["<end_of_turn>", "</s>"]
        repeat_penalty = _DEFAULT_REPEAT_PENALTY
        prompt_tokens = self.count_tokens(prompt) or None
        if prompt_tokens is not None and prompt_tokens >= int(self.n_ctx) - 8:
            raise ValueError(f"Requested tokens ({prompt_tokens}) exceed context window of {self.n_ctx}")
        if prompt_tokens is not None:
            max_allowed = max(16, int(self.n_ctx) - prompt_tokens - 16)
            max_new_tokens = min(int(max_new_tokens), max_allowed)
        chat = _llama_chat(
            self.llm,
            prompt,
            max_tokens=max_new_tokens,
            temperature=temperature,
            top_p=top_p,
            stop=stop,
            repeat_penalty=repeat_penalty,
        )
        if chat is not None:
            return chat
        return _llama_complete(
            self.llm,
            prompt,
            max_tokens=max_new_tokens,
            temperature=temperature,
            top_p=top_p,
            stop=stop,
            repeat_penalty=repeat_penalty,
        )


@dataclass
class TranslateGemmaModel:
    model_path: Path
    llm: Any
    n_ctx: int
    n_threads: int
    n_gpu_layers: int
    seed: int
    verbose: bool
    chat_format: str | None = None

    @staticmethod
    def load(
        model_path: Path,
        *,
        n_ctx: int,
        n_threads: int,
        n_gpu_layers: int,
        seed: int,
        verbose: bool,
        chat_format: str | None = None,
    ) -> "TranslateGemmaModel":
        if not model_path.exists():
            raise ModelLoadError(f"Model file not found: {model_path}")
        Llama = _import_llama()
        kwargs: dict[str, Any] = {
            "model_path": str(model_path),
            "n_ctx": int(n_ctx),
            "n_threads": int(n_threads),
            "n_gpu_layers": int(n_gpu_layers),
            "seed": int(seed),
            "verbose": bool(verbose),
        }
        if chat_format:
            kwargs["chat_format"] = str(chat_format)
        try:
            llm = _init_llama(Llama, kwargs)
        except Exception as exc:  # noqa: BLE001
            raise ModelLoadError(f"Failed to load GGUF model from {model_path}: {exc}") from exc
        return TranslateGemmaModel(
            model_path=model_path,
            llm=llm,
            n_ctx=int(n_ctx),
            n_threads=int(n_threads),
            n_gpu_layers=int(n_gpu_layers),
            seed=int(seed),
            verbose=bool(verbose),
            chat_format=str(chat_format) if chat_format else None,
        )

    def count_tokens(self, text: str) -> int:
        toks = _safe_tokenize(self.llm, text)
        return len(toks) if toks else 0

    def translate_text(
        self,
        text: str,
        source_lang_code: str,
        target_lang_code: str,
        max_new_tokens: int,
        *,
        domain: str | None = None,
        doc_type: str | None = None,
        doc_summary: str | None = None,
        target_style: str | None = None,
        style_guide: str | None = None,
        glossary: str | None = None,
        structure_hint: str | None = None,
        neighbor_prev: str | None = None,
        neighbor_next: str | None = None,
        retrieved_context: str | None = None,
        agent_instruction: str | None = None,
        required_numbers: list[str] | None = None,
    ) -> str:
        # Model-specific prompt templates.
        #
        # HY-MT (Tencent/Hunyuan) ships an official prompt guide; for zh<->xx it recommends a
        # Chinese instruction, optionally preceded by context (contextual translation).
        is_hy_mt = (self.chat_format or "").strip().lower() == "hunyuan"
        try:
            meta = getattr(self.llm, "metadata", {}) or {}
            if isinstance(meta, dict):
                tok_pre = str(meta.get("tokenizer.ggml.pre") or "").strip().lower()
                if tok_pre == "hunyuan":
                    is_hy_mt = True
        except Exception:  # noqa: BLE001
            pass

        src = source_lang_code.strip().lower()
        tgt = target_lang_code.strip().lower()
        if src.startswith("en") and tgt.startswith("zh"):
            src_name = "English"
            tgt_name = "Simplified Chinese"
            tgt_native = "简体中文"
        elif src.startswith("zh") and tgt.startswith("en"):
            src_name = "Chinese"
            tgt_name = "English"
            tgt_native = "English"
        else:
            src_name = source_lang_code
            tgt_name = target_lang_code
            tgt_native = target_lang_code

        nums_line = ""
        if required_numbers:
            nums = [str(x).strip() for x in required_numbers if str(x).strip()]
            if nums:
                counts = Counter(nums)
                # Keep multiplicity explicit: hard gate checks Counter(src_digits) == Counter(tgt_digits).
                shown = ", ".join([f"{k}×{v}" if v > 1 else k for k, v in counts.items()])
                nums_line = (
                    "Required digits (must keep as digits; do not invent; keep counts): "
                    + shown
                    + "\n"
                )

        ctx_parts: list[str] = []
        if domain:
            ctx_parts.append(f"domain={str(domain).strip()}")
        if doc_type:
            ctx_parts.append(f"doc_type={str(doc_type).strip()}")
        if doc_summary:
            ctx_parts.append("Document summary (context only)\n" + str(doc_summary).strip())
        if target_style:
            ctx_parts.append(f"target_style={str(target_style).strip()}")
        if style_guide:
            ctx_parts.append("Style guide (must follow)\n" + str(style_guide).strip())
        if glossary:
            ctx_parts.append("Glossary (must follow)\n" + str(glossary).strip())
        if structure_hint:
            ctx_parts.append("Structure hints:\n" + str(structure_hint).strip())
        if neighbor_prev:
            ctx_parts.append("Previous source paragraph (context only):\n" + str(neighbor_prev).strip())
        if neighbor_next:
            ctx_parts.append("Next source paragraph (context only):\n" + str(neighbor_next).strip())
        if retrieved_context:
            ctx_parts.append("Relevant excerpts (context only):\n" + str(retrieved_context).strip())

        if is_hy_mt:
            # Official prompts from tencent/HY-MT1.5-7B README:
            # - ZH<=>XX: Chinese instruction (we only support en<->zh in this project).
            target_language = "中文" if tgt.startswith("zh") else "英语"
            preserve_hint = (
                "请严格保留原文中的所有标签/占位符（例如 ⟦TAB⟧、⟦BR⟧、⟦NT:0001⟧ 等）原样输出，顺序保持一致，"
                "不要删除、不要新增、不要改写。"
            )
            if required_numbers:
                nums = [str(x).strip() for x in required_numbers if str(x).strip()]
                if nums:
                    preserve_hint += " 必须保留以下数字（保持阿拉伯数字不变，不要新增不要删减）：" + ", ".join(nums) + "。"

            hy_ctx_parts: list[str] = []
            if domain or doc_type:
                d = str(domain or "").strip()
                t = str(doc_type or "").strip()
                dt = "；".join([x for x in [f"领域：{d}" if d else "", f"文档类型：{t}" if t else ""] if x])
                if dt:
                    hy_ctx_parts.append(dt)
            if doc_summary:
                hy_ctx_parts.append("摘要（仅供参考）：\n" + str(doc_summary).strip())
            if target_style:
                hy_ctx_parts.append("目标风格：\n" + str(target_style).strip())
            if style_guide:
                hy_ctx_parts.append("翻译风格指南（必须遵循）：\n" + str(style_guide).strip())
            if glossary:
                gl_lines: list[str] = []
                for raw in str(glossary).splitlines():
                    line = raw.strip().lstrip("-*•").strip()
                    if not line:
                        continue
                    if "->" in line:
                        left, right = line.split("->", 1)
                        left = left.strip()
                        right = right.strip()
                        if left and right:
                            gl_lines.append(f"{left} 翻译成 {right}")
                            continue
                    if ":" in line:
                        left, right = line.split(":", 1)
                        left = left.strip()
                        right = right.strip()
                        if left and right:
                            gl_lines.append(f"{left} 翻译成 {right}")
                            continue
                    gl_lines.append(line)
                if gl_lines:
                    hy_ctx_parts.append("术语表（必须遵循）：\n" + "\n".join(gl_lines))
            if structure_hint:
                hy_ctx_parts.append("结构提示（仅供参考）：\n" + str(structure_hint).strip())
            if neighbor_prev:
                hy_ctx_parts.append("上一段（仅供参考）：\n" + str(neighbor_prev).strip())
            if neighbor_next:
                hy_ctx_parts.append("下一段（仅供参考）：\n" + str(neighbor_next).strip())
            if retrieved_context:
                hy_ctx_parts.append("相关摘录（仅供参考）：\n" + str(retrieved_context).strip())

            if hy_ctx_parts:
                ctx_block = "\n\n".join(hy_ctx_parts).strip()
                prompt = (
                    ctx_block
                    + "\n\n"
                    + f"参考上面的信息，把下面的文本翻译成{target_language}，注意不需要翻译上文，也不要额外解释。\n"
                    + (("额外要求（最高优先级）：\n" + str(agent_instruction).strip() + "\n") if agent_instruction else "")
                    + preserve_hint
                    + "\n\n"
                    + str(text or "")
                )
            else:
                prompt = (
                    f"将以下文本翻译为{target_language}，注意只需要输出翻译后的结果，不要额外解释。\n"
                    + (("额外要求（最高优先级）：\n" + str(agent_instruction).strip() + "\n") if agent_instruction else "")
                    + preserve_hint
                    + "\n\n"
                    + str(text or "")
                )

            # Keep output room; drop low-priority context chunks if prompt would exceed n_ctx.
            reserved_out = max(128, min(int(max_new_tokens), int(self.n_ctx) // 3))
            max_prompt_tokens = max(256, int(self.n_ctx) - reserved_out - 64)
            if hy_ctx_parts:
                kept = list(hy_ctx_parts)
                while kept:
                    ctx_block = "\n\n".join(kept).strip()
                    candidate = (
                        ctx_block
                        + "\n\n"
                        + f"参考上面的信息，把下面的文本翻译成{target_language}，注意不需要翻译上文，也不要额外解释。\n"
                        + preserve_hint
                        + "\n\n"
                        + str(text or "")
                    )
                    if self.count_tokens(candidate) <= max_prompt_tokens:
                        prompt = candidate
                        break
                    kept.pop()
                if not kept:
                    prompt = (
                        f"将以下文本翻译为{target_language}，注意只需要输出翻译后的结果，不要额外解释。\n"
                        + preserve_hint
                        + "\n\n"
                        + str(text or "")
                    )

            prompt_tokens = self.count_tokens(prompt) or 0
            if prompt_tokens >= int(self.n_ctx) - 8:
                raise ValueError(f"Requested tokens ({prompt_tokens}) exceed context window of {self.n_ctx}")
            max_allowed = max(16, int(self.n_ctx) - prompt_tokens - 16)
            max_new_tokens = min(int(max_new_tokens), max_allowed)

            # Recommended HY-MT inference params (README): top_k=20, top_p=0.6, repetition_penalty=1.05, temperature=0.7
            temperature = 0.7
            top_p = 0.6
            top_k = 20
            repeat_penalty = 1.05
            stop = ["<|eos|>", "<|extra_0|>"]
            chat = _llama_chat(
                self.llm,
                prompt,
                max_tokens=max_new_tokens,
                temperature=temperature,
                top_p=top_p,
                top_k=top_k,
                stop=stop,
                repeat_penalty=repeat_penalty,
            )
            if chat is not None:
                return chat
            return _llama_complete(
                self.llm,
                prompt,
                max_tokens=max_new_tokens,
                temperature=temperature,
                top_p=top_p,
                top_k=top_k,
                stop=stop,
                repeat_penalty=repeat_penalty,
            )

        ctx_block = ""
        if ctx_parts:
            ctx_block = (
                "Context (for disambiguation only; do not translate this block):\n" + "\n".join(ctx_parts) + "\n\n"
            )

        header = (
            "You are a professional document translator.\n"
            f"Task: Translate from {src_name} to {tgt_name}.\n"
            f"Output language must be {tgt_native}.\n"
            "Output ONLY the translation, no explanations, no notes, no markdown.\n"
            "Do NOT output any prompt labels/sections or metadata lines.\n"
            "Do NOT invent any characters in scripts not present in the source text. If the source contains such scripts (e.g., Korean/Japanese/Russian/Arabic names), preserve them exactly.\n"
            "Do NOT omit any content. Translate EVERYTHING in the source; do not summarize; do not output partial translations.\n"
            "Do NOT add any new information. Do NOT expand the content.\n"
            "Do NOT introduce any new conditions/limitations/exceptions that are not explicitly in the source (e.g., do not add “如果/如/若适用 …” unless the source has such a condition).\n"
            "Do NOT change unconditional obligations into conditional obligations.\n"
            "In legal/contract context, translate “Default” as breach/default event (e.g., 违约/违约事件), not IT “默认”, unless the source clearly refers to default settings/values.\n"
            "Do NOT add any list numbering or section outlines that are not in the source.\n"
            "Preserve all placeholder tokens enclosed in ⟦...⟧ exactly (e.g., ⟦TAB⟧, ⟦BR⟧, ⟦NBH⟧, ⟦SHY⟧, ⟦NT:0001⟧); do not add/remove/reorder them.\n"
            "Preserve standalone variable/party markers like X, Y, Z exactly as-is (do not translate them to 甲/乙) unless the source explicitly maps them.\n"
            "Preserve all Arabic digits (0-9) from the source exactly; do not invent any digits.\n"
            "Do NOT spell out digits using words (e.g., 7 -> 七 or seven is NOT allowed).\n"
            "If the source contains references like \"Section 7\" / \"Article 7\", translate naturally but keep the digit 7 (e.g., \"第7条\").\n\n"
            + (nums_line + "\n" if nums_line else "")
            + (("Agent instruction (highest priority):\n" + str(agent_instruction).strip() + "\n\n") if agent_instruction else "")
            + "If CONTEXT is provided, use it ONLY to disambiguate; do NOT translate the CONTEXT blocks.\n\n"
        )
        text_block = "Text to translate:\n" + f"{text}\n"

        # Keep output room; drop low-priority context chunks if prompt would exceed n_ctx.
        reserved_out = max(128, min(int(max_new_tokens), int(self.n_ctx) // 3))
        max_prompt_tokens = max(256, int(self.n_ctx) - reserved_out - 64)
        if ctx_parts:
            kept = list(ctx_parts)
            while kept:
                prompt = (
                    header
                    + "Context (for disambiguation only; do not translate this block):\n"
                    + "\n".join(kept)
                    + "\n\n"
                    + text_block
                )
                if self.count_tokens(prompt) <= max_prompt_tokens:
                    break
                kept.pop()
            if kept:
                ctx_block = (
                    "Context (for disambiguation only; do not translate this block):\n"
                    + "\n".join(kept)
                    + "\n\n"
                )
            else:
                ctx_block = ""

        prompt = header + ctx_block + text_block
        prompt_tokens = self.count_tokens(prompt) or 0
        if prompt_tokens >= int(self.n_ctx) - 8:
            raise ValueError(f"Requested tokens ({prompt_tokens}) exceed context window of {self.n_ctx}")
        # llama.cpp cannot generate beyond remaining context; clip max_new_tokens accordingly.
        max_allowed = max(16, int(self.n_ctx) - prompt_tokens - 16)
        max_new_tokens = min(int(max_new_tokens), max_allowed)
        temperature = 0.0
        top_p = 1.0
        top_k = None
        repeat_penalty = _DEFAULT_REPEAT_PENALTY
        stop = ["<end_of_turn>", "</s>"]
        chat = _llama_chat(
            self.llm,
            prompt,
            max_tokens=max_new_tokens,
            temperature=temperature,
            top_p=top_p,
            top_k=top_k,
            stop=stop,
            repeat_penalty=repeat_penalty,
        )
        if chat is not None:
            return chat
        return _llama_complete(
            self.llm,
            prompt,
            max_tokens=max_new_tokens,
            temperature=temperature,
            top_p=top_p,
            top_k=top_k,
            stop=stop,
            repeat_penalty=repeat_penalty,
        )


@dataclass
class EmbeddingModel:
    model_path: Path
    llm: Any
    n_ctx: int
    n_threads: int
    n_gpu_layers: int
    seed: int
    verbose: bool

    @staticmethod
    def load(
        model_path: Path,
        *,
        n_ctx: int,
        n_threads: int,
        n_gpu_layers: int,
        seed: int,
        verbose: bool,
    ) -> "EmbeddingModel":
        if not model_path.exists():
            raise ModelLoadError(f"Model file not found: {model_path}")
        Llama = _import_llama()
        kwargs: dict[str, Any] = {
            "model_path": str(model_path),
            "n_ctx": int(n_ctx),
            "n_threads": int(n_threads),
            "n_gpu_layers": int(n_gpu_layers),
            "seed": int(seed),
            "verbose": bool(verbose),
            # llama-cpp-python uses `embeddings=True` (some older builds used `embedding`).
            "embeddings": True,
            "embedding": True,
        }
        try:
            llm = _init_llama(Llama, kwargs)
        except Exception as exc:  # noqa: BLE001
            raise ModelLoadError(f"Failed to load GGUF embedding model from {model_path}: {exc}") from exc
        return EmbeddingModel(
            model_path=model_path,
            llm=llm,
            n_ctx=int(n_ctx),
            n_threads=int(n_threads),
            n_gpu_layers=int(n_gpu_layers),
            seed=int(seed),
            verbose=bool(verbose),
        )

    def count_tokens(self, text: str) -> int:
        toks = _safe_tokenize(self.llm, text)
        return len(toks) if toks else 0

    def embed(self, text: str) -> list[float]:
        prompt_tokens = self.count_tokens(text) or None
        if prompt_tokens is not None and prompt_tokens >= int(self.n_ctx) - 8:
            raise ValueError(f"Requested tokens ({prompt_tokens}) exceed context window of {self.n_ctx}")
        out = _llama_embed(self.llm, text)
        if out is None:
            raise RuntimeError("Embedding generation failed")
        return out
