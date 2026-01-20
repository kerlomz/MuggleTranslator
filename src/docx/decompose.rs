use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, DateTime};

use crate::docx::package::{DocxEntry, DocxPackage};
use crate::docx::pure_text::PureTextJson;
use crate::docx::xml::{full_hash, parse_xml_part, write_xml_part, XmlEvent, XmlPart};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    Text,
    CData,
    Attr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextSlot {
    pub id: usize,
    pub part_name: String,
    pub kind: SlotKind,
    pub event_index: usize,
    pub attr_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OffsetsJson {
    pub version: u32,
    pub placeholder_prefix: String,
    pub slots: Vec<TextSlot>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "value", rename_all = "snake_case")]
pub enum MaskEntryData {
    Utf8(String),
    Base64(String),
    External(MaskBlobRef),
    Empty,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaskBlobRef {
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaskEntryJson {
    pub name: String,
    pub compression: u16,
    pub last_modified: (u16, u16),
    pub unix_mode: Option<u32>,
    pub is_dir: bool,
    pub data: MaskEntryData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaskJson {
    pub version: u32,
    pub placeholder_prefix: String,
    pub blobs_file: Option<String>,
    pub entries: Vec<MaskEntryJson>,
}

pub struct MaskOutputs {
    pub mask_json_path: PathBuf,
    pub offsets_json_path: PathBuf,
    pub blobs_bin_path: PathBuf,
}

fn hash_file_prefix(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read file: {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hex = hex::encode(hasher.finalize());
    Ok(hex.chars().take(10).collect())
}

fn placeholder(prefix: &str, id: usize) -> String {
    format!("__MT_MASK_{prefix}_{id:08}__")
}

fn is_placeholder(s: &str, prefix: &str) -> bool {
    let pfx = format!("__MT_MASK_{prefix}_");
    s.starts_with(&pfx) && s.ends_with("__") && s.len() >= pfx.len() + 8 + 2
}

fn find_attr_mut<'a>(attrs: &'a mut Vec<(String, String)>, key: &str) -> Option<&'a mut String> {
    attrs
        .iter_mut()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

fn verify_part_mask_pure(part: &XmlPart, prefix: &str) -> anyhow::Result<()> {
    for ev in &part.events {
        match ev {
            XmlEvent::Text { text } | XmlEvent::CData { text } => {
                if !is_placeholder(text, prefix) {
                    return Err(anyhow!(
                        "mask not pure: found non-placeholder text in {}: {:?}",
                        part.name,
                        text
                    ));
                }
            }
            XmlEvent::Start { name, attrs } | XmlEvent::Empty { name, attrs } => {
                if name == "w:lvlText" {
                    if let Some(v) = attrs.iter().find(|(k, _)| k == "w:val").map(|(_, v)| v) {
                        if !is_placeholder(v, prefix) {
                            return Err(anyhow!(
                                "mask not pure: found non-placeholder w:lvlText@w:val in {}: {:?}",
                                part.name,
                                v
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn compression_to_code(m: CompressionMethod) -> u16 {
    match m {
        CompressionMethod::Stored => 0,
        CompressionMethod::Deflated => 8,
        #[allow(deprecated)]
        CompressionMethod::Unsupported(code) => code,
        #[allow(unreachable_patterns)]
        _ => 0,
    }
}

fn compression_from_code(code: u16) -> CompressionMethod {
    match code {
        0 => CompressionMethod::Stored,
        8 => CompressionMethod::DEFLATE,
        #[allow(deprecated)]
        other => CompressionMethod::Unsupported(other),
    }
}

fn blob_path_for_json(mask_json: &Path, blobs_bin: &Path) -> anyhow::Result<String> {
    let mask_dir = mask_json.parent().unwrap_or_else(|| Path::new("."));
    if blobs_bin
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .eq(mask_dir)
    {
        let name = blobs_bin
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("invalid blobs file name: {}", blobs_bin.display()))?;
        return Ok(name.to_string());
    }
    Ok(blobs_bin.to_string_lossy().to_string())
}

fn resolve_blobs_path(mask_json: &Path, blobs_file: &str) -> anyhow::Result<PathBuf> {
    let p = PathBuf::from(blobs_file);
    if p.is_absolute() {
        return Ok(p);
    }
    let mask_dir = mask_json.parent().unwrap_or_else(|| Path::new("."));
    Ok(mask_dir.join(p))
}

fn decode_entry_data(data: &MaskEntryData, blobs: Option<&[u8]>) -> anyhow::Result<Vec<u8>> {
    match data {
        MaskEntryData::Empty => Ok(Vec::new()),
        MaskEntryData::Utf8(s) => Ok(s.as_bytes().to_vec()),
        MaskEntryData::Base64(s) => Ok(B64
            .decode(s.as_bytes())
            .context("decode base64 entry data")?),
        MaskEntryData::External(r) => {
            let blobs = blobs.ok_or_else(|| anyhow!("mask entry requires blobs_file (external)"))?;
            let start = r.offset as usize;
            let end = start.saturating_add(r.length as usize);
            if start > blobs.len() || end > blobs.len() {
                return Err(anyhow!(
                    "mask external ref out of range: offset={} length={} blobs_len={}",
                    r.offset,
                    r.length,
                    blobs.len()
                ));
            }
            let slice = &blobs[start..end];
            let mut hasher = Sha256::new();
            hasher.update(slice);
            let got = hex::encode(hasher.finalize());
            if got != r.sha256 {
                return Err(anyhow!(
                    "mask external sha256 mismatch: offset={} length={} expected={} got={}",
                    r.offset,
                    r.length,
                    r.sha256,
                    got
                ));
            }
            Ok(slice.to_vec())
        }
    }
}

pub fn extract_mask_json_and_offsets(
    input_docx: &Path,
    mask_json: &Path,
    offsets_json: &Path,
    blobs_bin: &Path,
) -> anyhow::Result<()> {
    let pkg = DocxPackage::read(input_docx)?;
    let prefix = hash_file_prefix(input_docx)?;
    let mut blobs = File::create(blobs_bin)
        .with_context(|| format!("create mask blobs: {}", blobs_bin.display()))?;
    let mut blob_offset: u64 = 0;

    let mut entries_out: Vec<MaskEntryJson> = Vec::with_capacity(pkg.entries.len());
    let mut slots: Vec<TextSlot> = Vec::new();
    let mut next_id = 1usize;

    for ent in &pkg.entries {
        let (datepart, timepart): (u16, u16) = ent.last_modified.into();
        let mut out_ent = MaskEntryJson {
            name: ent.name.clone(),
            compression: compression_to_code(ent.compression),
            last_modified: (datepart, timepart),
            unix_mode: ent.unix_mode,
            is_dir: ent.is_dir,
            data: MaskEntryData::Empty,
        };

        if ent.is_dir || ent.name.ends_with('/') {
            entries_out.push(out_ent);
            continue;
        }

        let out_bytes: Vec<u8> = if ent.name.to_lowercase().ends_with(".xml") && !ent.data.is_empty() {
            let mut part = parse_xml_part(&ent.name, &ent.data)
                .with_context(|| format!("parse xml: {}", ent.name))?;
            for (idx, ev) in part.events.iter_mut().enumerate() {
                match ev {
                    XmlEvent::Text { text } => {
                        let ph = placeholder(&prefix, next_id);
                        let _orig = std::mem::replace(text, ph);
                        slots.push(TextSlot {
                            id: next_id,
                            part_name: part.name.clone(),
                            kind: SlotKind::Text,
                            event_index: idx,
                            attr_name: None,
                        });
                        next_id += 1;
                    }
                    XmlEvent::CData { text } => {
                        let ph = placeholder(&prefix, next_id);
                        let _orig = std::mem::replace(text, ph);
                        slots.push(TextSlot {
                            id: next_id,
                            part_name: part.name.clone(),
                            kind: SlotKind::CData,
                            event_index: idx,
                            attr_name: None,
                        });
                        next_id += 1;
                    }
                    XmlEvent::Start { name, attrs } | XmlEvent::Empty { name, attrs } => {
                        if name == "w:lvlText" {
                            if let Some(v) = find_attr_mut(attrs, "w:val") {
                                let ph = placeholder(&prefix, next_id);
                                let _orig = std::mem::replace(v, ph);
                                slots.push(TextSlot {
                                    id: next_id,
                                    part_name: part.name.clone(),
                                    kind: SlotKind::Attr,
                                    event_index: idx,
                                    attr_name: Some("w:val".to_string()),
                                });
                                next_id += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }

            verify_part_mask_pure(&part, &prefix)?;

            write_xml_part(&part).with_context(|| format!("serialize masked xml: {}", ent.name))?
        } else {
            ent.data.clone()
        };

        if out_bytes.is_empty() {
            out_ent.data = MaskEntryData::Empty;
            entries_out.push(out_ent);
            continue;
        }
        let mut hasher = Sha256::new();
        hasher.update(&out_bytes);
        let sha256 = hex::encode(hasher.finalize());
        let len = out_bytes.len() as u64;
        blobs
            .write_all(&out_bytes)
            .with_context(|| format!("write mask blobs: {}", blobs_bin.display()))?;
        out_ent.data = MaskEntryData::External(MaskBlobRef {
            offset: blob_offset,
            length: len,
            sha256,
        });
        blob_offset = blob_offset.saturating_add(len);
        entries_out.push(out_ent);
    }

    let mask = MaskJson {
        version: 2,
        placeholder_prefix: prefix.clone(),
        blobs_file: Some(blob_path_for_json(mask_json, blobs_bin)?),
        entries: entries_out,
    };
    fs::write(
        mask_json,
        serde_json::to_vec_pretty(&mask).context("serialize mask json")?,
    )
    .with_context(|| format!("write mask json: {}", mask_json.display()))?;

    let offsets = OffsetsJson {
        version: 1,
        placeholder_prefix: prefix,
        slots,
    };
    fs::write(
        offsets_json,
        serde_json::to_vec_pretty(&offsets).context("serialize offsets json")?,
    )
    .with_context(|| format!("write offsets json: {}", offsets_json.display()))?;

    Ok(())
}

pub fn extract_slot_texts(input_docx: &Path) -> anyhow::Result<(String, Vec<String>)> {
    let pkg = DocxPackage::read(input_docx)?;
    let prefix = hash_file_prefix(input_docx)?;

    let mut out: Vec<String> = Vec::new();
    for ent in &pkg.entries {
        if ent.is_dir || ent.name.ends_with('/') || ent.data.is_empty() {
            continue;
        }
        if !ent.name.to_lowercase().ends_with(".xml") {
            continue;
        }
        let part =
            parse_xml_part(&ent.name, &ent.data).with_context(|| format!("parse xml: {}", ent.name))?;
        for ev in &part.events {
            match ev {
                XmlEvent::Text { text } | XmlEvent::CData { text } => out.push(text.clone()),
                XmlEvent::Start { name, attrs } | XmlEvent::Empty { name, attrs } => {
                    if name == "w:lvlText" {
                        if let Some(v) = attrs.iter().find(|(k, _)| k == "w:val").map(|(_, v)| v) {
                            out.push(v.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    Ok((prefix, out))
}

pub fn merge_mask_json_and_offsets(
    mask_json: &Path,
    offsets_json: &Path,
    text_json: &Path,
    output_docx: &Path,
) -> anyhow::Result<()> {
    let mask: MaskJson = serde_json::from_slice(
        &fs::read(mask_json).with_context(|| format!("read mask json: {}", mask_json.display()))?,
    )
    .context("parse mask json")?;
    let offsets: OffsetsJson = serde_json::from_slice(
        &fs::read(offsets_json)
            .with_context(|| format!("read offsets json: {}", offsets_json.display()))?,
    )
    .context("parse offsets json")?;
    let text: PureTextJson = serde_json::from_slice(
        &fs::read(text_json).with_context(|| format!("read text json: {}", text_json.display()))?,
    )
    .context("parse text json")?;

    if mask.placeholder_prefix != offsets.placeholder_prefix {
        return Err(anyhow!(
            "placeholder_prefix mismatch: mask={} offsets={}",
            mask.placeholder_prefix,
            offsets.placeholder_prefix
        ));
    }
    if text.placeholder_prefix != offsets.placeholder_prefix {
        return Err(anyhow!(
            "placeholder_prefix mismatch: text={} offsets={}",
            text.placeholder_prefix,
            offsets.placeholder_prefix
        ));
    }
    let max_id = offsets.slots.iter().map(|s| s.id).max().unwrap_or(0);
    let min_id = offsets.slots.iter().map(|s| s.id).min().unwrap_or(0);
    if !offsets.slots.is_empty() {
        if min_id != 1 {
            return Err(anyhow!("offsets slot ids must start at 1 (min_id={min_id})"));
        }
        if max_id != offsets.slots.len() {
            return Err(anyhow!(
                "offsets slot ids must be contiguous (max_id={max_id} slots_len={})",
                offsets.slots.len()
            ));
        }
    }
    if text.slot_texts.len() != max_id {
        return Err(anyhow!(
            "text slot_texts length mismatch: text_len={} expected={}",
            text.slot_texts.len(),
            max_id
        ));
    }

    let blobs_path = if let Some(p) = mask.blobs_file.as_deref() {
        Some(resolve_blobs_path(mask_json, p)?)
    } else {
        None
    };
    let blobs: Option<Vec<u8>> = if let Some(p) = blobs_path.as_ref() {
        Some(fs::read(p).with_context(|| format!("read mask blobs: {}", p.display()))?)
    } else {
        None
    };

    let mut entries: Vec<DocxEntry> = Vec::with_capacity(mask.entries.len());
    for ent in &mask.entries {
        let data = decode_entry_data(&ent.data, blobs.as_deref())
            .with_context(|| format!("decode entry: {}", ent.name))?;
        let last_modified = DateTime::try_from(ent.last_modified).unwrap_or_default();
        entries.push(DocxEntry {
            name: ent.name.clone(),
            data,
            compression: compression_from_code(ent.compression),
            last_modified,
            unix_mode: ent.unix_mode,
            is_dir: ent.is_dir,
        });
    }

    let mut parts: HashMap<String, XmlPart> = HashMap::new();
    let mut part_to_entry_idx: HashMap<String, usize> = HashMap::new();
    for (i, ent) in entries.iter().enumerate() {
        if !ent.name.to_lowercase().ends_with(".xml") || ent.data.is_empty() {
            continue;
        }
        let part = parse_xml_part(&ent.name, &ent.data)
            .with_context(|| format!("parse masked xml: {}", ent.name))?;
        part_to_entry_idx.insert(ent.name.clone(), i);
        parts.insert(ent.name.clone(), part);
    }

    for slot in &offsets.slots {
        let ph = placeholder(&offsets.placeholder_prefix, slot.id);
        let replacement = text
            .slot_texts
            .get(slot.id.saturating_sub(1))
            .ok_or_else(|| anyhow!("missing slot_texts[{}] for id={}", slot.id.saturating_sub(1), slot.id))?
            .clone();
        let part = parts
            .get_mut(&slot.part_name)
            .with_context(|| format!("missing part: {}", slot.part_name))?;
        let ev = part
            .events
            .get_mut(slot.event_index)
            .with_context(|| format!("event index out of range: {}@{}", slot.part_name, slot.event_index))?;
        match slot.kind {
            SlotKind::Text => match ev {
                XmlEvent::Text { text } => {
                    if *text != ph {
                        return Err(anyhow!(
                            "mask mismatch: expected placeholder {ph} at {}#{} (got={:?})",
                            slot.part_name,
                            slot.event_index,
                            text
                        ));
                    }
                    *text = replacement;
                }
                _ => return Err(anyhow!("expected Text event at {}#{}", slot.part_name, slot.event_index)),
            },
            SlotKind::CData => match ev {
                XmlEvent::CData { text } => {
                    if *text != ph {
                        return Err(anyhow!(
                            "mask mismatch: expected placeholder {ph} at {}#{} (got={:?})",
                            slot.part_name,
                            slot.event_index,
                            text
                        ));
                    }
                    *text = replacement;
                }
                _ => return Err(anyhow!("expected CData event at {}#{}", slot.part_name, slot.event_index)),
            },
            SlotKind::Attr => {
                let attr_name = slot
                    .attr_name
                    .as_deref()
                    .ok_or_else(|| anyhow!("attr slot missing attr_name"))?;
                match ev {
                    XmlEvent::Start { attrs, .. } | XmlEvent::Empty { attrs, .. } => {
                        let v = find_attr_mut(attrs, attr_name).ok_or_else(|| {
                            anyhow!("missing attr {attr_name} at {}#{}", slot.part_name, slot.event_index)
                        })?;
                        if *v != ph {
                            return Err(anyhow!(
                                "mask mismatch: expected placeholder {ph} for attr {attr_name} at {}#{} (got={:?})",
                                slot.part_name,
                                slot.event_index,
                                v
                            ));
                        }
                        *v = replacement;
                    }
                    _ => return Err(anyhow!("expected Start/Empty event at {}#{}", slot.part_name, slot.event_index)),
                }
            }
        }
    }

    // Strict: no leftover placeholders in any XML part.
    for (name, part) in parts.iter() {
        for ev in &part.events {
            match ev {
                XmlEvent::Text { text } | XmlEvent::CData { text } => {
                    if text.contains(&offsets.placeholder_prefix) {
                        return Err(anyhow!("leftover placeholder in {name}: {:?}", text));
                    }
                }
                XmlEvent::Start { attrs, .. } | XmlEvent::Empty { attrs, .. } => {
                    for (_, v) in attrs {
                        if v.contains(&offsets.placeholder_prefix) {
                            return Err(anyhow!("leftover placeholder in {name} attr: {:?}", v));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    for (part_name, part) in parts.iter() {
        let bytes =
            write_xml_part(part).with_context(|| format!("serialize restored xml: {part_name}"))?;
        let entry_idx = *part_to_entry_idx
            .get(part_name)
            .with_context(|| format!("missing entry index for part: {part_name}"))?;
        entries[entry_idx].data = bytes;
    }

    let pkg = DocxPackage { entries };
    pkg.write_with_replacements(output_docx, &HashMap::new())?;
    Ok(())
}

pub fn verify_docx_roundtrip(original_docx: &Path, restored_docx: &Path) -> anyhow::Result<()> {
    let orig = DocxPackage::read(original_docx)?;
    let restored = DocxPackage::read(restored_docx)?;

    if orig.entries.len() != restored.entries.len() {
        return Err(anyhow!(
            "zip entry count mismatch: orig={} restored={}",
            orig.entries.len(),
            restored.entries.len()
        ));
    }

    for (a, b) in orig.entries.iter().zip(restored.entries.iter()) {
        if a.name != b.name {
            return Err(anyhow!("zip entry order/name mismatch: {} vs {}", a.name, b.name));
        }
        if a.compression != b.compression {
            return Err(anyhow!("zip entry compression differs: {}", a.name));
        }
        if a.last_modified != b.last_modified {
            return Err(anyhow!("zip entry timestamp differs: {}", a.name));
        }
        if a.is_dir != b.is_dir {
            return Err(anyhow!("zip entry is_dir differs: {}", a.name));
        }
        let is_xml = a.name.to_lowercase().ends_with(".xml");
        if !is_xml {
            if a.data != b.data {
                return Err(anyhow!("non-xml entry bytes differ: {}", a.name));
            }
            continue;
        }
        if a.data.is_empty() && b.data.is_empty() {
            continue;
        }
        let pa =
            parse_xml_part(&a.name, &a.data).with_context(|| format!("parse orig xml: {}", a.name))?;
        let pb = parse_xml_part(&b.name, &b.data)
            .with_context(|| format!("parse restored xml: {}", b.name))?;
        let ha = full_hash(&pa.events);
        let hb = full_hash(&pb.events);
        if ha != hb {
            return Err(anyhow!("xml entry differs (full hash): {}", a.name));
        }
    }
    Ok(())
}

pub fn default_outputs_for(input_docx: &Path) -> MaskOutputs {
    let stem = input_docx
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("docx");
    let dir = input_docx.parent().unwrap_or_else(|| Path::new("."));
    MaskOutputs {
        mask_json_path: dir.join(format!("{stem}.mask.json")),
        offsets_json_path: dir.join(format!("{stem}.offsets.json")),
        blobs_bin_path: dir.join(format!("{stem}.mask.blobs.bin")),
    }
}
