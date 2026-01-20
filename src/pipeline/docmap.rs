use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context};

use crate::docx::decompose::{OffsetsJson, SlotKind};
use crate::docx::package::DocxPackage;
use crate::docx::pure_text::PureTextJson;
use crate::docx::xml::{parse_xml_part, XmlEvent};
use crate::sentinels::slot_token;

#[derive(Clone, Debug)]
pub struct ParaSlotUnit {
    pub tu_id: usize,
    pub part_name: String,
    pub scope_key: String,
    pub para_style: Option<String>,
    pub slot_ids: Vec<usize>,
    pub source_surface: String,
}

pub fn build_para_slot_units(
    docx_path: &Path,
    text: &PureTextJson,
    offsets: &OffsetsJson,
) -> anyhow::Result<Vec<ParaSlotUnit>> {
    let mut units: Vec<ParaSlotUnit> = Vec::with_capacity(text.paragraphs.len());
    let mut para_index: HashMap<(String, usize), usize> = HashMap::new();
    for (i, p) in text.paragraphs.iter().enumerate() {
        para_index.insert((p.part_name.clone(), p.xml_event_index), i);
        units.push(ParaSlotUnit {
            tu_id: p.para_id,
            part_name: p.part_name.clone(),
            scope_key: p.scope_key.clone(),
            para_style: p.p_style.clone(),
            slot_ids: Vec::new(),
            source_surface: String::new(),
        });
    }

    let mut slot_by_part_event: HashMap<(String, usize, u8), usize> = HashMap::new();
    for s in &offsets.slots {
        slot_by_part_event.insert(
            (s.part_name.clone(), s.event_index, slot_kind_code(&s.kind)),
            s.id,
        );
    }

    let pkg = DocxPackage::read(docx_path)?;
    for ent in pkg.xml_entries() {
        if ent.data.is_empty() {
            continue;
        }
        let part = parse_xml_part(&ent.name, &ent.data)
            .with_context(|| format!("parse xml: {}", ent.name))?;

        let mut stack: Vec<String> = Vec::new();
        let mut cur_para_idx: Option<usize> = None;
        let mut nested_para_depth: usize = 0;

        for (idx, ev) in part.events.iter().enumerate() {
            match ev {
                XmlEvent::Start { name, .. } => {
                    if name == "w:p" {
                        if cur_para_idx.is_some() {
                            nested_para_depth = nested_para_depth.saturating_add(1);
                        } else {
                            cur_para_idx = para_index.get(&(part.name.clone(), idx)).copied();
                            nested_para_depth = 0;
                        }
                    }
                    stack.push(name.clone());
                }
                XmlEvent::End { name } => {
                    if name == "w:p" {
                        if nested_para_depth > 0 {
                            nested_para_depth = nested_para_depth.saturating_sub(1);
                        } else {
                            cur_para_idx = None;
                        }
                    }
                    let _ = stack.pop();
                }
                XmlEvent::Empty { name, .. } => {
                    let _ = name;
                }
                XmlEvent::Text { .. } | XmlEvent::CData { .. } => {
                    if nested_para_depth > 0 {
                        continue;
                    }
                    let Some(pi) = cur_para_idx else {
                        continue;
                    };
                    let parent = stack.last().map(|s| s.as_str()).unwrap_or("");
                    if parent != "w:t" {
                        continue;
                    }

                    let kind = match ev {
                        XmlEvent::Text { .. } => slot_kind_code(&SlotKind::Text),
                        XmlEvent::CData { .. } => slot_kind_code(&SlotKind::CData),
                        _ => continue,
                    };
                    let Some(&slot_id) = slot_by_part_event.get(&(part.name.clone(), idx, kind))
                    else {
                        continue;
                    };
                    let slot_text = text
                        .slot_texts
                        .get(slot_id.saturating_sub(1))
                        .ok_or_else(|| anyhow!("missing slot_texts for slot_id={slot_id}"))?;
                    units[pi].slot_ids.push(slot_id);
                    units[pi].source_surface.push_str(&slot_token(slot_id));
                    units[pi].source_surface.push_str(slot_text);
                }
                _ => {}
            }
        }
    }

    for u in &mut units {
        if !u.slot_ids.is_empty() {
            u.source_surface.push_str(&slot_token(0));
        }
    }

    Ok(units)
}

fn slot_kind_code(k: &SlotKind) -> u8 {
    match k {
        SlotKind::Text => 0,
        SlotKind::CData => 1,
        SlotKind::Attr => 2,
    }
}
