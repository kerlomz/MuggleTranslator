use anyhow::{anyhow, Context};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

pub const SEG_ID_WIDTH: usize = 6;
pub const NT_ID_WIDTH: usize = 4;
pub const SLOT_ID_WIDTH: usize = 6;

pub const TAB: &str = "<<MT_TAB>>";
pub const BR: &str = "<<MT_BR>>";
pub const NBH: &str = "<<MT_NBH>>";
pub const SHY: &str = "<<MT_SHY>>";

pub const CONTROL_TOKENS: [&str; 4] = [TAB, BR, NBH, SHY];

static CONTROL_TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<<MT_(?:TAB|BR|NBH|SHY)>>").expect("control tok regex"));

static CONTROL_SEQ_RE: Lazy<Regex> = Lazy::new(|| {
    let toks = CONTROL_TOKENS
        .iter()
        .map(|t| regex::escape(t))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&toks).expect("control seq regex")
});

pub fn nt_token(nt_id: usize) -> String {
    format!("<<MT_NT:{nt_id:0NT_ID_WIDTH$}>>")
}

pub fn seg_start(seg_id: usize) -> String {
    format!("<<MT_SEG:{seg_id:0SEG_ID_WIDTH$}>>")
}

pub fn seg_end(seg_id: usize) -> String {
    format!("<<MT_END:{seg_id:0SEG_ID_WIDTH$}>>")
}

pub static ANY_SENTINEL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"<<MT_(?:TAB|BR|NBH|SHY|NT:\d{4}|SEG:\d{6}|END:\d{6}|SLOT:\d{6})>>")
        .expect("sentinel regex")
});

// Any token that looks like our internal markers. This is broader than ANY_SENTINEL_RE and is used
// to detect/strip hallucinated <<MT_...>> tokens that should never appear unless present in source.
pub static ANY_MT_TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<<MT_[A-Za-z0-9_:\-]{1,64}>>").expect("mt token regex"));

pub static NT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<<MT_NT:(\d{4})>>").expect("nt regex"));

pub fn slot_token(slot_id: usize) -> String {
    format!("<<MT_SLOT:{slot_id:0SLOT_ID_WIDTH$}>>")
}

pub fn control_tokens_from_text(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    CONTROL_TOKEN_RE
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

#[inline]
pub fn is_control_token(s: &str) -> bool {
    CONTROL_TOKENS.iter().any(|t| *t == s)
}

pub fn split_by_control_sequence(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut pos = 0usize;
    for m in CONTROL_SEQ_RE.find_iter(text) {
        parts.push(text[pos..m.start()].to_string());
        parts.push(m.as_str().to_string());
        pos = m.end();
    }
    parts.push(text[pos..].to_string());
    parts
}

pub fn parse_segmented_output(
    text: &str,
    expected_ids: &[usize],
) -> anyhow::Result<HashMap<usize, String>> {
    let mut segments: HashMap<usize, String> = HashMap::new();
    let mut cursor = 0usize;
    for &seg_id in expected_ids {
        let start_marker = seg_start(seg_id);
        let end_marker = seg_end(seg_id);

        let start_idx = text[cursor..]
            .find(&start_marker)
            .map(|i| cursor + i)
            .with_context(|| format!("missing SEG start for id={seg_id}"))?;
        let start_end = start_idx + start_marker.len();

        let end_idx = text[start_end..]
            .find(&end_marker)
            .map(|i| start_end + i)
            .with_context(|| format!("missing SEG end for id={seg_id}"))?;

        segments.insert(seg_id, text[start_end..end_idx].to_string());
        cursor = end_idx + end_marker.len();
    }
    Ok(segments)
}

pub fn parse_slot_output(
    text: &str,
    expected_ids: &[usize],
) -> anyhow::Result<HashMap<usize, String>> {
    let mut segments: HashMap<usize, String> = HashMap::new();
    if expected_ids.is_empty() {
        return Ok(segments);
    }

    let first_marker = slot_token(expected_ids[0]);
    let first_idx = text
        .find(&first_marker)
        .with_context(|| format!("missing SLOT for id={}", expected_ids[0]))?;
    if !text[..first_idx].trim().is_empty() {
        return Err(anyhow!("unexpected_prefix_before_first_slot"));
    }

    let mut cursor = 0usize;
    for (i, &slot_id) in expected_ids.iter().enumerate() {
        let marker = slot_token(slot_id);
        let start_idx = text[cursor..]
            .find(&marker)
            .map(|j| cursor + j)
            .with_context(|| format!("missing SLOT for id={slot_id}"))?;
        let start_end = start_idx + marker.len();

        let end_idx = if i + 1 < expected_ids.len() {
            let next_marker = slot_token(expected_ids[i + 1]);
            text[start_end..]
                .find(&next_marker)
                .map(|j| start_end + j)
                .with_context(|| format!("missing SLOT for id={}", expected_ids[i + 1]))?
        } else {
            text.len()
        };

        segments.insert(slot_id, text[start_end..end_idx].to_string());
        cursor = end_idx;
    }

    if cursor < text.len() && !text[cursor..].trim().is_empty() {
        return Err(anyhow!("unexpected_suffix_after_last_slot"));
    }
    Ok(segments)
}
