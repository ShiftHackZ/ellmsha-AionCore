use serde::{Deserialize, Serialize};

/// Which active guidance collection a rule belongs to.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRuleScope {
    Global,
    Project,
}

/// Human-editable fields projected from one complete, verified memory source.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRule {
    pub path: String,
    pub scope: ChatRuleScope,
    pub title: String,
    pub content: String,
    pub revision: String,
    pub editable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRulesResponse {
    pub rules: Vec<ChatRule>,
    pub project_available: bool,
    pub over_budget: bool,
    /// The connected memory gateway supports conditional edits.
    pub writable: bool,
    /// False for explicitly isolated sessions which bypass memory entirely.
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
pub enum ChatRuleMutation {
    Add {
        scope: ChatRuleScope,
        title: String,
        content: String,
    },
    Edit {
        scope: ChatRuleScope,
        path: String,
        revision: String,
        title: String,
        content: String,
    },
    Delete {
        scope: ChatRuleScope,
        path: String,
        revision: String,
    },
}

/// A structured, editable section from a recognized repository instruction file.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectFileRule {
    pub id: String,
    pub title: String,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRuleFile {
    pub id: String,
    /// Display only. The editor never accepts a path from a client.
    pub path: String,
    pub root_label: String,
    pub revision: String,
    pub writable: bool,
    pub condition: Option<String>,
    pub rules: Vec<ProjectFileRule>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRuleFiles {
    pub project_id: Option<String>,
    pub files: Vec<ProjectRuleFile>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    tag = "action",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ProjectFileRuleMutation {
    Add {
        file_id: String,
        revision: String,
        title: String,
        content: String,
    },
    Edit {
        file_id: String,
        revision: String,
        rule_id: String,
        title: String,
        content: String,
    },
    Delete {
        file_id: String,
        revision: String,
        rule_id: String,
    },
}
