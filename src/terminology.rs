use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct TermDecision {
    pub src: String,
    pub tgt: String,
    pub kind: Option<String>,
    pub note: Option<String>,
    pub seen: usize,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TermUpdate {
    pub src: String,
    pub tgt: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug)]
pub enum TermApplyEvent {
    Added {
        src: String,
        tgt: String,
    },
    Conflict {
        src: String,
        existing_tgt: String,
        proposed_tgt: String,
    },
}

#[derive(Default)]
pub struct TermMemory {
    terms: HashMap<String, TermDecision>,
}

impl TermMemory {
    #[must_use]
    pub fn new() -> Self {
        Self {
            terms: HashMap::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn apply_updates(
        &mut self,
        updates: impl IntoIterator<Item = TermUpdate>,
    ) -> Vec<TermApplyEvent> {
        let mut events = Vec::new();
        for up in updates {
            let src = up.src.trim();
            let tgt = up.tgt.trim();
            if src.is_empty() || tgt.is_empty() {
                continue;
            }

            match self.terms.get_mut(src) {
                None => {
                    self.terms.insert(
                        src.to_string(),
                        TermDecision {
                            src: src.to_string(),
                            tgt: tgt.to_string(),
                            kind: up
                                .kind
                                .as_ref()
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty()),
                            note: up
                                .note
                                .as_ref()
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty()),
                            seen: 1,
                        },
                    );
                    events.push(TermApplyEvent::Added {
                        src: src.to_string(),
                        tgt: tgt.to_string(),
                    });
                }
                Some(existing) => {
                    existing.seen = existing.seen.saturating_add(1);
                    if existing.tgt != tgt {
                        events.push(TermApplyEvent::Conflict {
                            src: src.to_string(),
                            existing_tgt: existing.tgt.clone(),
                            proposed_tgt: tgt.to_string(),
                        });
                    }
                }
            }
        }
        events
    }

    #[must_use]
    pub fn relevant_for_text<'a>(&'a self, text: &str, max_items: usize) -> Vec<&'a TermDecision> {
        if self.terms.is_empty() || text.is_empty() || max_items == 0 {
            return Vec::new();
        }
        let mut items: Vec<&TermDecision> = self
            .terms
            .values()
            .filter(|t| text.contains(&t.src))
            .collect();
        items.sort_by(|a, b| {
            b.src
                .len()
                .cmp(&a.src.len())
                .then_with(|| b.seen.cmp(&a.seen))
        });
        items.truncate(max_items);
        items
    }

    #[must_use]
    pub fn render_for_prompt(terms: &[&TermDecision]) -> String {
        if terms.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str("GLOSSARY (when relevant, follow these translations consistently):\n");
        for t in terms {
            out.push_str("- ");
            out.push_str(&t.src);
            out.push_str(" => ");
            out.push_str(&t.tgt);
            if let Some(kind) = t.kind.as_deref() {
                if !kind.trim().is_empty() {
                    out.push_str(" [");
                    out.push_str(kind.trim());
                    out.push(']');
                }
            }
            if let Some(note) = t.note.as_deref() {
                let note = note.trim();
                if !note.is_empty() {
                    out.push_str(" (");
                    out.push_str(note);
                    out.push(')');
                }
            }
            out.push('\n');
        }
        out
    }
}
