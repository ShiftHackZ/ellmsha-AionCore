use crate::AgentError;
use aionui_api_types::{RouterConfig, RouterHint, RouterMode, RouterProfile, RouterRequirements, RouterTarget};

#[derive(Clone)]
pub(super) struct Candidate {
    pub target: RouterTarget,
    pub hint: RouterHint,
    pub local: bool,
    pub backend: String,
    pub team_capable: bool,
    pub accepted: u32,
    pub reviewed: u32,
}

pub(super) fn choose<'a>(
    config: &RouterConfig,
    candidates: &'a [Candidate],
    requirements: &RouterRequirements,
    child: bool,
) -> Result<(&'a Candidate, Vec<String>), AgentError> {
    let fixed = if child {
        (config.mode == RouterMode::Manual)
            .then_some(config.child_target.as_ref())
            .flatten()
    } else {
        (config.mode != RouterMode::Auto)
            .then_some(config.root_target.as_ref())
            .flatten()
    };
    let mut eligible: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| {
            (!config.local_only || c.local)
                && (!requirements.vision || c.hint.supports_vision == Some(true))
                && c.hint.supports_tools != Some(false)
                && (requirements.minimum_context_tokens == 0
                    || c.hint
                        .context_tokens
                        .is_some_and(|n| n >= requirements.minimum_context_tokens))
                && (fixed.is_some() || config.allowed_targets.is_empty() || config.allowed_targets.contains(&c.target))
        })
        .collect();
    if let Some(fixed) = fixed {
        let selected = eligible.into_iter().find(|c| &c.target == fixed).ok_or_else(|| {
            AgentError::bad_request("Fixed router target is missing, disabled or incompatible with this task")
        })?;
        return Ok((selected, vec!["manual_target".into()]));
    }
    if eligible.is_empty() {
        return Err(AgentError::bad_request(
            "No eligible router targets; configure an available model and required capabilities",
        ));
    }
    let mut reasons = Vec::new();
    // Unknown values remain unknown. Compare only dimensions present for the whole pool;
    // otherwise incomplete metadata would make an unknown model look free or perfect.
    let all_cost = eligible.iter().all(|c| cost(&c.hint).is_some());
    let all_speed = eligible.iter().all(|c| c.hint.expected_latency_ms.is_some());
    let all_quality = eligible.iter().all(|c| c.hint.quality_tier.is_some());
    if !all_cost {
        reasons.push("cost_unknown".into());
    }
    if !all_speed {
        reasons.push("latency_unknown".into());
    }
    if !all_quality {
        reasons.push("quality_unknown".into());
    }
    reasons.push("user_priors_not_verified_accuracy".into());
    if eligible.iter().any(|c| c.reviewed > 0) {
        reasons.push("explicit_user_acceptance_history".into());
    }
    let max_cost = eligible
        .iter()
        .filter_map(|c| cost(&c.hint))
        .fold(0.0_f64, f64::max)
        .max(0.001);
    let max_speed = eligible
        .iter()
        .filter_map(|c| c.hint.expected_latency_ms)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let score = |c: &Candidate| {
        let quality = if all_quality {
            f64::from(c.hint.quality_tier.unwrap_or(1)) / 5.0
        } else {
            0.0
        };
        let strength = if c.hint.strengths.contains(&requirements.task) {
            0.15
        } else {
            0.0
        };
        let price = if all_cost {
            1.0 - cost(&c.hint).unwrap_or(max_cost) / max_cost
        } else {
            0.0
        };
        let speed = if all_speed {
            1.0 - c.hint.expected_latency_ms.unwrap_or(max_speed) / max_speed
        } else {
            0.0
        };
        let (q, p, s) = match config.profile {
            RouterProfile::Cost => (0.15, 0.8, 0.05),
            RouterProfile::Speed => (0.15, 0.05, 0.8),
            RouterProfile::Quality => (0.9, 0.05, 0.05),
            RouterProfile::Balanced => (0.5, 0.25, 0.25),
        };
        let observed = if c.reviewed > 0 {
            (f64::from(c.accepted) + 1.0) / (f64::from(c.reviewed) + 2.0) - 0.5
        } else {
            0.0
        };
        0.2 * observed + (q + if requirements.complex { 0.3 } else { 0.0 }) * quality + p * price + s * speed + strength
    };
    eligible.sort_by(|a, b| score(b).total_cmp(&score(a)).then_with(|| a.target.cmp(&b.target)));
    if !(all_cost || all_speed || all_quality) {
        reasons.push("conservative_stable_baseline".into());
    }
    Ok((eligible[0], reasons))
}
fn cost(h: &RouterHint) -> Option<f64> {
    Some(h.input_usd_per_million? + h.output_usd_per_million?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(id: &str) -> Candidate {
        Candidate {
            target: RouterTarget::Provider {
                provider_id: id.into(),
                model: "model".into(),
            },
            hint: RouterHint::default(),
            local: false,
            backend: "aionrs".into(),
            team_capable: true,
            accepted: 0,
            reviewed: 0,
        }
    }
    #[test]
    fn unknown_metrics_never_mean_free_or_perfect() {
        let mut known = candidate("a");
        known.hint.input_usd_per_million = Some(50.0);
        known.hint.output_usd_per_million = Some(50.0);
        let pool = vec![candidate("z"), known];
        let (winner, reasons) = choose(
            &RouterConfig {
                profile: RouterProfile::Cost,
                ..Default::default()
            },
            &pool,
            &Default::default(),
            false,
        )
        .unwrap();
        assert_eq!(winner.target.id(), "a");
        assert!(reasons.iter().any(|v| v == "cost_unknown"));
    }
    #[test]
    fn hard_constraints_apply_even_to_fixed_targets() {
        let target = candidate("cloud");
        let config = RouterConfig {
            mode: RouterMode::SemiAuto,
            root_target: Some(target.target.clone()),
            local_only: true,
            ..Default::default()
        };
        assert!(choose(&config, &[target], &Default::default(), false).is_err());
        assert!(
            choose(
                &RouterConfig::default(),
                &[candidate("no-vision")],
                &RouterRequirements {
                    vision: true,
                    ..Default::default()
                },
                false
            )
            .is_err()
        );
    }
    #[test]
    fn children_use_their_own_fixed_target_and_automatic_task_strength() {
        let root = candidate("root");
        let mut child = candidate("child");
        child.hint.strengths = vec![aionui_api_types::RouterTaskKind::Review];
        let pool = vec![root.clone(), child.clone()];
        let config = RouterConfig {
            mode: RouterMode::Manual,
            root_target: Some(root.target.clone()),
            child_target: Some(child.target.clone()),
            ..Default::default()
        };
        assert_eq!(
            choose(&config, &pool, &Default::default(), false).unwrap().0.target,
            root.target
        );
        assert_eq!(
            choose(&config, &pool, &Default::default(), true).unwrap().0.target,
            child.target
        );
        assert_eq!(
            choose(
                &RouterConfig::default(),
                &pool,
                &RouterRequirements {
                    task: aionui_api_types::RouterTaskKind::Review,
                    ..Default::default()
                },
                true
            )
            .unwrap()
            .0
            .target,
            child.target
        );
    }
    #[test]
    fn only_host_review_counts_affect_observed_quality() {
        let plain = candidate("a");
        let mut reviewed = candidate("z");
        reviewed.reviewed = 4;
        reviewed.accepted = 4;
        assert_eq!(
            choose(
                &RouterConfig::default(),
                &[plain.clone(), reviewed.clone()],
                &Default::default(),
                false
            )
            .unwrap()
            .0
            .target,
            reviewed.target
        );
        reviewed.reviewed = 0;
        reviewed.accepted = 0;
        assert_eq!(
            choose(
                &RouterConfig::default(),
                &[plain.clone(), reviewed],
                &Default::default(),
                false
            )
            .unwrap()
            .0
            .target,
            plain.target
        );
    }
}
