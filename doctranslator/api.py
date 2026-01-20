from __future__ import annotations

import sys
from pathlib import Path

if __name__ == "__main__" and __package__ is None:
    repo_root = Path(__file__).resolve().parents[1]
    if str(repo_root) not in sys.path:
        sys.path.insert(0, str(repo_root))

from doctranslator.pipeline import DocxTranslationPipeline
from doctranslator.settings import Settings


def translate_docx(docx_path: str | Path) -> Path:
    settings = Settings.from_env()
    pipeline = DocxTranslationPipeline(settings=settings)
    input_path = Path(docx_path)
    if input_path.suffix.lower() != ".docx":
        raise ValueError("Only .docx is supported")
    output_path = input_path.with_name(f"{input_path.stem}_\u7ffb\u8bd1{input_path.suffix}")
    pipeline.translate_file(input_path=input_path, output_path=output_path)
    return output_path


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(description="Translate a .docx file while preserving formatting.")
    parser.add_argument("docx", nargs="?", default=None, help="Input .docx path")
    args = parser.parse_args()

    if args.docx is None:
        default_docx = Path(__file__).resolve().parents[1] / "test.docx"
        out = translate_docx(default_docx)
    else:
        out = translate_docx(args.docx)
    print(out)
