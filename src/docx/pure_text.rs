use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use crate::docx::decompose::extract_slot_texts;
use crate::docx::package::DocxPackage;
use crate::docx::xml::{parse_xml_part, XmlEvent, XmlPart};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParaContainer {
    DocumentBody,
    TableCell,
    Header,
    Footer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PureParagraph {
    pub para_id: usize,
    pub part_name: String,
    pub scope_key: String,
    #[serde(default)]
    pub xml_event_index: usize,
    pub container: ParaContainer,
    pub section_index: Option<usize>,
    pub table_index: Option<usize>,
    pub row_index: Option<usize>,
    pub cell_index: Option<usize>,
    pub p_style: Option<String>,
    pub num_id: Option<i32>,
    pub num_ilvl: Option<i32>,
    pub outline_lvl: Option<i32>,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PureTextJson {
    pub version: u32,
    pub placeholder_prefix: String,
    pub slot_texts: Vec<String>,
    pub paragraphs: Vec<PureParagraph>,
}

pub struct PureTextOutputs {
    pub text_json_path: PathBuf,
}

fn find_attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn parse_i32_attr(attrs: &[(String, String)], key: &str) -> Option<i32> {
    find_attr(attrs, key).and_then(|v| v.trim().parse::<i32>().ok())
}

fn control_append(buf: &mut String, name: &str, attrs: &[(String, String)]) {
    match name {
        "w:tab" | "w:ptab" => buf.push('\t'),
        "w:cr" => buf.push('\n'),
        "w:br" => {
            let br_type = find_attr(attrs, "w:type");
            if br_type.unwrap_or("textWrapping") == "textWrapping" {
                buf.push('\n');
            }
        }
        "w:noBreakHyphen" => buf.push('-'),
        // python-docx run.text doesn't include softHyphen; skip here for parity.
        "w:softHyphen" => {}
        _ => {}
    }
}

#[derive(Default, Clone)]
struct ParaCapture {
    start_event_index: usize,
    p_stack_len: usize,
    text: String,
    p_style: Option<String>,
    num_id: Option<i32>,
    num_ilvl: Option<i32>,
    outline_lvl: Option<i32>,
    direct_ppr_stack_len: Option<usize>,
    direct_r_stack_len: Option<usize>,
    hyperlink_stack_len: Option<usize>,
    hyperlink_r_stack_len: Option<usize>,
    w_t_stack_len: Option<usize>,
}

fn finalize_paragraph(
    out: &mut Vec<PureParagraph>,
    next_para_id: &mut usize,
    part_name: &str,
    cap: ParaCapture,
    container: ParaContainer,
    section_index: Option<usize>,
    table_index: Option<usize>,
    row_index: Option<usize>,
    cell_index: Option<usize>,
) {
    if cap.text.trim().is_empty() {
        return;
    }
    let para_id = *next_para_id;
    *next_para_id += 1;
    out.push(PureParagraph {
        para_id,
        part_name: part_name.to_string(),
        scope_key: format!("{part_name}#w:p@{}", cap.start_event_index),
        xml_event_index: cap.start_event_index,
        container,
        section_index,
        table_index,
        row_index,
        cell_index,
        p_style: cap.p_style,
        num_id: cap.num_id,
        num_ilvl: cap.num_ilvl,
        outline_lvl: cap.outline_lvl,
        text: cap.text,
    });
}

fn extract_body_and_tables_from_document(
    part: &XmlPart,
    out: &mut Vec<PureParagraph>,
    next_para_id: &mut usize,
) {
    let mut stack: Vec<String> = Vec::new();

    let mut tbl_depth = 0usize;
    let mut current_table_index = 0usize;
    let mut current_row_index = 0usize;
    let mut current_cell_index = 0usize;

    let mut capturing: Option<(ParaCapture, ParaContainer, Option<usize>, Option<usize>, Option<usize>)> = None;

    for (idx, ev) in part.events.iter().enumerate() {
        match ev {
            XmlEvent::Start { name, attrs } => {
                let parent = stack.last().map(|s| s.as_str()).unwrap_or("");

                if name == "w:tbl" {
                    if parent == "w:body" && tbl_depth == 0 {
                        current_table_index += 1;
                        current_row_index = 0;
                        current_cell_index = 0;
                    }
                    tbl_depth += 1;
                } else if name == "w:tr" {
                    if tbl_depth == 1 && parent == "w:tbl" {
                        current_row_index += 1;
                        current_cell_index = 0;
                    }
                } else if name == "w:tc" {
                    if tbl_depth == 1 && parent == "w:tr" {
                        current_cell_index += 1;
                    }
                }

                    if name == "w:p" {
                        if parent == "w:body" && tbl_depth == 0 {
                            capturing = Some((
                                ParaCapture {
                                    start_event_index: idx,
                                    p_stack_len: stack.len() + 1,
                                    ..Default::default()
                                },
                                ParaContainer::DocumentBody,
                                None,
                                None,
                            None,
                        ));
                    } else if parent == "w:tc" && tbl_depth == 1 {
                        capturing = Some((
                            ParaCapture {
                                start_event_index: idx,
                                p_stack_len: stack.len() + 1,
                                ..Default::default()
                            },
                            ParaContainer::TableCell,
                            Some(current_table_index),
                            Some(current_row_index),
                            Some(current_cell_index),
                        ));
                    }
                }

                if let Some((ref mut cap, ..)) = capturing {
                    match name.as_str() {
                        "w:pPr" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.direct_ppr_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:hyperlink" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.hyperlink_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:r" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.direct_r_stack_len = Some(stack.len() + 1);
                            } else if parent == "w:hyperlink"
                                && cap.hyperlink_stack_len == Some(stack.len())
                            {
                                cap.hyperlink_r_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:t" => {
                            if parent == "w:r"
                                && (cap.direct_r_stack_len == Some(stack.len())
                                    || cap.hyperlink_r_stack_len == Some(stack.len()))
                            {
                                cap.w_t_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:pStyle" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if let Some(v) = find_attr(attrs, "w:val") {
                                    let v = v.trim();
                                    if !v.is_empty() {
                                        cap.p_style = Some(v.to_string());
                                    }
                                }
                            }
                        }
                        "w:ilvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_ilvl.is_none() {
                                    cap.num_ilvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:numId" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_id.is_none() {
                                    cap.num_id = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:outlineLvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if cap.outline_lvl.is_none() {
                                    cap.outline_lvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:tab" | "w:ptab" | "w:cr" | "w:br" | "w:noBreakHyphen" | "w:softHyphen" => {
                            if parent == "w:r"
                                && (cap.direct_r_stack_len == Some(stack.len())
                                    || cap.hyperlink_r_stack_len == Some(stack.len()))
                            {
                                control_append(&mut cap.text, name, attrs);
                            }
                        }
                        _ => {}
                    }
                }

                stack.push(name.clone());
            }
            XmlEvent::Empty { name, attrs } => {
                let parent = stack.last().map(|s| s.as_str()).unwrap_or("");

                if name == "w:tbl" {
                    if parent == "w:body" && tbl_depth == 0 {
                        current_table_index += 1;
                        current_row_index = 0;
                        current_cell_index = 0;
                    }
                }

                if let Some((ref mut cap, ..)) = capturing {
                    match name.as_str() {
                        "w:pPr" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.direct_ppr_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:pStyle" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if let Some(v) = find_attr(attrs, "w:val") {
                                    let v = v.trim();
                                    if !v.is_empty() {
                                        cap.p_style = Some(v.to_string());
                                    }
                                }
                            }
                        }
                        "w:ilvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_ilvl.is_none() {
                                    cap.num_ilvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:numId" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_id.is_none() {
                                    cap.num_id = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:outlineLvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if cap.outline_lvl.is_none() {
                                    cap.outline_lvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:tab" | "w:ptab" | "w:cr" | "w:br" | "w:noBreakHyphen" | "w:softHyphen" => {
                            if parent == "w:r"
                                && (cap.direct_r_stack_len == Some(stack.len())
                                    || cap.hyperlink_r_stack_len == Some(stack.len()))
                            {
                                control_append(&mut cap.text, name, attrs);
                            }
                        }
                        _ => {}
                    }
                }
            }
            XmlEvent::Text { text } => {
                if let Some((ref mut cap, ..)) = capturing {
                    if cap.w_t_stack_len.is_some() {
                        cap.text.push_str(text);
                    }
                }
            }
            XmlEvent::End { name } => {
                if let Some((ref mut cap, ..)) = capturing {
                    if name == "w:t" {
                        if cap.w_t_stack_len == Some(stack.len()) {
                            cap.w_t_stack_len = None;
                        }
                    } else if name == "w:pPr" {
                        if cap.direct_ppr_stack_len == Some(stack.len()) {
                            cap.direct_ppr_stack_len = None;
                        }
                    } else if name == "w:r" {
                        if cap.direct_r_stack_len == Some(stack.len()) {
                            cap.direct_r_stack_len = None;
                        }
                        if cap.hyperlink_r_stack_len == Some(stack.len()) {
                            cap.hyperlink_r_stack_len = None;
                        }
                    } else if name == "w:hyperlink" {
                        if cap.hyperlink_stack_len == Some(stack.len()) {
                            cap.hyperlink_stack_len = None;
                            cap.hyperlink_r_stack_len = None;
                        }
                    }
                }

                if name == "w:p" {
                    if let Some((cap, container, table_index, row_index, cell_index)) =
                        capturing.take()
                    {
                        match container {
                            ParaContainer::DocumentBody => finalize_paragraph(
                                out,
                                next_para_id,
                                &part.name,
                                cap,
                                container,
                                None,
                                None,
                                None,
                                None,
                            ),
                            ParaContainer::TableCell => finalize_paragraph(
                                out,
                                next_para_id,
                                &part.name,
                                cap,
                                container,
                                None,
                                table_index,
                                row_index,
                                cell_index,
                            ),
                            _ => {}
                        }
                    }
                }

                if name == "w:tbl" && tbl_depth > 0 {
                    tbl_depth -= 1;
                }

                if let Some(top) = stack.pop() {
                    let _ = top;
                }
            }
            _ => {}
        }
    }
}

fn extract_direct_paragraphs_from_part(
    part: &XmlPart,
    root_tag: &str,
    container: ParaContainer,
    section_index: Option<usize>,
    out: &mut Vec<PureParagraph>,
    next_para_id: &mut usize,
) {
    let mut stack: Vec<String> = Vec::new();
    let mut capturing: Option<ParaCapture> = None;

    for (idx, ev) in part.events.iter().enumerate() {
        match ev {
            XmlEvent::Start { name, attrs } => {
                let parent = stack.last().map(|s| s.as_str()).unwrap_or("");
                if name == root_tag {
                    // ok
                }
                if name == "w:p" && parent == root_tag {
                    capturing = Some(ParaCapture {
                        start_event_index: idx,
                        p_stack_len: stack.len() + 1,
                        ..Default::default()
                    });
                }
                if let Some(ref mut cap) = capturing {
                    match name.as_str() {
                        "w:pPr" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.direct_ppr_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:hyperlink" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.hyperlink_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:r" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.direct_r_stack_len = Some(stack.len() + 1);
                            } else if parent == "w:hyperlink"
                                && cap.hyperlink_stack_len == Some(stack.len())
                            {
                                cap.hyperlink_r_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:t" => {
                            if parent == "w:r"
                                && (cap.direct_r_stack_len == Some(stack.len())
                                    || cap.hyperlink_r_stack_len == Some(stack.len()))
                            {
                                cap.w_t_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:pStyle" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if let Some(v) = find_attr(attrs, "w:val") {
                                    let v = v.trim();
                                    if !v.is_empty() {
                                        cap.p_style = Some(v.to_string());
                                    }
                                }
                            }
                        }
                        "w:ilvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_ilvl.is_none() {
                                    cap.num_ilvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:numId" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_id.is_none() {
                                    cap.num_id = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:outlineLvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if cap.outline_lvl.is_none() {
                                    cap.outline_lvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:tab" | "w:ptab" | "w:cr" | "w:br" | "w:noBreakHyphen" | "w:softHyphen" => {
                            if parent == "w:r"
                                && (cap.direct_r_stack_len == Some(stack.len())
                                    || cap.hyperlink_r_stack_len == Some(stack.len()))
                            {
                                control_append(&mut cap.text, name, attrs);
                            }
                        }
                        _ => {}
                    }
                }
                stack.push(name.clone());
            }
            XmlEvent::Empty { name, attrs } => {
                let parent = stack.last().map(|s| s.as_str()).unwrap_or("");
                if name == "w:p" && parent == root_tag {
                    finalize_paragraph(
                        out,
                        next_para_id,
                        &part.name,
                        ParaCapture {
                            start_event_index: idx,
                            ..Default::default()
                        },
                        container,
                        section_index,
                        None,
                        None,
                        None,
                    );
                    continue;
                }
                if let Some(ref mut cap) = capturing {
                    match name.as_str() {
                        "w:pPr" => {
                            if parent == "w:p" && stack.len() == cap.p_stack_len {
                                cap.direct_ppr_stack_len = Some(stack.len() + 1);
                            }
                        }
                        "w:pStyle" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if let Some(v) = find_attr(attrs, "w:val") {
                                    let v = v.trim();
                                    if !v.is_empty() {
                                        cap.p_style = Some(v.to_string());
                                    }
                                }
                            }
                        }
                        "w:ilvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_ilvl.is_none() {
                                    cap.num_ilvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:numId" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:numPr" {
                                if cap.num_id.is_none() {
                                    cap.num_id = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:outlineLvl" => {
                            if cap.direct_ppr_stack_len.is_some() && parent == "w:pPr" {
                                if cap.outline_lvl.is_none() {
                                    cap.outline_lvl = parse_i32_attr(attrs, "w:val");
                                }
                            }
                        }
                        "w:tab" | "w:ptab" | "w:cr" | "w:br" | "w:noBreakHyphen" | "w:softHyphen" => {
                            if parent == "w:r"
                                && (cap.direct_r_stack_len == Some(stack.len())
                                    || cap.hyperlink_r_stack_len == Some(stack.len()))
                            {
                                control_append(&mut cap.text, name, attrs);
                            }
                        }
                        _ => {}
                    }
                }
            }
            XmlEvent::Text { text } => {
                if let Some(ref mut cap) = capturing {
                    if cap.w_t_stack_len.is_some() {
                        cap.text.push_str(text);
                    }
                }
            }
            XmlEvent::End { name } => {
                if let Some(ref mut cap) = capturing {
                    if name == "w:t" {
                        if cap.w_t_stack_len == Some(stack.len()) {
                            cap.w_t_stack_len = None;
                        }
                    } else if name == "w:pPr" {
                        if cap.direct_ppr_stack_len == Some(stack.len()) {
                            cap.direct_ppr_stack_len = None;
                        }
                    } else if name == "w:r" {
                        if cap.direct_r_stack_len == Some(stack.len()) {
                            cap.direct_r_stack_len = None;
                        }
                        if cap.hyperlink_r_stack_len == Some(stack.len()) {
                            cap.hyperlink_r_stack_len = None;
                        }
                    } else if name == "w:hyperlink" {
                        if cap.hyperlink_stack_len == Some(stack.len()) {
                            cap.hyperlink_stack_len = None;
                            cap.hyperlink_r_stack_len = None;
                        }
                    }
                }
                if name == "w:p" {
                    if let Some(cap) = capturing.take() {
                        finalize_paragraph(
                            out,
                            next_para_id,
                            &part.name,
                            cap,
                            container,
                            section_index,
                            None,
                            None,
                            None,
                        );
                    }
                }
                let _ = stack.pop();
            }
            _ => {}
        }
    }
}

fn normalize_target(base: &str, target: &str) -> String {
    let mut t = target.replace('\\', "/");
    if t.starts_with('/') {
        t = t.trim_start_matches('/').to_string();
    }
    if t.starts_with("../") {
        // Strip leading ../ relative to `word/`.
        while t.starts_with("../") {
            t = t.trim_start_matches("../").to_string();
        }
    }
    format!("{base}{t}")
}

fn extract_doc_rels_map(doc_rels: &XmlPart) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    for ev in &doc_rels.events {
        if let XmlEvent::Empty { name, attrs } | XmlEvent::Start { name, attrs } = ev {
            if name != "Relationship" {
                continue;
            }
            let id = find_attr(attrs, "Id").unwrap_or("").trim().to_string();
            let target = find_attr(attrs, "Target").unwrap_or("").trim().to_string();
            if id.is_empty() || target.is_empty() {
                continue;
            }
            map.insert(id, normalize_target("word/", &target));
        }
    }
    map
}

#[derive(Default, Clone)]
struct SectionRefs {
    header_rid: Option<String>,
    footer_rid: Option<String>,
}

fn extract_sections_from_document_xml(doc: &XmlPart) -> Vec<SectionRefs> {
    let mut sections: Vec<SectionRefs> = Vec::new();
    let mut stack: Vec<String> = Vec::new();

    let mut in_sectpr = false;
    let mut cur_sect = SectionRefs::default();
    let mut pending_header: Option<String> = None;
    let mut pending_footer: Option<String> = None;

    for ev in &doc.events {
        match ev {
            XmlEvent::Start { name, attrs } => {
                let parent = stack.last().map(|s| s.as_str()).unwrap_or("");
                let sectpr_allowed = name == "w:sectPr"
                    && (parent == "w:body"
                        || (parent == "w:pPr"
                            && stack.len() >= 3
                            && stack[stack.len() - 2] == "w:p"
                            && stack[stack.len() - 3] == "w:body"));

                if sectpr_allowed {
                    in_sectpr = true;
                    pending_header = None;
                    pending_footer = None;
                } else if in_sectpr && (name == "w:headerReference" || name == "w:footerReference") {
                    let typ = find_attr(attrs, "w:type").unwrap_or("default");
                    if typ != "default" {
                        continue;
                    }
                    if let Some(rid) = find_attr(attrs, "r:id") {
                        let rid = rid.trim();
                        if !rid.is_empty() {
                            if name == "w:headerReference" {
                                pending_header = Some(rid.to_string());
                            } else {
                                pending_footer = Some(rid.to_string());
                            }
                        }
                    }
                }
                stack.push(name.clone());
            }
            XmlEvent::Empty { name, attrs } => {
                let parent = stack.last().map(|s| s.as_str()).unwrap_or("");
                let sectpr_allowed = name == "w:sectPr"
                    && (parent == "w:body"
                        || (parent == "w:pPr"
                            && stack.len() >= 3
                            && stack[stack.len() - 2] == "w:p"
                            && stack[stack.len() - 3] == "w:body"));

                if sectpr_allowed {
                    // Empty sectPr counts as a section boundary.
                    if let Some(h) = pending_header.take() {
                        cur_sect.header_rid = Some(h);
                    }
                    if let Some(f) = pending_footer.take() {
                        cur_sect.footer_rid = Some(f);
                    }
                    sections.push(cur_sect.clone());
                    in_sectpr = false;
                    continue;
                }

                if in_sectpr && (name == "w:headerReference" || name == "w:footerReference") {
                    let typ = find_attr(attrs, "w:type").unwrap_or("default");
                    if typ != "default" {
                        continue;
                    }
                    if let Some(rid) = find_attr(attrs, "r:id") {
                        let rid = rid.trim();
                        if !rid.is_empty() {
                            if name == "w:headerReference" {
                                pending_header = Some(rid.to_string());
                            } else {
                                pending_footer = Some(rid.to_string());
                            }
                        }
                    }
                }
            }
            XmlEvent::End { name } => {
                if name == "w:sectPr" {
                    if let Some(h) = pending_header.take() {
                        cur_sect.header_rid = Some(h);
                    }
                    if let Some(f) = pending_footer.take() {
                        cur_sect.footer_rid = Some(f);
                    }
                    sections.push(cur_sect.clone());
                    in_sectpr = false;
                }
                let _ = stack.pop();
            }
            _ => {}
        }
    }
    sections
}

pub fn extract_pure_text(input_docx: &Path) -> anyhow::Result<PureTextJson> {
    let pkg = DocxPackage::read(input_docx)?;
    let mut by_name: HashMap<String, Vec<u8>> = HashMap::new();
    for ent in &pkg.entries {
        by_name.insert(ent.name.clone(), ent.data.clone());
    }

    let doc_bytes = by_name
        .get("word/document.xml")
        .ok_or_else(|| anyhow!("missing word/document.xml"))?;
    let doc = parse_xml_part("word/document.xml", doc_bytes).context("parse word/document.xml")?;

    let mut doc_paras: Vec<PureParagraph> = Vec::new();
    let mut next_para_id = 1usize;
    extract_body_and_tables_from_document(&doc, &mut doc_paras, &mut next_para_id);

    let rels_map = if let Some(rels_bytes) = by_name.get("word/_rels/document.xml.rels") {
        let rels = parse_xml_part("word/_rels/document.xml.rels", rels_bytes)
            .context("parse word/_rels/document.xml.rels")?;
        extract_doc_rels_map(&rels)
    } else {
        HashMap::new()
    };

    let sections = extract_sections_from_document_xml(&doc);
    let mut header_footer_paras: Vec<PureParagraph> = Vec::new();

    for (i, s) in sections.iter().enumerate() {
        let section_index = i + 1;
        if let Some(rid) = s.header_rid.as_ref() {
            if let Some(part_name) = rels_map.get(rid) {
                if let Some(bytes) = by_name.get(part_name) {
                    if !bytes.is_empty() {
                        let part = parse_xml_part(part_name, bytes)
                            .with_context(|| format!("parse header part: {}", part_name))?;
                        extract_direct_paragraphs_from_part(
                            &part,
                            "w:hdr",
                            ParaContainer::Header,
                            Some(section_index),
                            &mut header_footer_paras,
                            &mut next_para_id,
                        );
                    }
                }
            }
        }
        if let Some(rid) = s.footer_rid.as_ref() {
            if let Some(part_name) = rels_map.get(rid) {
                if let Some(bytes) = by_name.get(part_name) {
                    if !bytes.is_empty() {
                        let part = parse_xml_part(part_name, bytes)
                            .with_context(|| format!("parse footer part: {}", part_name))?;
                        extract_direct_paragraphs_from_part(
                            &part,
                            "w:ftr",
                            ParaContainer::Footer,
                            Some(section_index),
                            &mut header_footer_paras,
                            &mut next_para_id,
                        );
                    }
                }
            }
        }
    }

    let mut paragraphs: Vec<PureParagraph> = Vec::new();
    paragraphs.extend(doc_paras);
    paragraphs.extend(header_footer_paras);

    let (placeholder_prefix, slot_texts) = extract_slot_texts(input_docx)?;

    Ok(PureTextJson {
        version: 3,
        placeholder_prefix,
        slot_texts,
        paragraphs,
    })
}

pub fn extract_pure_text_json(input_docx: &Path, output_json: &Path) -> anyhow::Result<()> {
    let out = extract_pure_text(input_docx)?;
    fs::write(
        output_json,
        serde_json::to_vec_pretty(&out).context("serialize pure text json")?,
    )
    .with_context(|| format!("write pure text json: {}", output_json.display()))?;
    Ok(())
}

pub fn default_text_output_for(input_docx: &Path) -> PureTextOutputs {
    let stem = input_docx
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("docx");
    let dir = input_docx.parent().unwrap_or_else(|| Path::new("."));
    PureTextOutputs {
        text_json_path: dir.join(format!("{stem}.text.json")),
    }
}
