use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

use crate::ir::FreezeMaskSpan;
use crate::sentinels::{nt_token, ANY_SENTINEL_RE, NT_RE};

#[derive(Debug, Clone)]
pub struct FreezeResult {
    pub text: String,
    pub nt_map: HashMap<String, String>,
    pub mask: Vec<FreezeMaskSpan>,
}

static FREEZE_RE: Lazy<Regex> = Lazy::new(|| {
    let url = r"https?://[^\s<>()]+";
    let email = r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}";
    let win_path = r#"(?:[A-Za-z]:\\(?:[^\\/:*?"<>|\r\n]+\\)*[^\\/:*?"<>|\r\n]*)"#;
    let placeholder = r"(?:\{[^{}\r\n]{1,100}\}|\$\{[^{}\r\n]{1,100}\})";
    let percent_slot = r"%\d+";
    // Preserve structured identifiers such as "4.1(b)", "1-2.3", "(i)/(ii)", and "IV".
    let clause_ref = r"(?:\b\d+(?:[.,]\d+)+(?:-\d+(?:[.,]\d+)*)?(?:\([A-Za-z0-9]+\))*\b|\b\d+(?:-\d+)+(?:\([A-Za-z0-9]+\))*\b|\b\d+\([A-Za-z0-9]+\)(?:\([A-Za-z0-9]+\))*\b|\b[IVXLCDM]{1,8}\b)";
    let enum_num = r"\(\d{1,3}\)";
    let enum_roman = r"\((?:[ivxlcdmIVXLCDM]{1,6})\)";
    let enum_alpha = r"\([A-Za-z]\)";
    let dot_leader = r"[.\u2026]{8,}";
    let underscore_leader = r"_{5,}";
    let dash_leader = r"[-\u2010\u2011\u2012\u2013\u2014\u2015\u2212]{5,}";
    let trademark_token = r"[\p{L}\p{N}]{2,24}[\u00AE\u2122\u00A9]";
    let other_script_run =
        r"[\u0900-\u097F\u0980-\u09FF\u0600-\u06FF\u0400-\u04FF\u0370-\u03FF\u0590-\u05FF\u0E00-\u0E7F\uAC00-\uD7AF\u3040-\u309F\u30A0-\u30FF]+";
    let var_marker = r"\b[XYZ]\b";

    let pat = format!(
        "({trademark_token}|{other_script_run}|{url}|{email}|{win_path}|{placeholder}|{percent_slot}|{clause_ref}|{enum_num}|{enum_roman}|{enum_alpha}|{dot_leader}|{underscore_leader}|{dash_leader}|{var_marker})"
    );
    Regex::new(&pat).expect("freeze regex")
});

pub fn freeze_text(text: &str) -> FreezeResult {
    let mut nt_map: HashMap<String, String> = HashMap::new();
    let mut mask: Vec<FreezeMaskSpan> = Vec::new();
    let mut next_id: usize = 1;

    let mut add_token = |original: &str| -> String {
        let token = nt_token(next_id);
        next_id += 1;
        nt_map.insert(token.clone(), original.to_string());
        token
    };

    let freeze_plain = |plain: &str,
                        base: usize,
                        mask: &mut Vec<FreezeMaskSpan>,
                        add_token: &mut dyn FnMut(&str) -> String|
     -> String {
        if plain.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(plain.len());
        let mut pos = 0usize;
        for m in FREEZE_RE.find_iter(plain) {
            if m.start() > pos {
                out.push_str(&plain[pos..m.start()]);
            }
            let original = &plain[m.start()..m.end()];
            let token = add_token(original);
            mask.push(FreezeMaskSpan {
                src_start: base.saturating_add(m.start()),
                src_end: base.saturating_add(m.end()),
                token: token.clone(),
                original: original.to_string(),
            });
            out.push_str(&token);
            pos = m.end();
        }
        if pos < plain.len() {
            out.push_str(&plain[pos..]);
        }
        out
    };

    let mut pieces: Vec<String> = Vec::new();
    let mut pos = 0usize;
    for m in ANY_SENTINEL_RE.find_iter(text) {
        pieces.push(freeze_plain(
            &text[pos..m.start()],
            pos,
            &mut mask,
            &mut add_token,
        ));
        pieces.push(m.as_str().to_string());
        pos = m.end();
    }
    pieces.push(freeze_plain(&text[pos..], pos, &mut mask, &mut add_token));

    FreezeResult {
        text: pieces.concat(),
        nt_map,
        mask,
    }
}

pub fn unfreeze_text(text: &str, nt_map: &HashMap<String, String>) -> String {
    if nt_map.is_empty() || text.is_empty() {
        return text.to_string();
    }
    NT_RE
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let tok = caps.get(0).unwrap().as_str();
            nt_map.get(tok).cloned().unwrap_or_else(|| tok.to_string())
        })
        .into_owned()
}
