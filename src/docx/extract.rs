use std::collections::HashMap;

use crate::ir::{Atom, AtomKind, FormatSpan, TextNodeKind, TextNodeRef, TranslationUnit};
use crate::sentinels::{BR, NBH, SHY, TAB};

use super::xml::{XmlEvent, XmlPart};

#[derive(Default, Clone)]
struct WRunStyle {
    b: Option<bool>,
    i: Option<bool>,
    u: Option<String>,
    strike: Option<bool>,
    color: Option<String>,
    highlight: Option<String>,
    sz: Option<String>,
    sz_cs: Option<String>,
    r_style: Option<String>,
    fonts_ascii: Option<String>,
    fonts_hansi: Option<String>,
    fonts_eastasia: Option<String>,
    fonts_cs: Option<String>,
    seen_rpr: bool,
}

impl WRunStyle {
    fn signature(&self) -> String {
        if !self.seen_rpr {
            return "w:rPr()".to_string();
        }
        let b = bool_to_sig(self.b);
        let i = bool_to_sig(self.i);
        let strike = bool_to_sig(self.strike);
        let u = self.u.clone().unwrap_or_default();
        let color = self.color.clone().unwrap_or_default();
        let highlight = self.highlight.clone().unwrap_or_default();
        let sz = self.sz.clone().unwrap_or_default();
        let sz_cs = self.sz_cs.clone().unwrap_or_default();
        let r_style = self.r_style.clone().unwrap_or_default();
        let fonts = format!(
            "{}|{}|{}|{}",
            self.fonts_ascii.clone().unwrap_or_default(),
            self.fonts_hansi.clone().unwrap_or_default(),
            self.fonts_eastasia.clone().unwrap_or_default(),
            self.fonts_cs.clone().unwrap_or_default()
        );
        format!(
            "b={b}|i={i}|u={u}|strike={strike}|color={color}|highlight={highlight}|sz={sz}|szCs={sz_cs}|rStyle={r_style}|fonts={fonts}"
        )
    }
}

#[derive(Default, Clone)]
struct ARunStyle {
    b: Option<String>,
    i: Option<String>,
    u: Option<String>,
    strike: Option<String>,
    sz: Option<String>,
    typeface: Option<String>,
    seen_rpr: bool,
}

impl ARunStyle {
    fn signature(&self) -> String {
        if !self.seen_rpr {
            return "a:rPr()".to_string();
        }
        let b = self.b.clone().unwrap_or_default();
        let i = self.i.clone().unwrap_or_default();
        let u = self.u.clone().unwrap_or_default();
        let strike = self.strike.clone().unwrap_or_default();
        let sz = self.sz.clone().unwrap_or_default();
        let typeface = self.typeface.clone().unwrap_or_default();
        format!("b={b}|i={i}|u={u}|strike={strike}|sz={sz}|typeface={typeface}")
    }
}

fn bool_to_sig(v: Option<bool>) -> String {
    match v {
        Some(true) => "1".to_string(),
        Some(false) => "0".to_string(),
        None => "0".to_string(),
    }
}

pub fn extract_translation_units(
    part: &XmlPart,
    start_id: usize,
) -> anyhow::Result<(Vec<TranslationUnit>, usize)> {
    let mut tus: Vec<TranslationUnit> = Vec::new();
    let mut next_id = start_id;

    let mut in_w_p = false;
    let mut in_a_p = false;
    let mut p_start_index: usize = 0;
    let mut p_atoms: Vec<Atom> = Vec::new();
    let mut p_style: Option<String> = None;

    let mut w_run_stack: Vec<WRunStyle> = Vec::new();
    let mut a_run_stack: Vec<ARunStyle> = Vec::new();
    let mut in_w_rpr = false;
    let mut in_a_rpr = false;

    let mut current_text_elem: Option<(TextNodeKind, usize)> = None;

    for (idx, ev) in part.events.iter().enumerate() {
        match ev {
            XmlEvent::Start { name, attrs } => {
                let name_s = name.as_str();
                if name_s == "w:p" {
                    in_w_p = true;
                    in_a_p = false;
                    p_start_index = idx;
                    p_atoms.clear();
                    p_style = None;
                } else if name_s == "a:p" {
                    in_a_p = true;
                    in_w_p = false;
                    p_start_index = idx;
                    p_atoms.clear();
                    p_style = None;
                }

                if in_w_p && name_s == "w:pStyle" {
                    if let Some(val) = find_attr(attrs, "w:val") {
                        let val = val.trim();
                        if !val.is_empty() {
                            p_style = Some(val.to_string());
                        }
                    }
                }

                if in_w_p && name_s == "w:r" {
                    w_run_stack.push(WRunStyle::default());
                }
                if in_a_p && name_s == "a:r" {
                    a_run_stack.push(ARunStyle::default());
                }

                if in_w_p && name_s == "w:rPr" {
                    in_w_rpr = true;
                    if let Some(top) = w_run_stack.last_mut() {
                        top.seen_rpr = true;
                    }
                }
                if in_a_p && name_s == "a:rPr" {
                    in_a_rpr = true;
                    if let Some(top) = a_run_stack.last_mut() {
                        top.seen_rpr = true;
                        for (k, v) in attrs {
                            match k.as_str() {
                                "b" => top.b = Some(v.clone()),
                                "i" => top.i = Some(v.clone()),
                                "u" => top.u = Some(v.clone()),
                                "strike" => top.strike = Some(v.clone()),
                                "sz" => top.sz = Some(v.clone()),
                                _ => {}
                            }
                        }
                    }
                }

                if (in_w_p || in_a_p) && (name_s == "w:t" || name_s == "a:t") {
                    current_text_elem = Some((
                        if name_s == "w:t" {
                            TextNodeKind::Wt
                        } else {
                            TextNodeKind::At
                        },
                        idx,
                    ));
                }

                if in_w_rpr {
                    if let Some(top) = w_run_stack.last_mut() {
                        parse_w_rpr_property(top, name_s, attrs);
                    }
                }

                if in_a_rpr && name_s == "a:latin" {
                    if let Some(top) = a_run_stack.last_mut() {
                        if let Some(v) = find_attr(attrs, "typeface") {
                            top.typeface = Some(v.to_string());
                        }
                    }
                }

                if name_s == "w:lvlText" {
                    if let Some(val) = find_attr(attrs, "w:val") {
                        if !val.trim().is_empty() {
                            add_attr_tu(part, idx, val, &mut tus, &mut next_id);
                        }
                    }
                }
            }
            XmlEvent::Empty { name, attrs } => {
                let name_s = name.as_str();
                if in_w_p && name_s == "w:pStyle" {
                    if let Some(val) = find_attr(attrs, "w:val") {
                        let val = val.trim();
                        if !val.is_empty() {
                            p_style = Some(val.to_string());
                        }
                    }
                }
                if in_w_p {
                    match name_s {
                        "w:tab" => push_control(&mut p_atoms, AtomKind::Tab, TAB),
                        "w:br" | "w:cr" => push_control(&mut p_atoms, AtomKind::Br, BR),
                        "w:noBreakHyphen" => push_control(&mut p_atoms, AtomKind::Nbh, NBH),
                        "w:softHyphen" => push_control(&mut p_atoms, AtomKind::Shy, SHY),
                        _ => {}
                    }
                } else if in_a_p {
                    match name_s {
                        "a:tab" => push_control(&mut p_atoms, AtomKind::Tab, TAB),
                        "a:br" => push_control(&mut p_atoms, AtomKind::Br, BR),
                        _ => {}
                    }
                }

                if name_s == "w:lvlText" {
                    if let Some(val) = find_attr(attrs, "w:val") {
                        if !val.trim().is_empty() {
                            add_attr_tu(part, idx, val, &mut tus, &mut next_id);
                        }
                    }
                }
            }
            XmlEvent::End { name } => {
                let name_s = name.as_str();
                if in_w_p && name_s == "w:p" {
                    finalize_paragraph(
                        &mut tus,
                        &mut next_id,
                        part,
                        "w:p",
                        p_start_index,
                        &p_atoms,
                        p_style.clone(),
                    );
                    in_w_p = false;
                    p_atoms.clear();
                    p_style = None;
                } else if in_a_p && name_s == "a:p" {
                    finalize_paragraph(
                        &mut tus,
                        &mut next_id,
                        part,
                        "a:p",
                        p_start_index,
                        &p_atoms,
                        None,
                    );
                    in_a_p = false;
                    p_atoms.clear();
                    p_style = None;
                }

                if in_w_p && name_s == "w:rPr" {
                    in_w_rpr = false;
                }
                if in_a_p && name_s == "a:rPr" {
                    in_a_rpr = false;
                }
                if in_w_p && name_s == "w:r" {
                    let _ = w_run_stack.pop();
                }
                if in_a_p && name_s == "a:r" {
                    let _ = a_run_stack.pop();
                }
                if (in_w_p || in_a_p) && (name_s == "w:t" || name_s == "a:t") {
                    current_text_elem = None;
                }
            }
            XmlEvent::Text { text } => {
                if let Some((kind, elem_start_idx)) = current_text_elem.clone() {
                    if !(in_w_p || in_a_p) {
                        continue;
                    }
                    let style_sig = match kind {
                        TextNodeKind::Wt => {
                            w_run_stack.last().cloned().unwrap_or_default().signature()
                        }
                        TextNodeKind::At => {
                            a_run_stack.last().cloned().unwrap_or_default().signature()
                        }
                        TextNodeKind::Attr => "attr".to_string(),
                    };
                    let node_ref = TextNodeRef {
                        part_name: part.name.clone(),
                        kind,
                        elem_event_index: elem_start_idx,
                        text_event_index: Some(idx),
                        attr_name: None,
                        original_text: text.clone(),
                    };
                    p_atoms.push(Atom {
                        kind: AtomKind::Text,
                        node_ref: Some(node_ref),
                        value: text.clone(),
                        style_sig,
                    });
                }
            }
            _ => {}
        }
    }

    Ok((tus, next_id))
}

fn push_control(atoms: &mut Vec<Atom>, kind: AtomKind, value: &str) {
    atoms.push(Atom {
        kind,
        node_ref: None,
        value: value.to_string(),
        style_sig: String::new(),
    });
}

fn find_attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn parse_w_rpr_property(style: &mut WRunStyle, name: &str, attrs: &[(String, String)]) {
    match name {
        "w:b" => style.b = Some(parse_w_bool(attrs)),
        "w:i" => style.i = Some(parse_w_bool(attrs)),
        "w:strike" => style.strike = Some(parse_w_bool(attrs)),
        "w:u" => style.u = find_attr(attrs, "w:val").map(|v| v.to_string()),
        "w:color" => style.color = find_attr(attrs, "w:val").map(|v| v.to_string()),
        "w:highlight" => style.highlight = find_attr(attrs, "w:val").map(|v| v.to_string()),
        "w:sz" => style.sz = find_attr(attrs, "w:val").map(|v| v.to_string()),
        "w:szCs" => style.sz_cs = find_attr(attrs, "w:val").map(|v| v.to_string()),
        "w:rStyle" => style.r_style = find_attr(attrs, "w:val").map(|v| v.to_string()),
        "w:rFonts" => {
            style.fonts_ascii = find_attr(attrs, "w:ascii").map(|v| v.to_string());
            style.fonts_hansi = find_attr(attrs, "w:hAnsi").map(|v| v.to_string());
            style.fonts_eastasia = find_attr(attrs, "w:eastAsia").map(|v| v.to_string());
            style.fonts_cs = find_attr(attrs, "w:cs").map(|v| v.to_string());
        }
        _ => {}
    }
}

fn parse_w_bool(attrs: &[(String, String)]) -> bool {
    if let Some(v) = find_attr(attrs, "w:val") {
        let s = v.trim().to_ascii_lowercase();
        return !(s == "0" || s == "false" || s == "off" || s == "none");
    }
    true
}

fn build_spans(atoms: &[Atom]) -> Vec<FormatSpan> {
    let mut spans: Vec<FormatSpan> = Vec::new();
    let mut current_style: Option<String> = None;
    let mut current_nodes: Vec<TextNodeRef> = Vec::new();
    let mut current_text_parts: Vec<String> = Vec::new();

    let flush = |spans: &mut Vec<FormatSpan>,
                 current_style: &mut Option<String>,
                 current_nodes: &mut Vec<TextNodeRef>,
                 current_text_parts: &mut Vec<String>| {
        if !current_nodes.is_empty() {
            spans.push(FormatSpan {
                style_sig: current_style.clone().unwrap_or_default(),
                node_refs: current_nodes.clone(),
                source_text: current_text_parts.concat(),
            });
        }
        *current_style = None;
        current_nodes.clear();
        current_text_parts.clear();
    };

    for atom in atoms {
        if atom.kind != AtomKind::Text {
            flush(
                &mut spans,
                &mut current_style,
                &mut current_nodes,
                &mut current_text_parts,
            );
            continue;
        }
        if current_style.as_deref() != Some(atom.style_sig.as_str()) {
            flush(
                &mut spans,
                &mut current_style,
                &mut current_nodes,
                &mut current_text_parts,
            );
            current_style = Some(atom.style_sig.clone());
        }
        if let Some(nr) = atom.node_ref.clone() {
            current_nodes.push(nr);
            current_text_parts.push(atom.value.clone());
        }
    }
    flush(
        &mut spans,
        &mut current_style,
        &mut current_nodes,
        &mut current_text_parts,
    );
    spans
}

fn finalize_paragraph(
    tus: &mut Vec<TranslationUnit>,
    next_id: &mut usize,
    part: &XmlPart,
    tag: &str,
    start_idx: usize,
    atoms: &[Atom],
    para_style: Option<String>,
) {
    if !atoms
        .iter()
        .any(|a| a.kind == AtomKind::Text && !a.value.trim().is_empty())
    {
        return;
    }
    let spans = build_spans(atoms);
    let surface_text: String = atoms.iter().map(|a| a.value.as_str()).collect();
    tus.push(TranslationUnit {
        tu_id: *next_id,
        part_name: part.name.clone(),
        scope_key: format!("{}#{}@{}", part.name, tag, start_idx),
        para_style,
        atoms: atoms.to_vec(),
        spans,
        source_surface: surface_text,
        frozen_surface: String::new(),
        nt_map: HashMap::new(),
        nt_mask: Vec::new(),
        draft_translation: None,
        final_translation: None,
        alt_translation: None,
        draft_translation_model: None,
        alt_translation_model: None,
        qe_score: None,
        qe_flags: vec![],
    });
    *next_id += 1;
}

fn add_attr_tu(
    part: &XmlPart,
    idx: usize,
    val_s: &str,
    tus: &mut Vec<TranslationUnit>,
    next_id: &mut usize,
) {
    let node_ref = TextNodeRef {
        part_name: part.name.clone(),
        kind: TextNodeKind::Attr,
        elem_event_index: idx,
        text_event_index: None,
        attr_name: Some("w:val".to_string()),
        original_text: val_s.to_string(),
    };
    let atom = Atom {
        kind: AtomKind::Text,
        node_ref: Some(node_ref.clone()),
        value: val_s.to_string(),
        style_sig: "attr".to_string(),
    };
    let span = FormatSpan {
        style_sig: "attr".to_string(),
        node_refs: vec![node_ref],
        source_text: val_s.to_string(),
    };
    tus.push(TranslationUnit {
        tu_id: *next_id,
        part_name: part.name.clone(),
        scope_key: format!("{}#w:lvlText@{}", part.name, idx),
        para_style: None,
        atoms: vec![atom],
        spans: vec![span],
        source_surface: val_s.to_string(),
        frozen_surface: String::new(),
        nt_map: HashMap::new(),
        nt_mask: Vec::new(),
        draft_translation: None,
        final_translation: None,
        alt_translation: None,
        draft_translation_model: None,
        alt_translation_model: None,
        qe_score: None,
        qe_flags: vec![],
    });
    *next_id += 1;
}
