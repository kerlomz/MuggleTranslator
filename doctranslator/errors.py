from __future__ import annotations


class DocTranslatorError(Exception):
    pass


class DocxParseError(DocTranslatorError):
    pass


class ModelLoadError(DocTranslatorError):
    pass


class TranslationProtocolError(DocTranslatorError):
    pass

