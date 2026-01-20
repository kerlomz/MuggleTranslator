from __future__ import annotations

import hashlib
import zipfile
from dataclasses import dataclass
from pathlib import Path

from lxml import etree

from doctranslator.errors import DocxParseError


@dataclass(frozen=True)
class XmlPart:
    name: str
    xml_bytes: bytes
    tree: etree._ElementTree
    standalone: bool | None


def _detect_standalone(xml_bytes: bytes) -> bool | None:
    head = xml_bytes[:200].decode("utf-8", errors="ignore")
    if "standalone" not in head:
        return None
    if 'standalone="yes"' in head or "standalone='yes'" in head:
        return True
    if 'standalone="no"' in head or "standalone='no'" in head:
        return False
    return None


def parse_xml_part(name: str, xml_bytes: bytes) -> XmlPart:
    try:
        parser = etree.XMLParser(resolve_entities=False, huge_tree=True, recover=False)
        root = etree.fromstring(xml_bytes, parser=parser)
    except Exception as exc:  # noqa: BLE001
        raise DocxParseError(f"Failed to parse XML part {name}: {exc}") from exc

    standalone = _detect_standalone(xml_bytes)
    return XmlPart(name=name, xml_bytes=xml_bytes, tree=etree.ElementTree(root), standalone=standalone)


def serialize_xml_part(part: XmlPart) -> bytes:
    root = part.tree.getroot()
    return etree.tostring(
        root,
        encoding="UTF-8",
        xml_declaration=True,
        standalone=part.standalone,
    )


def structure_hash(
    tree: etree._ElementTree,
    text_qnames: set[str],
    attr_qnames: set[str],
    attr_pairs: set[tuple[str, str]] | None = None,
) -> str:
    root = tree.getroot()
    cloned = etree.fromstring(etree.tostring(root, encoding="UTF-8"))
    cloned_tree = etree.ElementTree(cloned)

    for elem in cloned_tree.iter():
        tag = etree.QName(elem).text
        if tag in text_qnames:
            elem.text = ""
        for attr_name in list(elem.attrib.keys()):
            qn = etree.QName(attr_name).text
            if qn in attr_qnames:
                del elem.attrib[attr_name]
                continue
            if attr_pairs is not None and (tag, qn) in attr_pairs:
                elem.attrib[attr_name] = ""

    canonical = etree.tostring(cloned_tree, method="c14n")
    return hashlib.sha256(canonical).hexdigest()


def read_docx(path: Path) -> zipfile.ZipFile:
    try:
        return zipfile.ZipFile(path, "r")
    except Exception as exc:  # noqa: BLE001
        raise DocxParseError(f"Failed to read docx: {exc}") from exc


def write_docx(input_zip: zipfile.ZipFile, output_path: Path, replacements: dict[str, bytes]) -> None:
    with zipfile.ZipFile(output_path, "w") as zout:
        for info in input_zip.infolist():
            data = replacements.get(info.filename)
            if data is None:
                data = input_zip.read(info.filename)
            out_info = zipfile.ZipInfo(info.filename, date_time=info.date_time)
            out_info.compress_type = info.compress_type
            out_info.comment = info.comment
            out_info.extra = info.extra
            out_info.external_attr = info.external_attr
            out_info.internal_attr = info.internal_attr
            out_info.create_system = info.create_system
            out_info.create_version = info.create_version
            out_info.extract_version = info.extract_version
            out_info.flag_bits = info.flag_bits
            zout.writestr(out_info, data)
