//! Public response contract for GET /api/usage. All token counters are nonnegative.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Sum of reported full input, including cached input.
    pub input_tokens: u64,
    /// Sum of reported completions, including reasoning when the provider includes it.
    pub output_tokens: u64,
    /// Cache-hit subset of input; never add it to input again.
    pub cache_read_tokens: u64,
    /// Cache-creation subset of input; never add it to input again.
    pub cache_creation_tokens: u64,
    /// Number of unique usage measurements; CLI turns may aggregate multiple calls.
    pub requests: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSeries {
    /// UTC calendar day, YYYY-MM-DD.
    pub period: String,
    /// Configured provider ID; retained even after its settings are removed.
    pub provider: String,
    /// Current user-scoped display name, falling back to the provider ID.
    pub provider_name: String,
    /// Actual model reported by the executing engine, not the router label.
    pub model: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageContext {
    /// Authenticated provider or agent catalog identity.
    pub provider: String,
    /// Current account-scoped provider or agent display name.
    pub provider_name: String,
    /// Executed model, independent from the Auto Router label.
    pub model: String,
    /// Conversation owning this latest non-child request snapshot.
    pub conversation_id: String,
    /// Current human-readable title of the owning conversation.
    pub conversation_name: String,
    /// Latest reported input plus output, not a cumulative spend counter.
    pub context_tokens: u64,
    /// Configured context capacity; not an assertion of provider-advertised capacity.
    pub context_window: u64,
    /// Estimated composition, scaled to context_tokens: system, memory, skills,
    /// tools (definitions), messages (including tool results), unattributed.
    pub breakdown: std::collections::BTreeMap<String, u64>,
    /// Exact pre-response character counts and explicitly named local token estimates.
    pub request_metrics: std::collections::BTreeMap<String, u64>,
    /// UTC Unix milliseconds when this request was recorded.
    pub created_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageResponse {
    /// Inclusive UTC Unix millisecond boundary.
    pub start: i64,
    /// Exclusive UTC Unix millisecond boundary.
    pub end: i64,
    pub totals: UsageTotals,
    /// Disjoint daily/provider/model aggregates. These sum to totals.
    pub series: Vec<UsageSeries>,
    /// One latest snapshot per matching conversation; never sum into spend.
    pub contexts: Vec<UsageContext>,
}
