use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

use crate::freezer::unfreeze_text;
use crate::ir::TranslationUnit;
use crate::sentinels::{
    control_tokens_from_text, is_control_token, split_by_control_sequence, ANY_MT_TOKEN_RE,
    ANY_SENTINEL_RE,
};

static DIGIT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+").expect("digit regex"));
static EN_LEGAL_REF_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:section|article|clause|paragraph|schedule|sec|art|cl|para|sch)\.?\s+(\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?(?:\([A-Za-z0-9]+\))*|[IVXLCDM]{1,8})\b",
    )
    .expect("en legal ref regex")
});
static LEGAL_ID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b(\d+(?:[.,]\d+)*(?:-\d+(?:[.,]\d+)*)?(?:\([A-Za-z0-9]+\))*|[IVXLCDM]{1,8})\b")
        .expect("legal id regex")
});

#[derive(Clone, Debug, Default)]
pub struct QualityHeuristics {
    pub hard_flags: Vec<String>,
    pub soft_flags: Vec<String>,
    pub src_chars: usize,
    pub tgt_chars: usize,
    pub len_ratio: f32,
    pub han_ratio: f32,
    pub latin_alpha_ratio: f32,
    pub bracket_notes: Vec<String>,
}

impl QualityHeuristics {
    #[must_use]
    pub fn render_block(&self) -> String {
        let mut out = String::new();
        out.push_str("QUALITY_HEURISTICS:\n");
        out.push_str(&format!(
            "- len: src_chars={} tgt_chars={} ratio={:.2}\n",
            self.src_chars, self.tgt_chars, self.len_ratio
        ));
        out.push_str(&format!(
            "- scripts: han_ratio={:.2} latin_alpha_ratio={:.2}\n",
            self.han_ratio, self.latin_alpha_ratio
        ));
        if !self.bracket_notes.is_empty() {
            out.push_str("- brackets: ");
            out.push_str(&self.bracket_notes.join(" | "));
            out.push('\n');
        }
        if !self.hard_flags.is_empty() {
            out.push_str("- hard_flags: ");
            out.push_str(&self.hard_flags.join(" | "));
            out.push('\n');
        }
        if !self.soft_flags.is_empty() {
            out.push_str("- soft_flags: ");
            out.push_str(&self.soft_flags.join(" | "));
            out.push('\n');
        }
        out.trim().to_string()
    }

    #[must_use]
    pub fn wants_force_retranslate(&self) -> bool {
        self.hard_flags.iter().any(|f| {
            f == "output_identical_to_source"
                || f.starts_with("target_script_missing_")
                || f == "len_ratio_too_short_extreme"
                || f == "len_ratio_too_long_extreme"
        })
    }
}

pub fn validate_translation(tu: &TranslationUnit, translated: &str) -> anyhow::Result<()> {
    if translated.trim().is_empty() {
        return Err(anyhow!("empty_output"));
    }

    // Disallow any <<MT_...>> tokens not present in source (hallucinations / prompt leakage).
    // Keep this separate from ANY_SENTINEL_RE so we don't accidentally treat unknown MT-like tokens
    // as must-preserve sentinels.
    let src_mt: HashSet<String> = ANY_MT_TOKEN_RE
        .find_iter(&tu.frozen_surface)
        .map(|m| m.as_str().to_string())
        .collect();
    for m in ANY_MT_TOKEN_RE.find_iter(translated) {
        let tok = m.as_str();
        if !src_mt.contains(tok) {
            return Err(anyhow!("unexpected_mt_token:{tok}"));
        }
    }

    let src_sentinels: Vec<String> = ANY_SENTINEL_RE
        .find_iter(&tu.frozen_surface)
        .map(|m| m.as_str().to_string())
        .collect();
    let tgt_sentinels: Vec<String> = ANY_SENTINEL_RE
        .find_iter(translated)
        .map(|m| m.as_str().to_string())
        .collect();
    if src_sentinels != tgt_sentinels {
        return Err(anyhow!("sentinel_sequence_mismatch"));
    }
    let src_ctrl = control_tokens_from_text(&tu.frozen_surface);
    let tgt_ctrl = control_tokens_from_text(translated);
    if src_ctrl != tgt_ctrl {
        return Err(anyhow!("control_token_sequence_mismatch"));
    }
    // Control tokens are "layout separators" used by the DOCX projection algorithm. It's not enough
    // to preserve the token sequence; the text must not "move across" control boundaries.
    // In particular, when the source has empty text blocks between consecutive control tokens
    // (e.g. "<<MT_TAB>><<MT_TAB>>..."), the corresponding target blocks must stay empty.
    let src_parts = split_by_control_sequence(&tu.frozen_surface);
    let tgt_parts = split_by_control_sequence(translated);
    if src_parts.len() != tgt_parts.len() {
        return Err(anyhow!("control_token_layout_mismatch"));
    }
    for (s, t) in src_parts.iter().zip(tgt_parts.iter()) {
        if is_control_token(s) {
            if s != t {
                return Err(anyhow!("control_token_layout_mismatch"));
            }
            continue;
        }
        if s.trim().is_empty() != t.trim().is_empty() {
            return Err(anyhow!("control_token_layout_mismatch"));
        }
    }
    for (tok, _) in &tu.nt_map {
        let src_count = tu.frozen_surface.matches(tok).count();
        let tgt_count = translated.matches(tok).count();
        if src_count != tgt_count {
            return Err(anyhow!("nt_token_count_mismatch:{tok}"));
        }
    }
    let src_unfrozen = unfreeze_text(&tu.frozen_surface, &tu.nt_map);
    let tgt_unfrozen = unfreeze_text(translated, &tu.nt_map);
    let src_plain = ANY_MT_TOKEN_RE.replace_all(&src_unfrozen, " ");
    let tgt_plain = ANY_MT_TOKEN_RE.replace_all(&tgt_unfrozen, " ");
    let src_digits = digit_counter(&src_plain);
    let tgt_digits = digit_counter(&tgt_plain);
    if src_digits != tgt_digits {
        return Err(anyhow!(
            "digits_mismatch src={:?} tgt={:?}",
            src_digits,
            tgt_digits
        ));
    }

    // Preserve structured legal references like "Section 4.1(b)" even though the keyword is translated.
    // We validate the identifier part (e.g., "4.1(b)", "IV") against the target.
    let src_legal_ids = en_legal_ref_ids(&src_plain);
    if !src_legal_ids.is_empty() {
        let src_map = string_counter(src_legal_ids.into_iter().filter(is_compound_legal_id));
        if !src_map.is_empty() {
            let tgt_ids = legal_id_candidates(&tgt_plain);
            let tgt_map = string_counter(tgt_ids.into_iter().filter(is_compound_legal_id));
            for (id, cnt) in src_map {
                let got = tgt_map.get(&id).cloned().unwrap_or(0);
                if got != cnt {
                    return Err(anyhow!(
                        "legal_ref_id_mismatch id={id} expected={cnt} got={got}"
                    ));
                }
            }
        }
    }

    // General compound identifier preservation (e.g., 4.1(b), 1-2.3, IV) regardless of keyword language.
    let src_compound = string_counter(
        legal_id_candidates(&src_plain)
            .into_iter()
            .filter(is_compound_legal_id),
    );
    if !src_compound.is_empty() {
        let tgt_compound = string_counter(
            legal_id_candidates(&tgt_plain)
                .into_iter()
                .filter(is_compound_legal_id),
        );
        if src_compound != tgt_compound {
            return Err(anyhow!(
                "compound_legal_id_mismatch src={:?} tgt={:?}",
                src_compound,
                tgt_compound
            ));
        }
    }
    Ok(())
}

#[must_use]
pub fn quality_heuristics(
    tu: &TranslationUnit,
    translated: &str,
    source_lang: &str,
    target_lang: &str,
) -> QualityHeuristics {
    let src_unfrozen = unfreeze_text(&tu.frozen_surface, &tu.nt_map);
    let tgt_unfrozen = unfreeze_text(translated, &tu.nt_map);
    let src_plain = ANY_MT_TOKEN_RE.replace_all(&src_unfrozen, " ").into_owned();
    let tgt_plain = ANY_MT_TOKEN_RE.replace_all(&tgt_unfrozen, " ").into_owned();

    let src_metrics = text_metrics(&src_plain);
    let tgt_metrics = text_metrics(&tgt_plain);
    let src_chars = src_metrics.non_ws;
    let tgt_chars = tgt_metrics.non_ws;
    let len_ratio = if src_chars == 0 {
        0.0
    } else {
        tgt_chars as f32 / src_chars as f32
    };

    let han_ratio = if tgt_chars == 0 {
        0.0
    } else {
        tgt_metrics.han as f32 / tgt_chars as f32
    };
    let latin_alpha_ratio = if tgt_chars == 0 {
        0.0
    } else {
        tgt_metrics.latin_alpha as f32 / tgt_chars as f32
    };

    let mut hard_flags: Vec<String> = Vec::new();
    let mut soft_flags: Vec<String> = Vec::new();

    let src_norm = normalize_for_similarity(&src_plain);
    let tgt_norm = normalize_for_similarity(&tgt_plain);
    if !src_norm.is_empty() && src_norm == tgt_norm && src_norm.len() >= 24 {
        hard_flags.push("output_identical_to_source".to_string());
    }

    if src_chars >= 40 {
        if len_ratio > 0.0 && len_ratio < 0.25 {
            hard_flags.push("len_ratio_too_short_extreme".to_string());
        } else if len_ratio > 4.0 {
            hard_flags.push("len_ratio_too_long_extreme".to_string());
        } else if len_ratio > 0.0 && len_ratio < 0.35 {
            soft_flags.push("len_ratio_too_short".to_string());
        } else if len_ratio > 2.8 {
            soft_flags.push("len_ratio_too_long".to_string());
        }
    }

    let tgt_lang = target_lang.trim().to_ascii_lowercase();
    let src_lang = source_lang.trim().to_ascii_lowercase();
    let lang_diff = !src_lang.is_empty() && !tgt_lang.is_empty() && src_lang != tgt_lang;
    if lang_diff && tgt_chars >= 20 {
        if tgt_lang.starts_with("zh") {
            if han_ratio < 0.06 && latin_alpha_ratio > 0.25 {
                hard_flags.push("target_script_missing_han".to_string());
            }
        } else if tgt_lang.starts_with("en") {
            if latin_alpha_ratio < 0.18 && han_ratio > 0.20 {
                hard_flags.push("target_script_missing_latin".to_string());
            } else if han_ratio > 0.25 {
                soft_flags.push("target_contains_much_han".to_string());
            }
        } else if tgt_lang.starts_with("ja") {
            if (han_ratio + tgt_metrics.jp_kana_ratio(tgt_chars)) < 0.10 && latin_alpha_ratio > 0.25
            {
                hard_flags.push("target_script_missing_japanese".to_string());
            }
        } else if tgt_lang.starts_with("ko") {
            if tgt_metrics.hangul_ratio(tgt_chars) < 0.10 && latin_alpha_ratio > 0.25 {
                hard_flags.push("target_script_missing_korean".to_string());
            }
        }
    }

    let bracket_notes =
        bracket_mismatch_notes(&src_plain, &tgt_plain, &mut hard_flags, &mut soft_flags);

    QualityHeuristics {
        hard_flags,
        soft_flags,
        src_chars,
        tgt_chars,
        len_ratio,
        han_ratio,
        latin_alpha_ratio,
        bracket_notes,
    }
}

fn digit_counter(text: &str) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for m in DIGIT_RE.find_iter(text) {
        let s = m.as_str().to_string();
        *out.entry(s).or_insert(0) += 1;
    }
    out
}

fn string_counter(items: impl IntoIterator<Item = String>) -> HashMap<String, usize> {
    let mut out: HashMap<String, usize> = HashMap::new();
    for s in items {
        if s.is_empty() {
            continue;
        }
        *out.entry(s).or_insert(0) += 1;
    }
    out
}

fn en_legal_ref_ids(text: &str) -> Vec<String> {
    EN_LEGAL_REF_RE
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| normalize_legal_id(m.as_str())))
        .collect()
}

fn legal_id_candidates(text: &str) -> Vec<String> {
    LEGAL_ID_RE
        .captures_iter(text)
        .filter_map(|c| c.get(1).map(|m| normalize_legal_id(m.as_str())))
        .collect()
}

fn is_compound_legal_id(id: &String) -> bool {
    let has_struct = id.contains('.') || id.contains('-') || id.contains('(') || id.contains(')');
    let has_alpha = id.chars().any(|c| c.is_ascii_alphabetic());
    has_struct || has_alpha
}

fn normalize_legal_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        let mapped = match ch {
            '（' => '(',
            '）' => ')',
            '，' | ',' => '.',
            '．' | '。' => '.',
            '－' | '–' | '—' | '−' => '-',
            c if c.is_whitespace() => continue,
            c => c,
        };
        out.push(mapped);
    }
    out.to_ascii_uppercase()
}

#[derive(Clone, Copy, Debug, Default)]
struct TextMetrics {
    non_ws: usize,
    han: usize,
    latin_alpha: usize,
    jp_kana: usize,
    hangul: usize,
}

impl TextMetrics {
    fn jp_kana_ratio(&self, total: usize) -> f32 {
        if total == 0 {
            0.0
        } else {
            self.jp_kana as f32 / total as f32
        }
    }

    fn hangul_ratio(&self, total: usize) -> f32 {
        if total == 0 {
            0.0
        } else {
            self.hangul as f32 / total as f32
        }
    }
}

fn text_metrics(text: &str) -> TextMetrics {
    let mut m = TextMetrics::default();
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        m.non_ws = m.non_ws.saturating_add(1);
        if ch.is_ascii_alphabetic() {
            m.latin_alpha = m.latin_alpha.saturating_add(1);
        }
        if is_han(ch) {
            m.han = m.han.saturating_add(1);
        } else if is_jp_kana(ch) {
            m.jp_kana = m.jp_kana.saturating_add(1);
        } else if is_hangul(ch) {
            m.hangul = m.hangul.saturating_add(1);
        }
    }
    m
}

fn is_han(ch: char) -> bool {
    let u = ch as u32;
    (0x3400..=0x4DBF).contains(&u)
        || (0x4E00..=0x9FFF).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0x20000..=0x2A6DF).contains(&u)
        || (0x2A700..=0x2B73F).contains(&u)
        || (0x2B740..=0x2B81F).contains(&u)
        || (0x2B820..=0x2CEAF).contains(&u)
        || (0x2CEB0..=0x2EBEF).contains(&u)
}

fn is_jp_kana(ch: char) -> bool {
    let u = ch as u32;
    (0x3040..=0x309F).contains(&u)
        || (0x30A0..=0x30FF).contains(&u)
        || (0x31F0..=0x31FF).contains(&u)
}

fn is_hangul(ch: char) -> bool {
    let u = ch as u32;
    (0xAC00..=0xD7AF).contains(&u)
        || (0x1100..=0x11FF).contains(&u)
        || (0x3130..=0x318F).contains(&u)
}

fn normalize_for_similarity(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        let mapped = match ch {
            '（' => '(',
            '）' => ')',
            '【' => '[',
            '】' => ']',
            '｛' => '{',
            '｝' => '}',
            '，' => ',',
            '。' => '.',
            '：' => ':',
            '；' => ';',
            '！' => '!',
            '？' => '?',
            '“' | '”' | '„' | '‟' => '"',
            '‘' | '’' | '‚' | '‛' => '\'',
            c => c,
        };
        for lc in mapped.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

fn canonical_bracket(ch: char) -> Option<char> {
    let mapped = match ch {
        '(' | '（' => '(',
        ')' | '）' => ')',
        '[' | '【' => '[',
        ']' | '】' => ']',
        '{' | '｛' => '{',
        '}' | '｝' => '}',
        '<' | '《' => '<',
        '>' | '》' => '>',
        _ => return None,
    };
    Some(mapped)
}

fn bracket_counts(text: &str) -> HashMap<char, usize> {
    let mut out: HashMap<char, usize> = HashMap::new();
    for ch in text.chars() {
        if let Some(b) = canonical_bracket(ch) {
            *out.entry(b).or_insert(0) += 1;
        }
    }
    out
}

fn bracket_mismatch_notes(
    src: &str,
    tgt: &str,
    hard_flags: &mut Vec<String>,
    soft_flags: &mut Vec<String>,
) -> Vec<String> {
    let src_map = bracket_counts(src);
    let tgt_map = bracket_counts(tgt);
    let pairs: &[(char, char, &str, &str)] = &[
        ('(', ')', "parens", "()"),
        ('[', ']', "square", "[]"),
        ('{', '}', "curly", "{}"),
        ('<', '>', "angle", "<>"),
    ];

    let mut notes = Vec::new();
    for (open, close, id, display) in pairs {
        let src_total =
            src_map.get(open).cloned().unwrap_or(0) + src_map.get(close).cloned().unwrap_or(0);
        let tgt_total =
            tgt_map.get(open).cloned().unwrap_or(0) + tgt_map.get(close).cloned().unwrap_or(0);
        if src_total == 0 && tgt_total == 0 {
            continue;
        }
        if src_total > 0 && tgt_total == 0 {
            hard_flags.push(format!("missing_brackets_{id}"));
            notes.push(format!("{display}:src={src_total} tgt={tgt_total}"));
            continue;
        }
        if src_total != tgt_total {
            if (src_total as i32 - tgt_total as i32).abs() >= 4 {
                soft_flags.push(format!("bracket_count_mismatch_{id}"));
            } else {
                soft_flags.push("bracket_count_mismatch".to_string());
            }
            notes.push(format!("{display}:src={src_total} tgt={tgt_total}"));
        }
    }
    notes
}

pub fn must_extract_json_obj(text: &str) -> anyhow::Result<serde_json::Value> {
    let start = text.find('{').context("no_json_object_start")?;
    let slice = &text[start..];
    let mut de = serde_json::Deserializer::from_str(slice);
    let v: serde_json::Value =
        serde_json::Value::deserialize(&mut de).context("json_parse_failed")?;
    Ok(v)
}
