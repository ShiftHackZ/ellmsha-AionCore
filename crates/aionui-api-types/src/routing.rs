//! Credential-free Auto Router contracts. Runtime decisions are server-owned.
use serde::{Deserialize, Serialize};
mod children;
mod settings;
pub use children::*;
pub use settings::*;

pub const ROUTER_CONFIG_KEY: &str = "router.config";
pub const ROUTER_AGENT_ID: &str = "auto-router";

/// Existing account/model or installed agent/model reference; never credentials.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouterTarget {
    Provider { provider_id: String, model: String },
    Agent { agent_id: String, model: String },
}
impl RouterTarget {
    pub fn model(&self) -> &str {
        match self {
            Self::Provider { model, .. } | Self::Agent { model, .. } => model,
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Provider { provider_id, .. } => provider_id,
            Self::Agent { agent_id, .. } => agent_id,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterMode {
    #[default]
    Auto,
    SemiAuto,
    Manual,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterProfile {
    Cost,
    Speed,
    Quality,
    #[default]
    Balanced,
}
/// Optional user-supplied metadata. These are priors, not verified task outcomes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterHint {
    pub target: Option<RouterTarget>,
    pub input_usd_per_million: Option<f64>,
    pub output_usd_per_million: Option<f64>,
    pub expected_latency_ms: Option<f64>,
    /// User-assessed quality, 1..5. Never presented as measured accuracy.
    pub quality_tier: Option<u8>,
    #[serde(default)]
    pub strengths: Vec<RouterTaskKind>,
    pub context_tokens: Option<u32>,
    pub supports_tools: Option<bool>,
    pub supports_vision: Option<bool>,
    /// Explicit declaration of local inference, also requiring a loopback endpoint.
    #[serde(default)]
    pub local_inference: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    /// Missing in frozen pre-graph conversations. Never upgrade a stored snapshot.
    #[serde(default = "legacy_version", skip_serializing_if = "is_legacy_version")]
    pub schema_version: u8,
    #[serde(default)]
    pub mode: RouterMode,
    #[serde(default)]
    pub profile: RouterProfile,
    #[serde(default)]
    pub local_only: bool,
    pub root_target: Option<RouterTarget>,
    pub child_target: Option<RouterTarget>,
    pub classifier_target: Option<RouterTarget>,
    /// Empty selects all eligible configured targets; explicit entries restrict the pool.
    #[serde(default)]
    pub allowed_targets: Vec<RouterTarget>,
    #[serde(default)]
    pub hints: Vec<RouterHint>,
    /// Child-only classification rules. Empty bypasses classification entirely.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<RouterClass>,
}
fn legacy_version() -> u8 {
    1
}
fn is_legacy_version(value: &u8) -> bool {
    *value == 1
}
impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            mode: RouterMode::default(),
            profile: RouterProfile::default(),
            local_only: false,
            root_target: None,
            child_target: None,
            classifier_target: None,
            allowed_targets: vec![],
            hints: vec![],
            classes: vec![],
        }
    }
}
impl RouterConfig {
    /// All persisted references, including currently inactive graph roles.
    pub fn targets(&self) -> impl Iterator<Item = &RouterTarget> {
        self.allowed_targets
            .iter()
            .chain(self.root_target.iter())
            .chain(self.child_target.iter())
            .chain(self.classifier_target.iter())
            .chain(self.hints.iter().filter_map(|h| h.target.as_ref()))
            .chain(self.classes.iter().map(|c| &c.target))
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_shape()?;
        if self.mode != RouterMode::Auto && self.root_target.is_none() {
            return Err("Semi-auto and manual routing require a root target");
        }
        if self.mode == RouterMode::Manual && self.child_target.is_none() {
            return Err("Manual routing requires a child target");
        }
        if !self.classes.is_empty() && self.classifier_target.is_none() {
            return Err("Custom classes require an explicit classifier target");
        }
        Ok(())
    }
    /// Validate a saved draft's shape without requiring all execution roles yet.
    /// Execution and the active settings slot still require [`Self::validate`].
    pub fn validate_shape(&self) -> Result<(), &'static str> {
        if !matches!(self.schema_version, 1 | 2) || (self.schema_version == 1 && !self.classes.is_empty()) {
            return Err("Unsupported router policy version");
        }
        if self.allowed_targets.len() > 128 || self.hints.len() > 128 {
            return Err("Router pool exceeds 128 targets");
        }
        for target in self.targets() {
            if target.id().trim().is_empty()
                || target.id().len() > 256
                || target.model().trim().is_empty()
                || target.model().len() > 256
                || target.id() == ROUTER_AGENT_ID
            {
                return Err("Router target requires an existing non-router account/agent and model");
            }
        }
        if self.schema_version == 1 && matches!(self.classifier_target, Some(RouterTarget::Agent { .. })) {
            return Err("Task classification requires a configured provider model");
        }
        if self.classes.len() > 32 {
            return Err("Router supports at most 32 custom classes");
        }
        let mut ids = std::collections::BTreeSet::new();
        for class in &self.classes {
            if class.id.is_empty()
                || class.id.len() > 64
                || class.id.starts_with('-')
                || class.id.ends_with('-')
                || !class
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                || !ids.insert(&class.id)
                || class.name.trim().is_empty()
                || class.name.len() > 80
                || class.description.trim().is_empty()
                || class.description.len() > 1024
            {
                return Err(
                    "Custom classes require unique lowercase IDs, names up to 80 bytes and descriptions up to 1024 bytes",
                );
            }
        }
        for hint in &self.hints {
            if hint.target.is_none()
                || hint.quality_tier.is_some_and(|v| !(1..=5).contains(&v))
                || hint.context_tokens == Some(0)
                || hint.strengths.len() > 8
            {
                return Err("Invalid router model hint");
            }
            if [
                hint.input_usd_per_million,
                hint.output_usd_per_million,
                hint.expected_latency_ms,
            ]
            .into_iter()
            .flatten()
            .any(|n| !n.is_finite() || !(0.0..=1_000_000_000.0).contains(&n))
            {
                return Err("Router metrics must be finite nonnegative values");
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterTaskKind {
    Coding,
    Review,
    Research,
    Writing,
    Analysis,
    #[default]
    General,
}
/// Classifier describes task needs, never target IDs, prices or quality scores.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterRequirements {
    #[serde(default)]
    pub task: RouterTaskKind,
    #[serde(default)]
    pub complex: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub minimum_context_tokens: u32,
}
/// Actual server decision, persisted with the concrete conversation runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterDecision {
    pub id: String,
    pub target: RouterTarget,
    pub requirements: RouterRequirements,
    pub reasons: Vec<String>,
    pub evidence_count: usize,
    pub elapsed_ms: u64,
    pub managed_children: bool,
    /// Frozen canonical workspace for bounded historical acceptance.
    pub scope_workspace: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterSnapshot {
    pub config: RouterConfig,
    pub decision: RouterDecision,
}
/// Transient create input. The prompt is classified but not persisted or sent here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterCreateRequest {
    pub initial_prompt: String,
    pub config: Option<RouterConfig>,
    #[serde(default)]
    pub has_images: bool,
}

/// Explicit user review submitted after inspecting a completed routed task.
/// All provenance and target fields come from the stored conversation, not this input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterOutcomeRequest {
    pub accepted: bool,
    /// Existing completed assistant response reviewed by the user.
    pub message_id: String,
}
/// Receipt for an explicit acceptance observation, not an automatic correctness score.
#[derive(Debug, Serialize)]
pub struct RouterOutcomeResponse {
    pub decision_id: String,
    pub accepted: bool,
    pub source: String,
}
