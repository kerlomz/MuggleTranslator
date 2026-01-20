from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))


_CJK_RE = re.compile(r"[\u4e00-\u9fff]")
_LATIN_RE = re.compile(r"[A-Za-z]")


def _extract_scopes(docx_path: Path):
    from doctranslator.docx_package import parse_xml_part, read_docx
    from doctranslator.extract import extract_scopes_from_xml

    scopes = []
    with read_docx(docx_path) as zin:
        xml_entries = [
            info.filename
            for info in zin.infolist()
            if info.filename.lower().endswith(".xml") and info.file_size > 0
        ]
        # Match Rust pipeline part iteration order (sorted by name).
        xml_entries.sort()
        for name in xml_entries:
            part = parse_xml_part(name, zin.read(name))
            scopes.extend(extract_scopes_from_xml(name, part.tree))
    return scopes


def _short(s: str, n: int) -> str:
    s = " ".join((s or "").split())
    if len(s) <= n:
        return s
    return s[:n] + "…"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("input_docx", type=Path)
    ap.add_argument("output_docx", type=Path)
    ap.add_argument("--max-show", type=int, default=20)
    ap.add_argument("--max-chars", type=int, default=220)
    args = ap.parse_args()

    if not args.input_docx.exists():
        raise FileNotFoundError(args.input_docx)
    if not args.output_docx.exists():
        raise FileNotFoundError(args.output_docx)

    src = _extract_scopes(args.input_docx)
    dst = _extract_scopes(args.output_docx)

    if len(src) != len(dst):
        print(f"[fail] scope count mismatch: src={len(src)} dst={len(dst)}")
        return 2

    total = len(src)
    changed = 0
    unchanged = 0
    src_latin = 0
    src_latin_unchanged = 0
    dst_has_cjk = 0

    unchanged_examples = []
    changed_examples = []

    for i, (s, t) in enumerate(zip(src, dst)):
        s_text = (s.surface_text or "").strip()
        t_text = (t.surface_text or "").strip()

        same = s_text == t_text
        if same:
            unchanged += 1
        else:
            changed += 1

        if _LATIN_RE.search(s_text):
            src_latin += 1
            if same:
                src_latin_unchanged += 1
                if len(unchanged_examples) < int(args.max_show):
                    unchanged_examples.append((i + 1, s_text, t_text))

        if _CJK_RE.search(t_text):
            dst_has_cjk += 1
            if not same and len(changed_examples) < int(args.max_show):
                changed_examples.append((i + 1, s_text, t_text))

    print("[ok] docx translation report")
    print(f"- scopes: total={total} changed={changed} unchanged={unchanged}")
    print(
        f"- latin_src: total={src_latin} unchanged={src_latin_unchanged} "
        f"(rate={src_latin_unchanged/max(src_latin,1):.2%})"
    )
    print(f"- dst_has_cjk: {dst_has_cjk} (rate={dst_has_cjk/max(total,1):.2%})")

    if unchanged_examples:
        print("\n[warn] unchanged examples (src contains latin):")
        for i, s_text, t_text in unchanged_examples:
            print(f"- #{i}: {_short(s_text, int(args.max_chars))}")

    if changed_examples:
        print("\n[info] changed examples (dst contains CJK):")
        for i, s_text, t_text in changed_examples:
            print(f"- #{i}: SRC={_short(s_text, int(args.max_chars))}")
            print(f"        DST={_short(t_text, int(args.max_chars))}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
