use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextNodeKind {
    Wt,
    At,
    Attr,
}

#[derive(Clone, Debug)]
pub struct TextNodeRef {
    pub part_name: String,
    pub kind: TextNodeKind,
    pub elem_event_index: usize,
    pub text_event_index: Option<usize>,
    pub attr_name: Option<String>,
    pub original_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomKind {
    Text,
    Tab,
    Br,
    Nbh,
    Shy,
}

#[derive(Clone, Debug)]
pub struct Atom {
    pub kind: AtomKind,
    pub node_ref: Option<TextNodeRef>,
    pub value: String,
    pub style_sig: String,
}

#[derive(Clone, Debug)]
pub struct FormatSpan {
    pub style_sig: String,
    pub node_refs: Vec<TextNodeRef>,
    pub source_text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FreezeMaskSpan {
    pub src_start: usize,
    pub src_end: usize,
    pub token: String,
    pub original: String,
}

#[derive(Clone, Debug)]
pub struct TranslationUnit {
    pub tu_id: usize,
    pub part_name: String,
    pub scope_key: String,
    pub para_style: Option<String>,
    pub atoms: Vec<Atom>,
    pub spans: Vec<FormatSpan>,
    pub source_surface: String,
    pub frozen_surface: String,
    pub nt_map: HashMap<String, String>,
    pub nt_mask: Vec<FreezeMaskSpan>,
    pub draft_translation: Option<String>,
    pub final_translation: Option<String>,
    pub alt_translation: Option<String>,
    pub draft_translation_model: Option<String>,
    pub alt_translation_model: Option<String>,
    pub qe_score: Option<i32>,
    pub qe_flags: Vec<String>,
}
