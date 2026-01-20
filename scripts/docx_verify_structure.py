from __future__ import annotations

import argparse
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))


def _hash_docx(docx_path: Path) -> tuple[set[str], dict[str, str]]:
    from doctranslator.docx_package import parse_xml_part, read_docx, structure_hash

    with read_docx(docx_path) as zin:
        entries = {i.filename for i in zin.infolist()}
        xml_entries = [
            i.filename
            for i in zin.infolist()
            if i.filename.lower().endswith(".xml") and i.file_size > 0
        ]

        parts = {}
        for name in xml_entries:
            part = parse_xml_part(name, zin.read(name))
            parts[name] = part.tree

        w = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
        a = "http://schemas.openxmlformats.org/drawingml/2006/main"
        text_qnames = {f"{{{w}}}t", f"{{{w}}}delText", f"{{{a}}}t"}
        attr_qnames = {"{http://www.w3.org/XML/1998/namespace}space"}
        attr_pairs = {(f"{{{w}}}lvlText", f"{{{w}}}val")}

        hashes = {}
        for name, tree in parts.items():
            hashes[name] = structure_hash(
                tree, text_qnames=text_qnames, attr_qnames=attr_qnames, attr_pairs=attr_pairs
            )
        return entries, hashes


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("input_docx", type=Path)
    ap.add_argument("output_docx", type=Path)
    args = ap.parse_args()

    if not args.input_docx.exists():
        raise FileNotFoundError(args.input_docx)
    if not args.output_docx.exists():
        raise FileNotFoundError(args.output_docx)

    in_entries, in_hashes = _hash_docx(args.input_docx)
    out_entries, out_hashes = _hash_docx(args.output_docx)

    missing = sorted(in_entries - out_entries)
    extra = sorted(out_entries - in_entries)
    if missing:
        print(f"[fail] missing zip entries: {len(missing)}")
        for n in missing[:50]:
            print(f"  - {n}")
    if extra:
        print(f"[fail] extra zip entries: {len(extra)}")
        for n in extra[:50]:
            print(f"  - {n}")

    mismatched = []
    for name, h in in_hashes.items():
        h2 = out_hashes.get(name)
        if h2 is None:
            continue
        if h != h2:
            mismatched.append(name)

    if mismatched:
        print(f"[fail] structure mismatched xml parts: {len(mismatched)}")
        for n in mismatched[:80]:
            print(f"  - {n}")
        return 2

    if missing or extra:
        return 2

    print("[ok] structure unchanged for all xml parts")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
