use std::collections::HashMap;

use anyhow::{anyhow, Context};

use crate::docx::project::{distribute_span_text_to_nodes, project_translation_to_spans};
use crate::docx::xml::{XmlEvent, XmlPart};
use crate::ir::{TextNodeKind, TextNodeRef, TranslationUnit};

pub fn apply_translation_unit(
    parts: &mut HashMap<String, XmlPart>,
    tu: &TranslationUnit,
    translated_frozen: &str,
) -> anyhow::Result<()> {
    let span_slices =
        project_translation_to_spans(&tu.spans, &tu.frozen_surface, translated_frozen, &tu.nt_map)
            .context("project translation to spans")?;
    for span_slice in span_slices {
        for (node_ref, node_text) in distribute_span_text_to_nodes(&span_slice.span, &span_slice.text)
        {
            let part = parts
                .get_mut(&node_ref.part_name)
                .with_context(|| format!("missing part: {}", node_ref.part_name))?;
            apply_node_text(part, &node_ref, &node_text)?;
        }
    }
    Ok(())
}

pub fn apply_node_text(part: &mut XmlPart, node_ref: &TextNodeRef, node_text: &str) -> anyhow::Result<()> {
    match node_ref.kind {
        TextNodeKind::Attr => {
            let attr_name = node_ref
                .attr_name
                .as_ref()
                .context("attr node missing attr_name")?;
            let ev = part
                .events
                .get_mut(node_ref.elem_event_index)
                .context("attr elem index out of range")?;
            set_attr_value(ev, attr_name.as_str(), node_text);
            Ok(())
        }
        TextNodeKind::Wt | TextNodeKind::At => {
            let text_idx = node_ref
                .text_event_index
                .context("text node missing text index")?;
            if let Some(XmlEvent::Text { text }) = part.events.get_mut(text_idx) {
                *text = node_text.to_string();
            } else {
                return Err(anyhow!("expected Text event at {}", text_idx));
            }
            if node_text.starts_with(' ') || node_text.ends_with(' ') {
                let ev = part
                    .events
                    .get_mut(node_ref.elem_event_index)
                    .context("elem index out of range")?;
                set_attr_value(ev, "xml:space", "preserve");
            }
            Ok(())
        }
    }
}

fn set_attr_value(ev: &mut XmlEvent, key: &str, value: &str) {
    match ev {
        XmlEvent::Start { attrs, .. } | XmlEvent::Empty { attrs, .. } => {
            for (k, v) in attrs.iter_mut() {
                if k == key {
                    *v = value.to_string();
                    return;
                }
            }
            attrs.push((key.to_string(), value.to_string()));
        }
        _ => {}
    }
}
