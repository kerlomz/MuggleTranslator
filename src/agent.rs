use serde::Deserialize;

use crate::terminology::TermUpdate;

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AgentReviewResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub score_fidelity: i32,
    #[serde(default)]
    pub score_fluency: i32,
    #[serde(default)]
    pub score_style: i32,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub rewrite: String,
    #[serde(default)]
    pub needs_retranslate: bool,
    #[serde(default)]
    pub retranslate_instructions: String,
    #[serde(default)]
    pub term_updates: Vec<TermUpdate>,
}

impl AgentReviewResponse {
    #[must_use]
    pub fn chosen_text<'a>(&'a self, draft: &'a str) -> &'a str {
        let r = self.rewrite.trim();
        if r.is_empty() {
            draft
        } else {
            r
        }
    }

    #[must_use]
    pub fn quality_ok(&self, min_fidelity: i32, min_fluency: i32, min_style: i32) -> bool {
        if !self.ok {
            return false;
        }
        self.score_fidelity >= min_fidelity
            && self.score_fluency >= min_fluency
            && self.score_style >= min_style
    }
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AgentPlanResponse {
    #[serde(default)]
    pub doc_profile: PlanDocProfile,
    #[serde(default)]
    pub dimensions: Vec<Dimension>,
    #[serde(default)]
    pub term_policy: TermPolicy,
    #[serde(default)]
    pub style_guide: StyleGuide,
    #[serde(default)]
    pub state_delta: PlanStateDelta,
    #[serde(default)]
    pub metrics: PlanMetrics,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PlanDocProfile {
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub doc_type: Option<String>,
    #[serde(default)]
    pub tone: Option<String>,
    #[serde(default)]
    pub target_locale: Option<String>,
    #[serde(default)]
    pub risk_level: Option<String>,
    #[serde(default)]
    pub target_style: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub entities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct TermPolicy {
    #[serde(default)]
    pub enforce: bool,
    #[serde(default)]
    pub allow_variants: bool,
    #[serde(default)]
    pub case_policy: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct StyleGuide {
    #[serde(default)]
    pub rules: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PlanStateDelta {
    #[serde(default)]
    pub term_candidates: Vec<TermUpdate>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PlanMetrics {
    #[serde(default)]
    pub confidence: f32,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AgentJudgeResponse {
    #[serde(default)]
    pub sufficient: bool,
    #[serde(default)]
    pub dimension_scores: Vec<DimensionScore>,
    #[serde(default)]
    pub issues: Vec<Issue>,
    #[serde(default)]
    pub missing_info: Vec<MissingInfo>,
    #[serde(default)]
    pub decision: JudgeDecision,
    #[serde(default)]
    pub state_delta: JudgeStateDelta,
    #[serde(default)]
    pub metrics: JudgeMetrics,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct DimensionScore {
    #[serde(default)]
    pub dimension: Dimension,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct Issue {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub dimension: Dimension,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub location: IssueLocation,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub fix_hint: String,
    #[serde(default)]
    pub must_fix: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct IssueLocation {
    #[serde(default)]
    pub tu_id: Option<String>,
    #[serde(default)]
    pub anchor: Option<String>,
    #[serde(default)]
    pub sentence_index: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct MissingInfo {
    #[serde(default)]
    pub dimension: Dimension,
    #[serde(default)]
    pub why_needed: String,
    #[serde(default)]
    pub needed_scope: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct JudgeDecision {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub preferred_backend: String,
    #[serde(default)]
    pub retranslate_instructions: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct JudgeStateDelta {
    #[serde(default)]
    pub term_updates: Vec<TermUpdate>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct JudgeMetrics {
    #[serde(default)]
    pub issue_count: usize,
    #[serde(default)]
    pub hard_fail: usize,
    #[serde(default)]
    pub changed_since_last: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AgentProbeResponse {
    #[serde(default)]
    pub missing_info: Vec<MissingInfo>,
    #[serde(default)]
    pub questions: Vec<ProbeQuestion>,
    #[serde(default)]
    pub state_delta: ProbeStateDelta,
    #[serde(default)]
    pub metrics: ProbeMetrics,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ProbeQuestion {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub expected_answer_form: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ProbeStateDelta {
    #[serde(default)]
    pub evidence_add: Vec<EvidenceItem>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct EvidenceItem {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub dimension: Dimension,
    #[serde(default)]
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ProbeMetrics {
    #[serde(default)]
    pub evidence_delta: i32,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct AgentActResponse {
    #[serde(default)]
    pub patches: Vec<ActPatch>,
    #[serde(default)]
    pub state_delta: ActStateDelta,
    #[serde(default)]
    pub metrics: ActMetrics,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ActStateDelta {
    #[serde(default)]
    pub term_map_add: Vec<TermUpdate>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ActMetrics {
    #[serde(default)]
    pub patch_count: usize,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ActPatch {
    #[serde(default)]
    pub patch_id: String,
    #[serde(default)]
    pub patch_type: String,
    #[serde(default)]
    pub location: PatchLocation,
    #[serde(default)]
    pub before: PatchBefore,
    #[serde(default)]
    pub edit: PatchEdit,
    #[serde(default)]
    pub after: PatchAfter,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub verification: PatchVerification,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PatchLocation {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub sentence_index: Option<usize>,
    #[serde(default)]
    pub anchors: PatchAnchors,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PatchAnchors {
    #[serde(default)]
    pub strong: String,
    #[serde(default)]
    pub weak: Vec<String>,
    #[serde(default)]
    pub occurrence: usize,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PatchBefore {
    #[serde(default)]
    pub sentence: String,
    #[serde(default)]
    pub context_prev: Option<String>,
    #[serde(default)]
    pub context_next: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PatchAfter {
    #[serde(default)]
    pub sentence: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PatchEdit {
    #[serde(default)]
    pub minimal_from: String,
    #[serde(default)]
    pub minimal_to: String,
    #[serde(default)]
    pub operation: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PatchVerification {
    #[serde(default)]
    pub must_preserve_tokens: Vec<String>,
    #[serde(default)]
    pub diff_summary: String,
    #[serde(default)]
    pub apply_check: Option<PatchApplyCheck>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PatchApplyCheck {
    #[serde(default)]
    pub expect_before_contains: Vec<String>,
    #[serde(default)]
    pub expect_after_contains: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Term,
    Coref,
    Coverage,
    Style,
    Factual,
    Format,
    Global,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Ok,
    Warn,
    Fail,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchType {
    SentenceReplace,
    SentenceMinimalEdit,
    TermMapUpdate,
    Unknown,
}

impl PatchType {
    #[must_use]
    pub fn from_str(s: &str) -> Self {
        match s.trim() {
            "sentence_replace" => Self::SentenceReplace,
            "sentence_minimal_edit" => Self::SentenceMinimalEdit,
            "term_map_update" => Self::TermMapUpdate,
            _ => Self::Unknown,
        }
    }
}
