from __future__ import annotations

import sys
import time
from dataclasses import dataclass
from typing import TextIO


def _fmt_elapsed(seconds: float) -> str:
    seconds = max(0, int(seconds))
    h = seconds // 3600
    m = (seconds % 3600) // 60
    s = seconds % 60
    if h:
        return f"{h:02d}:{m:02d}:{s:02d}"
    return f"{m:02d}:{s:02d}"


def _safe_for_stream(text: str, stream: TextIO) -> str:
    encoding = getattr(stream, "encoding", None)
    if not encoding:
        return text
    try:
        text.encode(encoding)
        return text
    except Exception:  # noqa: BLE001
        return text.encode(encoding, errors="backslashreplace").decode(encoding)


@dataclass
class ConsoleProgress:
    enabled: bool = True
    stream: TextIO = sys.stderr

    def __post_init__(self) -> None:
        self._t0 = time.time()
        self._inline = False
        self._last_len = 0

    def _ensure_newline(self) -> None:
        if self._inline:
            print("", file=self.stream, flush=True)
            self._inline = False
            self._last_len = 0

    def info(self, message: str) -> None:
        if not self.enabled:
            return
        self._ensure_newline()
        ts = _fmt_elapsed(time.time() - self._t0)
        msg = _safe_for_stream(f"[{ts}] {message}", self.stream)
        print(msg, file=self.stream, flush=True)

    def progress(self, label: str, current: int, total: int) -> None:
        if not self.enabled:
            return
        total = max(total, 1)
        current = max(0, min(current, total))
        pct = (current / total) * 100.0
        ts = _fmt_elapsed(time.time() - self._t0)
        line = f"[{ts}] {label} {current}/{total} ({pct:5.1f}%)"
        line = _safe_for_stream(line, self.stream)
        pad = max(0, self._last_len - len(line))
        print("\r" + line + (" " * pad), end="", file=self.stream, flush=True)
        self._inline = True
        self._last_len = len(line)
        if current >= total:
            self._ensure_newline()


@dataclass(frozen=True)
class NullProgress:
    def info(self, message: str) -> None:  # noqa: ARG002
        return

    def progress(self, label: str, current: int, total: int) -> None:  # noqa: ARG002
        return
