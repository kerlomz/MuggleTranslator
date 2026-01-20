use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Context};
use serde::Deserialize;

use crate::docx::package::DocxPackage;
use crate::docx::xml::{parse_xml_part, write_xml_part, XmlEvent, XmlPart};

#[derive(Clone, Debug, Deserialize)]
pub struct DocxFilterRules {
    pub version: u32,

    #[serde(default)]
    pub strip_attributes: Vec<String>,

    #[serde(default)]
    pub drop_elements: Vec<String>,

    #[serde(default)]
    pub drop_run_properties: Vec<String>,

    #[serde(default)]
    pub preserve_whitespace_text_in: Vec<String>,

    #[serde(default)]
    pub merge_adjacent_runs: bool,

    #[serde(default)]
    pub merge_run_parts: Vec<String>,
}

impl DocxFilterRules {
    pub fn from_toml_path(path: &Path) -> anyhow::Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("read filter rules: {}", path.display()))?;
        let s = String::from_utf8(bytes).context("filter rules must be utf-8")?;
        let rules: DocxFilterRules = toml::from_str(&s).context("parse filter rules (toml)")?;
        if rules.version != 1 {
            return Err(anyhow!(
                "unsupported filter rules version: {} (expected 1)",
                rules.version
            ));
        }
        Ok(rules)
    }
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }
    let mut rest = text;
    let mut first = true;
    for seg in pattern.split('*') {
        if seg.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            if !rest.starts_with(seg) {
                return false;
            }
            rest = &rest[seg.len()..];
            first = false;
            continue;
        }
        if let Some(pos) = rest.find(seg) {
            rest = &rest[pos + seg.len()..];
            first = false;
            continue;
        }
        return false;
    }
    if !pattern.ends_with('*') {
        let last_seg = pattern.split('*').filter(|s| !s.is_empty()).last().unwrap_or("");
        return text.ends_with(last_seg);
    }
    true
}

fn should_merge_runs_for_part(rules: &DocxFilterRules, part_name: &str) -> bool {
    if !rules.merge_adjacent_runs {
        return false;
    }
    if rules.merge_run_parts.is_empty() {
        return true;
    }
    rules
        .merge_run_parts
        .iter()
        .any(|p| wildcard_match(p, part_name))
}

pub fn filter_docx_with_rules(input_docx: &Path, output_docx: &Path, rules: &DocxFilterRules) -> anyhow::Result<()> {
    let pkg = DocxPackage::read(input_docx)?;
    let strip_attrs: HashSet<&str> = rules.strip_attributes.iter().map(|s| s.as_str()).collect();
    let drop_elements: HashSet<&str> = rules.drop_elements.iter().map(|s| s.as_str()).collect();
    let drop_rpr: HashSet<&str> = rules.drop_run_properties.iter().map(|s| s.as_str()).collect();
    let preserve_ws_in: HashSet<&str> = rules
        .preserve_whitespace_text_in
        .iter()
        .map(|s| s.as_str())
        .collect();

    let mut replacements: HashMap<String, Vec<u8>> = HashMap::new();
    for ent in pkg.xml_entries() {
        if ent.data.is_empty() {
            continue;
        }
        let mut part = parse_xml_part(&ent.name, &ent.data)
            .with_context(|| format!("parse xml: {}", ent.name))?;
        filter_xml_part(&mut part, &strip_attrs, &drop_elements, &drop_rpr, &preserve_ws_in)?;
        if should_merge_runs_for_part(rules, &part.name) {
            part.events = merge_adjacent_text_runs_in_paragraphs(&part.events);
        }
        let bytes = write_xml_part(&part).with_context(|| format!("serialize xml: {}", ent.name))?;
        replacements.insert(ent.name.clone(), bytes);
    }
    pkg.write_with_replacements(output_docx, &replacements)?;
    Ok(())
}

fn filter_xml_part(
    part: &mut XmlPart,
    strip_attrs: &HashSet<&str>,
    drop_elements: &HashSet<&str>,
    drop_rpr: &HashSet<&str>,
    preserve_ws_in: &HashSet<&str>,
) -> anyhow::Result<()> {
    let mut out: Vec<XmlEvent> = Vec::with_capacity(part.events.len());
    let mut stack: Vec<String> = Vec::new();

    let mut skip_level: usize = 0;
    let mut skip_element: Option<String> = None;

    let mut i = 0usize;
    while i < part.events.len() {
        let ev = part.events[i].clone();
        if skip_level > 0 {
            match &ev {
                XmlEvent::Start { name, .. } => {
                    stack.push(name.clone());
                    skip_level = skip_level.saturating_add(1);
                }
                XmlEvent::End { name } => {
                    stack.pop();
                    skip_level = skip_level.saturating_sub(1);
                    if skip_level == 0 {
                        skip_element = None;
                    }
                    let _ = name;
                }
                _ => {}
            }
            i += 1;
            continue;
        }

        match ev {
            XmlEvent::Start { name, mut attrs } => {
                let in_rpr = stack.last().map(|s| s.as_str()) == Some("w:rPr");
                if in_rpr && drop_rpr.contains(name.as_str()) {
                    stack.push(name.clone());
                    skip_level = 1;
                    skip_element = Some(name);
                    i += 1;
                    continue;
                }
                if drop_elements.contains(name.as_str()) {
                    stack.push(name.clone());
                    skip_level = 1;
                    skip_element = Some(name);
                    i += 1;
                    continue;
                }
                attrs.retain(|(k, _)| !strip_attrs.contains(k.as_str()));
                out.push(XmlEvent::Start {
                    name: name.clone(),
                    attrs,
                });
                stack.push(name);
            }
            XmlEvent::Empty { name, mut attrs } => {
                let in_rpr = stack.last().map(|s| s.as_str()) == Some("w:rPr");
                if (in_rpr && drop_rpr.contains(name.as_str())) || drop_elements.contains(name.as_str()) {
                    i += 1;
                    continue;
                }
                attrs.retain(|(k, _)| !strip_attrs.contains(k.as_str()));
                out.push(XmlEvent::Empty { name, attrs });
            }
            XmlEvent::End { name } => {
                out.push(XmlEvent::End { name: name.clone() });
                stack.pop();
            }
            XmlEvent::Text { text } => {
                if text.chars().all(|c| c.is_whitespace()) {
                    let parent = stack.last().map(|s| s.as_str()).unwrap_or("");
                    if !preserve_ws_in.contains(parent) {
                        i += 1;
                        continue;
                    }
                }
                out.push(XmlEvent::Text { text });
            }
            XmlEvent::CData { text } => {
                if text.chars().all(|c| c.is_whitespace()) {
                    let parent = stack.last().map(|s| s.as_str()).unwrap_or("");
                    if !preserve_ws_in.contains(parent) {
                        i += 1;
                        continue;
                    }
                }
                out.push(XmlEvent::CData { text });
            }
            other => out.push(other),
        }

        i += 1;
    }

    if let Some(n) = skip_element {
        return Err(anyhow!(
            "filter left unterminated skip element in {}: {}",
            part.name,
            n
        ));
    }

    part.events = out;
    Ok(())
}

#[derive(Clone)]
struct NormalizedRun {
    run_start_attrs: Vec<(String, String)>,
    rpr_events: Vec<XmlEvent>,
    rpr_fingerprint: Vec<String>,
    text: String,
    has_xml_space_preserve: bool,
}

fn merge_adjacent_text_runs_in_paragraphs(events: &[XmlEvent]) -> Vec<XmlEvent> {
    let mut out: Vec<XmlEvent> = Vec::with_capacity(events.len());
    let mut stack: Vec<String> = Vec::new();
    let mut pending: Option<NormalizedRun> = None;

    let mut i = 0usize;
    while i < events.len() {
        match &events[i] {
            XmlEvent::Start { name, attrs } => {
                if name == "w:r" && stack.last().map(|s| s.as_str()) == Some("w:p") {
                    let (run_events, next_i) = collect_subtree(events, i);
                    if let Some(run) = normalize_text_run(&run_events) {
                        if let Some(prev) = pending.as_mut() {
                            if prev.rpr_fingerprint == run.rpr_fingerprint {
                                prev.text.push_str(&run.text);
                                prev.has_xml_space_preserve |= run.has_xml_space_preserve;
                            } else {
                                out.extend(render_run(prev));
                                *prev = run;
                            }
                        } else {
                            pending = Some(run);
                        }
                    } else {
                        if let Some(prev) = pending.take() {
                            out.extend(render_run(&prev));
                        }
                        out.extend(run_events);
                    }
                    i = next_i;
                    continue;
                }

                if let Some(prev) = pending.take() {
                    out.extend(render_run(&prev));
                }
                out.push(XmlEvent::Start {
                    name: name.clone(),
                    attrs: attrs.clone(),
                });
                stack.push(name.clone());
                i += 1;
            }
            XmlEvent::End { name } => {
                if name == "w:p" {
                    if let Some(prev) = pending.take() {
                        out.extend(render_run(&prev));
                    }
                } else if let Some(prev) = pending.take() {
                    out.extend(render_run(&prev));
                }

                out.push(XmlEvent::End { name: name.clone() });
                if stack.pop().as_deref() != Some(name.as_str()) {
                    // ignore mismatch
                }
                i += 1;
            }
            XmlEvent::Empty { .. } => {
                if let Some(prev) = pending.take() {
                    out.extend(render_run(&prev));
                }
                out.push(events[i].clone());
                i += 1;
            }
            _ => {
                if let Some(prev) = pending.take() {
                    out.extend(render_run(&prev));
                }
                out.push(events[i].clone());
                i += 1;
            }
        }
    }

    if let Some(prev) = pending.take() {
        out.extend(render_run(&prev));
    }

    out
}

fn collect_subtree(events: &[XmlEvent], start: usize) -> (Vec<XmlEvent>, usize) {
    let mut out: Vec<XmlEvent> = Vec::new();
    let mut depth = 0i32;

    let mut i = start;
    while i < events.len() {
        let ev = events[i].clone();
        match &ev {
            XmlEvent::Start { .. } => {
                depth += 1;
            }
            XmlEvent::End { .. } => {
                depth -= 1;
            }
            _ => {}
        }
        out.push(ev);
        i += 1;
        if depth == 0 {
            break;
        }
    }
    (out, i)
}

fn normalize_text_run(run_events: &[XmlEvent]) -> Option<NormalizedRun> {
    let XmlEvent::Start { name, attrs } = run_events.first()? else {
        return None;
    };
    if name != "w:r" {
        return None;
    }
    if !matches!(run_events.last(), Some(XmlEvent::End { name }) if name == "w:r") {
        return None;
    }

    let mut stack: Vec<String> = Vec::new();
    let mut rpr_events: Vec<XmlEvent> = Vec::new();
    let mut rpr_fingerprint: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut has_xml_space_preserve = false;

    let mut in_rpr = false;
    let mut in_t = false;
    let mut allowed = true;

    for ev in run_events.iter().skip(1).take(run_events.len().saturating_sub(2)) {
        match ev {
            XmlEvent::Start { name, attrs } => {
                if !in_rpr && !in_t && stack.is_empty() {
                    if name == "w:rPr" {
                        in_rpr = true;
                        rpr_events.push(XmlEvent::Start {
                            name: name.clone(),
                            attrs: attrs.clone(),
                        });
                        continue;
                    }
                    if name == "w:t" {
                        in_t = true;
                        if attrs.iter().any(|(k, v)| k == "xml:space" && v == "preserve") {
                            has_xml_space_preserve = true;
                        }
                        stack.push(name.clone());
                        continue;
                    }
                    allowed = false;
                    break;
                }

                if in_rpr {
                    rpr_events.push(XmlEvent::Start {
                        name: name.clone(),
                        attrs: attrs.clone(),
                    });
                    rpr_fingerprint.push(fingerprint_start_like(name, attrs, false));
                    stack.push(name.clone());
                    continue;
                }

                if in_t {
                    allowed = false;
                    break;
                }

                // Nested structure inside run; treat as non-mergeable.
                allowed = false;
                break;
            }
            XmlEvent::Empty { name, attrs } => {
                if !in_rpr && !in_t && stack.is_empty() {
                    allowed = false;
                    break;
                }
                if in_rpr {
                    rpr_events.push(XmlEvent::Empty {
                        name: name.clone(),
                        attrs: attrs.clone(),
                    });
                    rpr_fingerprint.push(fingerprint_start_like(name, attrs, true));
                    continue;
                }
                if in_t {
                    allowed = false;
                    break;
                }
                allowed = false;
                break;
            }
            XmlEvent::End { name } => {
                if in_rpr {
                    if name == "w:rPr" {
                        in_rpr = false;
                        rpr_events.push(XmlEvent::End { name: name.clone() });
                        continue;
                    }
                    rpr_events.push(XmlEvent::End { name: name.clone() });
                    rpr_fingerprint.push(format!("</{name}>"));
                    stack.pop();
                    continue;
                }
                if in_t {
                    if name == "w:t" {
                        in_t = false;
                        stack.pop();
                        continue;
                    }
                    allowed = false;
                    break;
                }
                allowed = false;
                break;
            }
            XmlEvent::Text { text: t } => {
                if in_t {
                    text.push_str(t);
                    continue;
                }
                if in_rpr {
                    // Ignore whitespace-only pretty-printing inside rPr.
                    if t.chars().all(|c| c.is_whitespace()) {
                        continue;
                    }
                    allowed = false;
                    break;
                }
                if t.chars().all(|c| c.is_whitespace()) {
                    continue;
                }
                allowed = false;
                break;
            }
            XmlEvent::CData { .. } => {
                allowed = false;
                break;
            }
            _ => {
                allowed = false;
                break;
            }
        }
    }

    if !allowed || in_rpr || in_t || !stack.is_empty() {
        return None;
    }

    let rpr_events = if rpr_events.is_empty() {
        Vec::new()
    } else if rpr_events.len() == 2 && matches!(rpr_events.first(), Some(XmlEvent::Start { name, .. }) if name == "w:rPr") {
        // Only <w:rPr></w:rPr>
        Vec::new()
    } else {
        rpr_events
    };

    Some(NormalizedRun {
        run_start_attrs: attrs.clone(),
        rpr_events,
        rpr_fingerprint,
        text,
        has_xml_space_preserve,
    })
}

fn fingerprint_start_like(name: &str, attrs: &[(String, String)], empty: bool) -> String {
    let mut a = attrs.to_vec();
    a.sort_by(|(ka, va), (kb, vb)| ka.cmp(kb).then(va.cmp(vb)));
    let mut s = String::new();
    s.push('<');
    s.push_str(name);
    for (k, v) in a {
        s.push(' ');
        s.push_str(&k);
        s.push('=');
        s.push_str(&v);
    }
    if empty {
        s.push_str("/>");
    } else {
        s.push('>');
    }
    s
}

fn render_run(run: &NormalizedRun) -> Vec<XmlEvent> {
    let mut out: Vec<XmlEvent> = Vec::new();
    out.push(XmlEvent::Start {
        name: "w:r".to_string(),
        attrs: run.run_start_attrs.clone(),
    });
    out.extend(run.rpr_events.clone());

    let mut t_attrs: Vec<(String, String)> = Vec::new();
    if run.has_xml_space_preserve
        || run.text.starts_with(|c: char| c.is_whitespace())
        || run.text.ends_with(|c: char| c.is_whitespace())
    {
        t_attrs.push(("xml:space".to_string(), "preserve".to_string()));
    }
    out.push(XmlEvent::Start {
        name: "w:t".to_string(),
        attrs: t_attrs,
    });
    out.push(XmlEvent::Text { text: run.text.clone() });
    out.push(XmlEvent::End { name: "w:t".to_string() });

    out.push(XmlEvent::End { name: "w:r".to_string() });
    out
}
