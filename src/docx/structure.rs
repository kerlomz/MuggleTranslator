use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::docx::pure_text::{extract_pure_text, ParaContainer, PureParagraph, PureTextJson};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructureNodeKind {
    Root,
    Part,
    Heading,
    List,
    ListItem,
    Paragraph,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParaLoc {
    pub para_id: usize,
    pub part_name: String,
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StructureNode {
    pub kind: StructureNodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc: Option<ParaLoc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading_level: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ilvl: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub children: Vec<StructureNode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StructureJson {
    pub version: u32,
    pub placeholder_prefix: String,
    pub root: StructureNode,
}

pub struct StructureOutputs {
    pub structure_json_path: PathBuf,
}

#[derive(Default)]
struct ListCtx {
    section_parent: Option<usize>,
    active_key: Option<String>,
    active_num_id: Option<i32>,
    list_node_stack: Vec<usize>,
    last_item_stack: Vec<Option<usize>>,
}

impl ListCtx {
    fn reset(&mut self) {
        self.section_parent = None;
        self.active_key = None;
        self.active_num_id = None;
        self.list_node_stack.clear();
        self.last_item_stack.clear();
    }
}

#[derive(Clone, Debug)]
struct ArenaNode {
    kind: StructureNodeKind,
    part_name: Option<String>,
    loc: Option<ParaLoc>,
    heading_level: Option<u32>,
    num_id: Option<i32>,
    ilvl: Option<i32>,
    text: Option<String>,
    children: Vec<usize>,
}

fn arena_add(arena: &mut Vec<ArenaNode>, parent: Option<usize>, node: ArenaNode) -> usize {
    let idx = arena.len();
    arena.push(node);
    if let Some(p) = parent {
        arena[p].children.push(idx);
    }
    idx
}

fn arena_to_tree(idx: usize, arena: &[ArenaNode]) -> StructureNode {
    let n = &arena[idx];
    StructureNode {
        kind: n.kind,
        part_name: n.part_name.clone(),
        loc: n.loc.clone(),
        heading_level: n.heading_level,
        num_id: n.num_id,
        ilvl: n.ilvl,
        text: n.text.clone(),
        children: n
            .children
            .iter()
            .copied()
            .map(|c| arena_to_tree(c, arena))
            .collect(),
    }
}

fn para_loc(p: &PureParagraph) -> ParaLoc {
    ParaLoc {
        para_id: p.para_id,
        part_name: p.part_name.clone(),
        xml_event_index: p.xml_event_index,
        container: p.container,
        section_index: p.section_index,
        table_index: p.table_index,
        row_index: p.row_index,
        cell_index: p.cell_index,
        p_style: p.p_style.clone(),
        num_id: p.num_id,
        num_ilvl: p.num_ilvl,
        outline_lvl: p.outline_lvl,
    }
}

fn heading_level(p: &PureParagraph) -> Option<usize> {
    if let Some(lvl) = p.outline_lvl {
        if lvl >= 0 {
            return Some(lvl as usize + 1);
        }
    }
    let style = p.p_style.as_deref()?.trim();
    if style.is_empty() {
        return None;
    }
    let lower = style.to_ascii_lowercase();
    if lower.starts_with("heading") {
        let digits: String = style.chars().skip_while(|c| !c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<usize>() {
            if n > 0 {
                return Some(n);
            }
        }
    }
    if lower == "title" {
        return Some(1);
    }
    None
}

fn list_key(p: &PureParagraph) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        p.container, p.section_index, p.table_index, p.row_index, p.cell_index, p.part_name
    )
}

fn build_part_tree(arena: &mut Vec<ArenaNode>, part_idx: usize, paras: &[PureParagraph]) {
    let mut section_stack: Vec<usize> = vec![part_idx];
    let mut list_ctx = ListCtx::default();

    for p in paras {
        if let Some(level) = heading_level(p) {
            list_ctx.reset();
            while section_stack.len() > level {
                section_stack.pop();
            }
            let parent = *section_stack.last().unwrap_or(&part_idx);
            let idx = arena_add(
                arena,
                Some(parent),
                ArenaNode {
                    kind: StructureNodeKind::Heading,
                    part_name: None,
                    loc: Some(para_loc(p)),
                    heading_level: Some(level as u32),
                    num_id: p.num_id,
                    ilvl: p.num_ilvl,
                    text: Some(p.text.clone()),
                    children: Vec::new(),
                },
            );
            section_stack.push(idx);
            continue;
        }

        let section_parent = *section_stack.last().unwrap_or(&part_idx);
        if let (Some(num_id), Some(ilvl)) = (p.num_id, p.num_ilvl) {
            let level = if ilvl < 0 { 0 } else { ilvl as usize };
            let key = list_key(p);

            let needs_reset = list_ctx.section_parent != Some(section_parent)
                || list_ctx.active_num_id != Some(num_id)
                || list_ctx.active_key.as_deref() != Some(key.as_str())
                || list_ctx.list_node_stack.is_empty();
            if needs_reset {
                list_ctx.reset();
                list_ctx.section_parent = Some(section_parent);
                list_ctx.active_num_id = Some(num_id);
                list_ctx.active_key = Some(key);
                let list0 = arena_add(
                    arena,
                    Some(section_parent),
                    ArenaNode {
                        kind: StructureNodeKind::List,
                        part_name: None,
                        loc: None,
                        heading_level: None,
                        num_id: Some(num_id),
                        ilvl: Some(0),
                        text: None,
                        children: Vec::new(),
                    },
                );
                list_ctx.list_node_stack.push(list0);
                list_ctx.last_item_stack.push(None);
            }

            if level == 0 {
                list_ctx.list_node_stack.truncate(1);
                list_ctx.last_item_stack.truncate(1);
            } else {
                if list_ctx.list_node_stack.len() > level + 1 {
                    list_ctx.list_node_stack.truncate(level + 1);
                }
                if list_ctx.last_item_stack.len() > level + 1 {
                    list_ctx.last_item_stack.truncate(level + 1);
                }

                while list_ctx.list_node_stack.len() < level + 1 {
                    let prev_level = list_ctx.list_node_stack.len() - 1;
                    let parent_item = list_ctx.last_item_stack[prev_level].unwrap_or(section_parent);
                    let new_level = list_ctx.list_node_stack.len() as i32;
                    let list_node = arena_add(
                        arena,
                        Some(parent_item),
                        ArenaNode {
                            kind: StructureNodeKind::List,
                            part_name: None,
                            loc: None,
                            heading_level: None,
                            num_id: Some(num_id),
                            ilvl: Some(new_level),
                            text: None,
                            children: Vec::new(),
                        },
                    );
                    list_ctx.list_node_stack.push(list_node);
                    list_ctx.last_item_stack.push(None);
                }
            }

            let list_node = *list_ctx.list_node_stack.last().unwrap();
            let item_idx = arena_add(
                arena,
                Some(list_node),
                ArenaNode {
                    kind: StructureNodeKind::ListItem,
                    part_name: None,
                    loc: Some(para_loc(p)),
                    heading_level: None,
                    num_id: Some(num_id),
                    ilvl: Some(ilvl),
                    text: Some(p.text.clone()),
                    children: Vec::new(),
                },
            );
            if list_ctx.last_item_stack.len() < level + 1 {
                list_ctx.last_item_stack.resize(level + 1, None);
            }
            list_ctx.last_item_stack[level] = Some(item_idx);
            continue;
        }

        list_ctx.reset();
        let _ = arena_add(
            arena,
            Some(section_parent),
            ArenaNode {
                kind: StructureNodeKind::Paragraph,
                part_name: None,
                loc: Some(para_loc(p)),
                heading_level: None,
                num_id: p.num_id,
                ilvl: p.num_ilvl,
                text: Some(p.text.clone()),
                children: Vec::new(),
            },
        );
    }
}

pub fn build_structure(pure: &PureTextJson) -> StructureJson {
    let mut by_part: BTreeMap<String, Vec<PureParagraph>> = BTreeMap::new();
    for p in &pure.paragraphs {
        by_part.entry(p.part_name.clone()).or_default().push(p.clone());
    }

    let mut arena: Vec<ArenaNode> = Vec::new();
    let root_idx = arena_add(
        &mut arena,
        None,
        ArenaNode {
            kind: StructureNodeKind::Root,
            part_name: None,
            loc: None,
            heading_level: None,
            num_id: None,
            ilvl: None,
            text: None,
            children: Vec::new(),
        },
    );

    for (part_name, mut paras) in by_part {
        paras.sort_by_key(|p| p.xml_event_index);
        let part_idx = arena_add(
            &mut arena,
            Some(root_idx),
            ArenaNode {
                kind: StructureNodeKind::Part,
                part_name: Some(part_name.clone()),
                loc: None,
                heading_level: None,
                num_id: None,
                ilvl: None,
                text: None,
                children: Vec::new(),
            },
        );
        build_part_tree(&mut arena, part_idx, &paras);
    }

    StructureJson {
        version: 1,
        placeholder_prefix: pure.placeholder_prefix.clone(),
        root: arena_to_tree(root_idx, &arena),
    }
}

pub fn extract_structure_json(input_docx: &Path, output_json: &Path) -> anyhow::Result<()> {
    let pure = extract_pure_text(input_docx)?;
    let out = build_structure(&pure);
    fs::write(
        output_json,
        serde_json::to_vec_pretty(&out).context("serialize structure json")?,
    )
    .with_context(|| format!("write structure json: {}", output_json.display()))?;
    Ok(())
}

pub fn default_structure_output_for(input_docx: &Path) -> StructureOutputs {
    let stem = input_docx
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("docx");
    let dir = input_docx.parent().unwrap_or_else(|| Path::new("."));
    StructureOutputs {
        structure_json_path: dir.join(format!("{stem}.structure.json")),
    }
}

