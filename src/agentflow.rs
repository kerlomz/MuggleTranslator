use anyhow::{anyhow, Context};
use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::agent::{ActPatch, PatchType};
use crate::sentinels::ANY_SENTINEL_RE;

static WS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").expect("ws regex"));
static DIGIT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+").expect("digit regex"));
static ACRONYM_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b[A-Z]{2,10}\b").expect("acronym regex"));

#[derive(Clone, Debug)]
pub struct SentenceSegment {
    pub text: String,
    pub suffix: String,
}

#[must_use]
pub fn split_sentences(text: &str) -> Vec<SentenceSegment> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<SentenceSegment> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        let ch = text[i..].chars().next().expect("char");
        let ch_len = ch.len_utf8();

        let mut boundary_at: Option<usize> = None;
        let mut include_char = false;

        if ch == '\n' {
            let mut sentence_end = i;
            if sentence_end > start && text.as_bytes()[sentence_end - 1] == b'\r' {
                sentence_end -= 1;
                boundary_at = Some(sentence_end);
            } else {
                boundary_at = Some(i);
            }
            include_char = false;
        } else if is_sentence_terminator(ch) {
            let boundary_end = i + ch_len;
            if is_terminator_boundary(text, boundary_end) {
                boundary_at = Some(boundary_end);
                include_char = true;
            }
        }

        if let Some(boundary) = boundary_at {
            let sentence_end = boundary;
            let suffix_start = boundary;
            let mut suffix_end = i + ch_len;
            if include_char {
                suffix_end = boundary;
            }

            // Capture \r?\n if the boundary was triggered by newline.
            if ch == '\n' {
                suffix_end = i + ch_len;
                if sentence_end < i {
                    // include the preceding '\r' as part of suffix
                    // (already excluded from sentence text)
                    suffix_end = i + ch_len;
                }
            }

            // Capture trailing whitespace after terminator/newline.
            while suffix_end < text.len() {
                let next = text[suffix_end..].chars().next().expect("char");
                if !next.is_whitespace() {
                    break;
                }
                suffix_end += next.len_utf8();
            }

            let sentence = text[start..sentence_end].to_string();
            let suffix = text[suffix_start..suffix_end].to_string();
            out.push(SentenceSegment {
                text: sentence,
                suffix,
            });
            start = suffix_end;
            i = start;
            continue;
        }

        i += ch_len;
    }

    if start < text.len() {
        out.push(SentenceSegment {
            text: text[start..].to_string(),
            suffix: String::new(),
        });
    }

    out
}

#[must_use]
pub fn render_sentence_list(segments: &[SentenceSegment], max_chars: usize) -> String {
    if segments.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for (idx, seg) in segments.iter().enumerate() {
        let line = format!("[{idx}] {}\n", seg.text);
        if max_chars > 0 && used + line.len() > max_chars {
            out.push_str("... (truncated)\n");
            break;
        }
        out.push_str(&line);
        used += line.len();
    }
    out
}

#[must_use]
pub fn join_sentences(segments: &[SentenceSegment]) -> String {
    let mut out = String::new();
    for seg in segments {
        out.push_str(&seg.text);
        out.push_str(&seg.suffix);
    }
    out
}

#[must_use]
pub fn normalize_for_match(text: &str) -> String {
    let t = text.trim();
    WS_RE.replace_all(t, " ").to_string()
}

#[must_use]
pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

#[must_use]
pub fn extract_must_preserve_tokens(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for m in ANY_SENTINEL_RE.find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    for m in DIGIT_RE.find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    for m in ACRONYM_RE.find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

pub fn apply_patches_to_draft(draft: &str, patches: &[ActPatch]) -> anyhow::Result<String> {
    if patches.is_empty() {
        return Ok(draft.to_string());
    }

    let mut segments = split_sentences(draft);
    if segments.is_empty() {
        return Err(anyhow!("cannot apply patches to empty draft"));
    }

    for patch in patches {
        let patch_type = PatchType::from_str(&patch.patch_type);
        match patch_type {
            PatchType::SentenceReplace | PatchType::SentenceMinimalEdit => {
                apply_sentence_patch(&mut segments, patch)?;
            }
            PatchType::TermMapUpdate => {
                // No direct text change. Handled by orchestrator via state_delta.
                continue;
            }
            PatchType::Unknown => {
                return Err(anyhow!("unknown_patch_type:{}", patch.patch_type));
            }
        }
    }

    Ok(join_sentences(&segments))
}

fn apply_sentence_patch(segments: &mut [SentenceSegment], patch: &ActPatch) -> anyhow::Result<()> {
    let idx = patch
        .location
        .sentence_index
        .context("patch missing sentence_index")?;
    if idx >= segments.len() {
        return Err(anyhow!(
            "patch sentence_index out of range: idx={} len={}",
            idx,
            segments.len()
        ));
    }

    let current = &segments[idx].text;
    let before = patch.before.sentence.trim();
    let after = patch.after.sentence.trim();

    if after.is_empty() {
        return Err(anyhow!("patch after.sentence is empty"));
    }
    if after.contains('\n') || after.contains('\r') {
        return Err(anyhow!("patch after.sentence contains newline"));
    }

    let cur_n = normalize_for_match(current);
    let before_n = normalize_for_match(before);
    if cur_n != before_n {
        return Err(anyhow!("patch before.sentence mismatch at idx={}", idx));
    }
    let after_n = normalize_for_match(after);
    if after_n == before_n {
        return Err(anyhow!("patch_no_change idx={}", idx));
    }

    if let Some(check) = patch.verification.apply_check.as_ref() {
        for s in &check.expect_before_contains {
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            if !current.contains(s) {
                return Err(anyhow!("patch expect_before_contains failed: {}", s));
            }
        }
        for s in &check.expect_after_contains {
            let s = s.trim();
            if s.is_empty() {
                continue;
            }
            if !after.contains(s) {
                return Err(anyhow!("patch expect_after_contains failed: {}", s));
            }
        }
    }

    if !patch.edit.minimal_from.trim().is_empty() {
        let mf = patch.edit.minimal_from.trim();
        if !before.contains(mf) {
            return Err(anyhow!("patch minimal_from not found in before"));
        }
    }
    if !patch.edit.minimal_to.trim().is_empty() {
        let mt = patch.edit.minimal_to.trim();
        if !after.contains(mt) {
            return Err(anyhow!("patch minimal_to not found in after"));
        }
    }

    for tok in &patch.verification.must_preserve_tokens {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }

        let c_before = count_token_occurrences(before, tok);
        if c_before == 0 {
            continue;
        }
        let c_after = count_token_occurrences(after, tok);
        if c_after != c_before {
            return Err(anyhow!(
                "patch must_preserve_tokens mismatch token={} before={} after={}",
                tok,
                c_before,
                c_after
            ));
        }
    }

    let hashes_before: Vec<String> = segments.iter().map(|s| sha256_hex(&s.text)).collect();
    segments[idx].text = after.to_string();
    let hashes_after: Vec<String> = segments.iter().map(|s| sha256_hex(&s.text)).collect();
    let changed: Vec<usize> = hashes_before
        .iter()
        .zip(hashes_after.iter())
        .enumerate()
        .filter_map(|(i, (a, b))| if a != b { Some(i) } else { None })
        .collect();
    if changed != vec![idx] {
        if changed.is_empty() {
            return Err(anyhow!("patch_no_change idx={}", idx));
        }
        return Err(anyhow!(
            "patch changed unexpected sentences: expected=[{}] got={:?}",
            idx,
            changed
        ));
    }

    Ok(())
}

fn count_token_occurrences(text: &str, token: &str) -> usize {
    if token.starts_with("<<MT_") {
        return text.matches(token).count();
    }
    if token.chars().all(|c| c.is_ascii_digit()) {
        return DIGIT_RE
            .find_iter(text)
            .filter(|m| m.as_str() == token)
            .count();
    }
    text.matches(token).count()
}

fn is_sentence_terminator(ch: char) -> bool {
    matches!(
        ch,
        '.'
            | '!'
            | '?'
            | ';'
            | '\u{3002}' // 。
            | '\u{FF01}' // ！
            | '\u{FF1F}' // ？
            | '\u{FF1B}' // ；
    )
}

fn is_terminator_boundary(text: &str, boundary_end: usize) -> bool {
    if boundary_end >= text.len() {
        return true;
    }
    let next = text[boundary_end..].chars().next().unwrap_or('\0');
    if next.is_whitespace() {
        return true;
    }
    false
}
