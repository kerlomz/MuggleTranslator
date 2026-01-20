from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path
from typing import Any

import requests

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))


def _default_server_exe() -> Path:
    return REPO_ROOT / "llama-b7772-bin-win-cuda-13.1-x64" / "llama-server.exe"


def _default_model() -> Path:
    return REPO_ROOT / "gemma-3-4b-it.Q6_K.gguf"


def _wait_ready(base_url: str, timeout_s: float) -> None:
    deadline = time.time() + timeout_s
    last_err: Exception | None = None
    while time.time() < deadline:
        try:
            r = requests.get(f"{base_url}/health", timeout=2)
            if r.status_code == 200:
                return
        except Exception as exc:  # noqa: BLE001
            last_err = exc
        time.sleep(0.25)
    raise RuntimeError(f"llama-server not ready: {last_err}")


def _detect_model_id(base_url: str) -> str:
    try:
        r = requests.get(f"{base_url}/v1/models", timeout=5)
        r.raise_for_status()
        data = r.json()
        items = data.get("data") or []
        if items and isinstance(items, list):
            mid = items[0].get("id")
            if isinstance(mid, str) and mid.strip():
                return mid.strip()
    except Exception:  # noqa: BLE001
        pass
    return "llama-cpp"


def _chat(base_url: str, model_id: str, prompt: str, *, max_tokens: int) -> str:
    payload: dict[str, Any] = {
        "model": model_id,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.1,
        "top_p": 0.9,
        "max_tokens": int(max_tokens),
    }
    r = requests.post(f"{base_url}/v1/chat/completions", json=payload, timeout=600)
    r.raise_for_status()
    data = r.json()
    return str(data["choices"][0]["message"]["content"])


def _extract_json_obj(text: str) -> Any:
    start = text.find("{")
    if start == -1:
        raise ValueError("no JSON object start")

    in_string = False
    escape = False
    depth = 0
    end: int | None = None

    for i in range(start, len(text)):
        ch = text[i]
        if in_string:
            if escape:
                escape = False
                continue
            if ch == "\\":
                escape = True
                continue
            if ch == '"':
                in_string = False
            continue

        if ch == '"':
            in_string = True
            continue
        if ch == "{":
            depth += 1
            continue
        if ch == "}":
            depth -= 1
            if depth == 0:
                end = i + 1
                break

    if end is None:
        raise ValueError("no balanced JSON object")

    return json.loads(text[start:end])


def _repair_to_json(base_url: str, model_id: str, raw: str, *, max_tokens: int) -> str:
    head = raw[:6000]
    prompt = (
        "You are a JSON repair tool.\n"
        "Return STRICT JSON only (single JSON object). No markdown. No extra text.\n"
        "If required keys are missing, add them with empty defaults.\n"
        "Do not add new facts.\n\n"
        "BROKEN_OUTPUT:\n"
        f"{head}\n"
    )
    return _chat(base_url, model_id, prompt, max_tokens=max_tokens)


def _extract_paragraphs_from_docx(docx_path: Path, max_items: int) -> list[tuple[int, str]]:
    from doctranslator.docx_package import parse_xml_part, read_docx
    from doctranslator.extract import extract_scopes_from_xml

    paras: list[str] = []
    with read_docx(docx_path) as zin:
        xml_names = [
            info.filename
            for info in zin.infolist()
            if info.filename.lower().endswith(".xml") and info.file_size > 0
        ]
        # Prefer main document first.
        xml_names.sort(key=lambda n: (0 if "word/document" in n.replace("\\", "/") else 1, n))
        for name in xml_names:
            xml = zin.read(name)
            part = parse_xml_part(name, xml)
            scopes = extract_scopes_from_xml(name, part.tree)
            for sc in scopes:
                t = (sc.surface_text or "").strip()
                if not t:
                    continue
                paras.append(t)
                if len(paras) >= max_items:
                    break
            if len(paras) >= max_items:
                break
    return [(i + 1, paras[i]) for i in range(len(paras))]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--server-exe", type=Path, default=_default_server_exe())
    ap.add_argument("--model", type=Path, default=_default_model())
    ap.add_argument("--docx", type=Path, default=REPO_ROOT / "test.docx")
    ap.add_argument("--port", type=int, default=18080)
    ap.add_argument("--ctx", type=int, default=16384)
    ap.add_argument("--gpu-layers", type=int, default=9999)
    ap.add_argument("--max-paras", type=int, default=120)
    ap.add_argument("--timeout", type=float, default=30.0)
    ap.add_argument("--max-tokens", type=int, default=2400)
    args = ap.parse_args()

    if not args.server_exe.exists():
        raise FileNotFoundError(f"llama-server.exe not found: {args.server_exe}")
    if not args.model.exists():
        raise FileNotFoundError(f"model not found: {args.model}")
    if not args.docx.exists():
        raise FileNotFoundError(f"docx not found: {args.docx}")

    base_url = f"http://127.0.0.1:{int(args.port)}"
    out_dir = REPO_ROOT / "_trace" / "prompt_lab"
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")

    cmd = [
        str(args.server_exe),
        "--host",
        "127.0.0.1",
        "--port",
        str(int(args.port)),
        "--model",
        str(args.model),
        "--ctx-size",
        str(int(args.ctx)),
        "--n-gpu-layers",
        str(int(args.gpu_layers)),
        "--threads",
        "-1",
    ]
    proc = subprocess.Popen(
        cmd,
        cwd=str(REPO_ROOT),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )

    try:
        _wait_ready(base_url, float(args.timeout))
        model_id = _detect_model_id(base_url)

        paras = _extract_paragraphs_from_docx(Path(args.docx), int(args.max_paras))
        ids_csv = ",".join(str(i) for i, _ in paras)
        block = "\n\n".join(f"TU#{i}:\n{t}" for i, t in paras)

        prompt_notes = (
            "Return STRICT JSON only.\n"
            "Task: For each SOURCE paragraph TU#id, produce:\n"
            f"- understanding (1 concise sentence in target language)\n"
            "- proper_nouns (strings)\n"
            "- terms (strings)\n\n"
            "Rules:\n"
            f"- paragraphs must cover EXACTLY these tu_ids in this order: {ids_csv}\n"
            "- Do NOT invent content.\n"
            "- Ignore layout control tokens like <<MT_TAB>>/<<MT_BR>>.\n\n"
            "Schema:\n"
            '{"paragraphs":[{"tu_id":1,"understanding":"...","proper_nouns":["..."],"terms":["..."]}]}\n\n'
            "SOURCE_PARAGRAPHS:\n"
            f"{block}\n"
        )

        prompt_hierarchy = (
            "Return STRICT JSON only.\n"
            "Task: Build a hierarchy/outline from the paragraphs.\n"
            "Output sections with title + tu_id ranges.\n\n"
            "Schema:\n"
            '{"sections":[{"level":1,"title":"...","tu_ids":[1,2,3]}]}\n\n'
            "PARAGRAPHS:\n"
            f"{block}\n"
        )

        prompts = {
            "para_notes": prompt_notes,
            "hierarchy": prompt_hierarchy,
        }

        for name, prompt in prompts.items():
            raw = _chat(base_url, model_id, prompt, max_tokens=int(args.max_tokens))
            raw_path = out_dir / f"{ts}.{name}.raw.txt"
            raw_path.write_text(raw, encoding="utf-8")

            parsed: Any | None = None
            err: str | None = None
            try:
                parsed = _extract_json_obj(raw)
            except Exception as exc:  # noqa: BLE001
                err = str(exc)
                try:
                    repaired = _repair_to_json(base_url, model_id, raw, max_tokens=int(args.max_tokens))
                    repaired_path = out_dir / f"{ts}.{name}.repaired.raw.txt"
                    repaired_path.write_text(repaired, encoding="utf-8")
                    parsed = _extract_json_obj(repaired)
                    err = None
                except Exception as exc2:  # noqa: BLE001
                    err = f"{err}; repair_failed={exc2}"

            if parsed is not None:
                json_path = out_dir / f"{ts}.{name}.json"
                json_path.write_text(json.dumps(parsed, ensure_ascii=False, indent=2), encoding="utf-8")
                print(f"[ok] {name}: {json_path}")
            else:
                print(f"[warn] {name}: JSON parse failed: {err} (raw={raw_path})")

    finally:
        try:
            proc.terminate()
        except Exception:  # noqa: BLE001
            pass
        try:
            proc.wait(timeout=10)
        except Exception:  # noqa: BLE001
            try:
                proc.kill()
            except Exception:  # noqa: BLE001
                pass

        if proc.stdout is not None:
            try:
                server_log = proc.stdout.read()
                log_path = out_dir / f"{ts}.llama-server.log.txt"
                log_path.write_text(server_log, encoding="utf-8")
            except Exception:  # noqa: BLE001
                pass

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
