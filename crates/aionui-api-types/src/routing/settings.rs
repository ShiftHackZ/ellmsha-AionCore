//! Saved graph editor state is deliberately separate from a frozen runtime policy.
use super::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterClass {
    /// Stable lowercase kebab-case rule identity; independent of its display name.
    pub id: String,
    /// Human-readable class label (up to 80 bytes).
    pub name: String,
    /// Task matching description (up to 1024 bytes), treated as classifier data.
    pub description: String,
    /// Existing account/agent and model used only for matching children.
    pub target: RouterTarget,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterPreset {
    Cost,
    Speed,
    Quality,
    #[default]
    Balanced,
    LocalOnly,
}
impl RouterPreset {
    pub fn profile(self) -> RouterProfile {
        match self {
            Self::Cost => RouterProfile::Cost,
            Self::Speed => RouterProfile::Speed,
            Self::Quality => RouterProfile::Quality,
            Self::Balanced | Self::LocalOnly => RouterProfile::Balanced,
        }
    }
    pub fn for_config(config: &RouterConfig) -> Self {
        if config.local_only {
            return Self::LocalOnly;
        }
        match config.profile {
            RouterProfile::Cost => Self::Cost,
            RouterProfile::Speed => Self::Speed,
            RouterProfile::Quality => Self::Quality,
            RouterProfile::Balanced => Self::Balanced,
        }
    }
}

/// Every populated mode/preset slot owns a complete policy. Absent slots are
/// initialized by the editor once; switching never overlays another slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterSettings {
    /// Saved editor schema; currently exactly 2.
    pub schema_version: u8,
    /// Mode selected for the next new chat; existing snapshots are unaffected.
    pub active_mode: RouterMode,
    /// Last selected preset for each mode, including not-yet-configured modes.
    pub active_presets: BTreeMap<RouterMode, RouterPreset>,
    /// Independent complete policy per initialized mode/preset pair.
    pub configurations: BTreeMap<RouterMode, BTreeMap<RouterPreset, RouterConfig>>,
}
impl Default for RouterSettings {
    fn default() -> Self {
        Self::migrate(RouterConfig::default())
    }
}
impl RouterSettings {
    /// Migration is only for global preferences, never conversation snapshots.
    pub fn migrate(mut config: RouterConfig) -> Self {
        config.schema_version = 2;
        let preset = RouterPreset::for_config(&config);
        Self {
            schema_version: 2,
            active_mode: config.mode,
            active_presets: [RouterMode::Auto, RouterMode::SemiAuto, RouterMode::Manual]
                .into_iter()
                .map(|mode| {
                    (
                        mode,
                        if mode == config.mode {
                            preset
                        } else {
                            RouterPreset::Balanced
                        },
                    )
                })
                .collect(),
            configurations: BTreeMap::from([(config.mode, BTreeMap::from([(preset, config)]))]),
        }
    }
    pub fn from_value(value: serde_json::Value) -> Result<Self, String> {
        let settings: Self = if value.get("configurations").is_some() {
            serde_json::from_value(value).map_err(|e| e.to_string())?
        } else {
            let legacy: RouterConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
            legacy.validate().map_err(str::to_owned)?;
            Self::migrate(legacy)
        };
        settings.validate().map_err(str::to_owned)?;
        Ok(settings)
    }
    pub fn active(&self) -> Result<&RouterConfig, &'static str> {
        self.active_presets
            .get(&self.active_mode)
            .and_then(|preset| self.configurations.get(&self.active_mode)?.get(preset))
            .ok_or("Active router mode/preset has no saved configuration")
    }
    pub fn policies(&self) -> impl Iterator<Item = &RouterConfig> {
        self.configurations.values().flat_map(|p| p.values())
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 2 || self.active_presets.len() != 3 {
            return Err("Invalid router settings version or active preset map");
        }
        self.active()?.validate()?;
        for (mode, presets) in &self.configurations {
            for (preset, config) in presets {
                if config.schema_version != 2
                    || config.mode != *mode
                    || (*preset != RouterPreset::LocalOnly && config.profile != preset.profile())
                    || config.local_only != (*preset == RouterPreset::LocalOnly)
                {
                    return Err("Router mode/preset slot does not match its policy");
                }
                config.validate_shape()?;
            }
        }
        Ok(())
    }
}

/// A role's supported state is execution eligibility, never a claim of model quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterEligibility {
    /// Whether this role can currently execute through an implemented host adapter.
    pub supported: bool,
    /// Stable disabled/conflict reason code; absent when supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterCatalogEntry {
    /// Credential-free configured or saved account/agent/model reference.
    pub target: RouterTarget,
    /// Account/agent display name; model is available in target.model.
    pub label: String,
    /// Runtime adapter identifier, empty for a missing saved target.
    pub backend: String,
    /// Explicit local inference hint verified against a loopback API endpoint.
    pub local: bool,
    /// Eligibility for a host-managed root runtime.
    pub root: RouterEligibility,
    /// Eligibility for a host-managed child runtime.
    pub child: RouterEligibility,
    /// Eligibility for bounded tool-free classification.
    pub classifier: RouterEligibility,
}

/// Read-only POST /api/agents/router/catalog input. `config` is an optional
/// in-progress graph policy (no credentials); omitted selects saved active settings.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterCatalogRequest {
    /// Optional in-progress policy, including model hints and stale assignments.
    pub config: Option<RouterConfig>,
}
/// Catalog response inside ApiResponse.data. Every entry has role eligibility;
/// unavailable configured/saved references are retained with a stable reason code.
#[derive(Debug, Serialize)]
pub struct RouterCatalogResponse {
    /// All configured models plus missing saved assignments, with eligibility per role.
    pub entries: Vec<RouterCatalogEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inactive_mode_drafts_roundtrip_but_active_policy_requires_pins_and_classifier() {
        let mut settings = RouterSettings::default();
        let draft = RouterConfig {
            schema_version: 2,
            mode: RouterMode::Manual,
            classes: vec![RouterClass {
                id: "qa".into(),
                name: "QA".into(),
                description: "Tests".into(),
                target: target(),
            }],
            ..Default::default()
        };
        settings.configurations.insert(
            RouterMode::Manual,
            BTreeMap::from([(RouterPreset::Balanced, draft.clone())]),
        );
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(RouterSettings::from_value(value).unwrap(), settings);
        assert_eq!(
            settings.configurations[&RouterMode::Manual][&RouterPreset::Balanced],
            draft
        );
        settings.active_mode = RouterMode::Manual;
        assert_eq!(
            settings.validate(),
            Err("Semi-auto and manual routing require a root target")
        );
        let active = settings
            .configurations
            .get_mut(&RouterMode::Manual)
            .unwrap()
            .get_mut(&RouterPreset::Balanced)
            .unwrap();
        active.root_target = Some(target());
        active.child_target = Some(target());
        assert_eq!(
            settings.validate(),
            Err("Custom classes require an explicit classifier target")
        );
        settings.active_mode = RouterMode::Auto;
        settings
            .configurations
            .get_mut(&RouterMode::Manual)
            .unwrap()
            .get_mut(&RouterPreset::Balanced)
            .unwrap()
            .classes[0]
            .description = "x".repeat(1025);
        assert!(
            settings.validate().unwrap_err().contains("1024 bytes"),
            "inactive drafts still enforce structure"
        );
        assert!(
            RouterSettings::from_value(serde_json::to_value(draft).unwrap()).is_err(),
            "plain legacy writes stay strict"
        );
    }
    fn target() -> RouterTarget {
        RouterTarget::Provider {
            provider_id: "owned".into(),
            model: "model".into(),
        }
    }
    #[test]
    fn legacy_snapshot_roundtrip_is_unchanged_but_preferences_upgrade() {
        let value = serde_json::to_value(RouterConfig::default()).unwrap();
        assert!(value.get("schema_version").is_none());
        assert!(value.get("classes").is_none());
        let frozen: RouterConfig = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(frozen.schema_version, 1);
        let settings = RouterSettings::from_value(value).unwrap();
        assert_eq!(settings.active().unwrap().schema_version, 2);
        assert_eq!(frozen.schema_version, 1);
    }
    #[test]
    fn legacy_local_only_migration_preserves_optimization_and_hints() {
        let legacy = RouterConfig {
            local_only: true,
            profile: RouterProfile::Quality,
            hints: vec![RouterHint {
                target: Some(target()),
                quality_tier: Some(4),
                ..Default::default()
            }],
            allowed_targets: vec![target()],
            ..Default::default()
        };
        let settings = RouterSettings::from_value(serde_json::to_value(&legacy).unwrap()).unwrap();
        let mut expected = legacy;
        expected.schema_version = 2;
        assert_eq!(settings.active().unwrap(), &expected);
        assert_eq!(settings.active_presets[&RouterMode::Auto], RouterPreset::LocalOnly);
        assert_eq!(RouterPreset::LocalOnly.profile(), RouterProfile::Balanced);
    }
    #[test]
    fn mode_and_preset_roundtrip_preserves_independent_complete_policies() {
        let mut settings = RouterSettings::default();
        let mut custom = settings.active().unwrap().clone();
        custom.classifier_target = Some(target());
        custom.classes.push(RouterClass {
            id: "code".into(),
            name: "Code".into(),
            description: "Write code".into(),
            target: target(),
        });
        settings
            .configurations
            .get_mut(&RouterMode::Auto)
            .unwrap()
            .insert(RouterPreset::Balanced, custom.clone());
        custom.mode = RouterMode::Manual;
        custom.root_target = Some(target());
        custom.child_target = Some(target());
        custom.profile = RouterProfile::Cost;
        settings
            .configurations
            .insert(RouterMode::Manual, BTreeMap::from([(RouterPreset::Cost, custom)]));
        settings.active_presets.insert(RouterMode::Manual, RouterPreset::Cost);
        settings.active_mode = RouterMode::Manual;
        settings
            .configurations
            .get_mut(&RouterMode::Manual)
            .unwrap()
            .get_mut(&RouterPreset::Cost)
            .unwrap()
            .classes[0]
            .description = "Changed independently".into();
        let mut restored = RouterSettings::from_value(serde_json::to_value(settings).unwrap()).unwrap();
        assert_eq!(
            restored.active().unwrap().classes[0].description,
            "Changed independently"
        );
        restored.active_mode = RouterMode::Auto;
        assert_eq!(restored.active().unwrap().classes[0].description, "Write code");
    }
    #[test]
    fn rejects_missing_slots_duplicate_classes_and_unbounded_descriptions() {
        let settings = RouterSettings {
            active_mode: RouterMode::Manual,
            ..Default::default()
        };
        assert_eq!(
            settings.validate(),
            Err("Active router mode/preset has no saved configuration")
        );
        let mut config = RouterConfig {
            schema_version: 2,
            classifier_target: Some(target()),
            ..Default::default()
        };
        let class = RouterClass {
            id: "qa".into(),
            name: "QA".into(),
            description: "Test".into(),
            target: target(),
        };
        config.classes = vec![class.clone(), class];
        assert!(config.validate().unwrap_err().contains("unique lowercase IDs"));
        config.classes.pop();
        config.classes[0].description = "x".repeat(1025);
        assert!(config.validate().unwrap_err().contains("1024 bytes"));
        config.classes[0].description = "Test".into();
        config.classifier_target = None;
        assert_eq!(
            config.validate(),
            Err("Custom classes require an explicit classifier target")
        );
    }
}
