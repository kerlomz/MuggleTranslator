use anyhow::anyhow;

use crate::freezer::unfreeze_text;
use crate::ir::{FormatSpan, TextNodeRef};
use crate::sentinels::{
    control_tokens_from_text, is_control_token, split_by_control_sequence, ANY_SENTINEL_RE,
};

#[derive(Clone, Debug)]
pub struct SpanSlice {
    pub span: FormatSpan,
    pub text: String,
}

pub fn project_translation_to_spans(
    spans: &[FormatSpan],
    source_surface: &str,
    target_surface: &str,
    nt_map: &std::collections::HashMap<String, String>,
) -> anyhow::Result<Vec<SpanSlice>> {
    if control_tokens_from_text(source_surface) != control_tokens_from_text(target_surface) {
        return Err(anyhow!("control token sequence mismatch"));
    }

    let source_parts = split_by_control_sequence(source_surface);
    let target_parts = split_by_control_sequence(target_surface);
    if source_parts.len() != target_parts.len() {
        return Err(anyhow!("control token part count mismatch"));
    }

    let mut span_slices: Vec<SpanSlice> = Vec::new();
    let mut span_idx = 0usize;

    for (src_part, tgt_part) in source_parts.into_iter().zip(target_parts.into_iter()) {
        if is_control_token(&src_part) {
            if src_part != tgt_part {
                return Err(anyhow!("control token mismatch"));
            }
            continue;
        }

        let mut block_spans: Vec<FormatSpan> = Vec::new();
        let mut block_src_len = 0usize;
        while span_idx < spans.len() && block_src_len < src_part.len() {
            let sp = spans[span_idx].clone();
            block_src_len += sp.source_text.len();
            block_spans.push(sp);
            span_idx += 1;
        }

        if block_spans.is_empty() {
            if !tgt_part.trim().is_empty() {
                return Err(anyhow!("translated text exists for empty source block"));
            }
            continue;
        }

        let tgt_units = unitize(&tgt_part);
        let total_plain = count_plain_units(&tgt_units);
        let weights: Vec<usize> = block_spans
            .iter()
            .map(|s| s.source_text.len().max(1))
            .collect();
        let desired = allocate_plain_counts(total_plain, &weights);

        let mut slices_units: Vec<Vec<String>> = vec![Vec::new(); block_spans.len()];
        let mut current_span = 0usize;
        let mut current_plain = 0usize;

        for unit in tgt_units {
            slices_units[current_span].push(unit.clone());
            if !is_sentinel(&unit) {
                current_plain += 1;
            }
            if current_span + 1 < block_spans.len() && current_plain >= desired[current_span] {
                current_span += 1;
                current_plain = 0;
            }
        }

        for (sp, units) in block_spans.into_iter().zip(slices_units.into_iter()) {
            let frozen = units.concat();
            let unfrozen = unfreeze_text(&frozen, nt_map);
            span_slices.push(SpanSlice {
                span: sp,
                text: unfrozen,
            });
        }
    }

    if span_idx != spans.len() {
        let remaining = &spans[span_idx..];
        if remaining.iter().any(|s| !s.source_text.trim().is_empty()) {
            return Err(anyhow!("span coverage mismatch"));
        }
        for sp in remaining {
            span_slices.push(SpanSlice {
                span: sp.clone(),
                text: String::new(),
            });
        }
    }

    Ok(span_slices)
}

pub fn distribute_span_text_to_nodes(span: &FormatSpan, text: &str) -> Vec<(TextNodeRef, String)> {
    if span.node_refs.is_empty() {
        return vec![];
    }
    if span.node_refs.len() == 1 {
        return vec![(span.node_refs[0].clone(), text.to_string())];
    }
    let weights: Vec<usize> = span
        .node_refs
        .iter()
        .map(|n| n.original_text.len().max(1))
        .collect();
    let units: Vec<char> = text.chars().collect();
    let desired = allocate_plain_counts(units.len(), &weights);
    let mut out: Vec<(TextNodeRef, String)> = Vec::new();

    let mut boundaries: Vec<usize> = Vec::new();
    let mut pos = 0usize;
    for count in desired.iter().take(span.node_refs.len().saturating_sub(1)) {
        pos = (pos + *count).min(units.len());
        boundaries.push(pos);
    }

    // Boundary smoothing to avoid splitting ASCII words/numbers across XML nodes.
    // This is best-effort and keeps monotonic boundaries.
    fn is_ascii_word_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric()
    }
    fn boundary_quality(units: &[char], pos: usize) -> (bool, bool) {
        if pos == 0 || pos >= units.len() {
            return (true, true);
        }
        let left = units[pos - 1];
        let right = units[pos];
        let ok = !(is_ascii_word_char(left) && is_ascii_word_char(right));
        let whitespace = left.is_whitespace() || right.is_whitespace();
        (ok, whitespace)
    }

    let window = 12usize;
    let mut prev_b = 0usize;
    for b in &mut boundaries {
        let orig = *b;
        let start = orig.saturating_sub(window);
        let end = (orig + window).min(units.len());

        let mut best = orig;
        let mut best_dist = usize::MAX;
        let mut best_ws = false;
        for cand in start..=end {
            if cand < prev_b || cand > units.len() {
                continue;
            }
            let (ok, ws) = boundary_quality(&units, cand);
            if !ok {
                continue;
            }
            let dist = orig.abs_diff(cand);
            if dist < best_dist || (dist == best_dist && ws && !best_ws) {
                best = cand;
                best_dist = dist;
                best_ws = ws;
            }
        }
        *b = best;
        prev_b = best;
    }

    let mut start_idx = 0usize;
    for (i, nr) in span.node_refs.iter().cloned().enumerate() {
        let end_idx = if i < boundaries.len() {
            boundaries[i].min(units.len())
        } else {
            units.len()
        };
        let piece: String = units[start_idx..end_idx].iter().collect();
        out.push((nr, piece));
        start_idx = end_idx;
    }
    out
}

fn is_sentinel(unit: &str) -> bool {
    ANY_SENTINEL_RE.is_match(unit)
}

fn unitize(text: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();
    let mut pos = 0usize;
    for m in ANY_SENTINEL_RE.find_iter(text) {
        let seg = &text[pos..m.start()];
        units.extend(seg.chars().map(|c| c.to_string()));
        units.push(m.as_str().to_string());
        pos = m.end();
    }
    let tail = &text[pos..];
    units.extend(tail.chars().map(|c| c.to_string()));
    units
}

fn count_plain_units(units: &[String]) -> usize {
    units.iter().filter(|u| !is_sentinel(u)).count()
}

fn allocate_plain_counts(total_plain: usize, weights: &[usize]) -> Vec<usize> {
    if weights.is_empty() {
        return vec![];
    }
    if total_plain == 0 {
        return vec![0; weights.len()];
    }
    let total_w: usize = weights.iter().sum();
    if total_w == 0 {
        let base = total_plain / weights.len();
        let mut out = vec![base; weights.len()];
        let used: usize = out.iter().sum();
        if let Some(last) = out.last_mut() {
            *last += total_plain - used;
        }
        return out;
    }

    let raw: Vec<f64> = weights
        .iter()
        .map(|w| (total_plain as f64) * (*w as f64) / (total_w as f64))
        .collect();
    let mut floored: Vec<usize> = raw.iter().map(|x| x.floor() as usize).collect();
    let mut remain = total_plain.saturating_sub(floored.iter().sum());

    let mut frac: Vec<(f64, usize)> = raw
        .iter()
        .enumerate()
        .map(|(i, x)| (x - (floored[i] as f64), i))
        .collect();
    frac.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let len = frac.len().max(1);
    let mut k = 0usize;
    while remain > 0 {
        let idx = frac[k % len].1;
        floored[idx] += 1;
        remain -= 1;
        k += 1;
    }
    floored
}
