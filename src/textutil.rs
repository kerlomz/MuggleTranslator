use once_cell::sync::Lazy;
use regex::Regex;

use crate::sentinels::ANY_SENTINEL_RE;

static CJK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\u4e00-\u9fff]").expect("cjk"));
static LATIN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Za-z]").expect("latin"));
static LETTER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\p{L}").expect("letter"));

pub fn strip_sentinels(text: &str) -> String {
    ANY_SENTINEL_RE.replace_all(text, " ").into_owned()
}

pub fn is_trivial_sentinel_text(text: &str) -> bool {
    let plain = strip_sentinels(text);
    let plain = plain.trim();
    if plain.is_empty() {
        return true;
    }
    !LETTER_RE.is_match(plain)
}

pub fn auto_language_pair(excerpts: &[String]) -> (String, String) {
    let mut cjk = 0usize;
    let mut latin = 0usize;
    for ex in excerpts {
        let plain = strip_sentinels(ex);
        cjk += CJK_RE.find_iter(&plain).count();
        latin += LATIN_RE.find_iter(&plain).count();
    }
    if latin >= cjk.saturating_mul(2).max(12) {
        ("en".to_string(), "zh".to_string())
    } else if cjk >= latin.saturating_mul(2).max(12) {
        ("zh".to_string(), "en".to_string())
    } else {
        ("en".to_string(), "zh".to_string())
    }
}
