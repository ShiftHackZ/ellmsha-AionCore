//! The chat-facing contract shared by native provider questions and confirmation recovery.
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    pub is_other: bool,
    pub is_secret: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub allow_empty: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

impl UserQuestion {
    /// Normalize the two native wire schemas at the adapter boundary. Claude
    /// identifies an answer by its exact question text, Codex by its opaque id.
    pub fn from_native(raw: &Value, claude: bool) -> Option<Vec<Self>> {
        let questions = raw.as_array().or_else(|| raw.get("questions")?.as_array())?;
        if questions.is_empty() || questions.len() > 12 {
            return None;
        }
        let questions = questions
            .iter()
            .map(|q| {
                let question = q.get("question")?.as_str()?.to_owned();
                let id = if claude {
                    question.clone()
                } else {
                    q.get("id")?.as_str()?.to_owned()
                };

                let options: Vec<QuestionOption> = match q.get("options") {
                    Some(Value::Null) | None => vec![],
                    Some(v) => serde_json::from_value(v.clone()).ok()?,
                };
                Some(Self {
                    id,
                    question,
                    header: q.get("header").and_then(Value::as_str).unwrap_or_default().into(),
                    multi_select: claude && q["multiSelect"].as_bool().unwrap_or(false),
                    is_other: claude || options.is_empty() || q["isOther"].as_bool().unwrap_or(false),
                    is_secret: q["isSecret"].as_bool().unwrap_or(false),
                    optional: false,
                    allow_empty: false,
                    default_labels: vec![],
                    options,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Self::valid_form(&questions).then_some(questions)
    }

    pub fn valid_form(questions: &[Self]) -> bool {
        let mut ids = HashSet::new();
        !questions.is_empty()
            && questions.len() <= 12
            && questions.iter().all(|q| {
                let mut labels = HashSet::new();
                !q.id.trim().is_empty()
                    && q.id.len() <= 8192
                    && ids.insert(&q.id)
                    && !q.question.trim().is_empty()
                    && q.question.len() <= 8192
                    && q.header.len() <= 512
                    && (q.is_other || !q.options.is_empty())
                    && (!q.allow_empty || q.multi_select)
                    && (q.default_labels.is_empty() || !q.is_secret)
                    && q.default_labels.len() <= if q.multi_select { 32 } else { 1 }
                    && q.default_labels
                        .iter()
                        .all(|v| v.len() <= 16_384 && (q.is_other || q.options.iter().any(|o| &o.label == v)))
                    && q.default_labels.iter().collect::<HashSet<_>>().len() == q.default_labels.len()
                    && q.options.len() <= 32
                    && q.options.iter().all(|o| {
                        !o.label.trim().is_empty()
                            && o.label.len() <= 2048
                            && o.description.len() <= 4096
                            && labels.insert(&o.label)
                    })
            })
    }
}

/// Answer to one structured question, keyed by its original provider id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct QuestionAnswer {
    pub question: String,
    pub labels: Vec<String>,
}

impl QuestionAnswer {
    /// Check the whole answer set against the currently outstanding request. An
    /// approval option or partial answer must never silently answer another question.
    pub fn validate(questions: &[UserQuestion], answers: &[Self]) -> Result<(), &'static str> {
        let mut ids = HashSet::new();
        if answers
            .iter()
            .any(|a| !ids.insert(&a.question) || !questions.iter().any(|q| q.id == a.question))
        {
            return Err("Unknown or duplicate question answer");
        }
        for question in questions {
            let answer = answers.iter().find(|a| a.question == question.id);
            if question.optional && answer.is_none_or(|a| a.labels.is_empty()) {
                continue;
            }
            let answer = answer.ok_or("Missing question answer")?;
            let labels = &answer.labels;
            if question.multi_select && question.allow_empty && labels.is_empty() {
                continue;
            }
            if labels.is_empty()
                || labels.len() > 33
                || (!question.multi_select && labels.len() != 1)
                || labels.iter().any(|label| {
                    label.len() > 16_384
                        || label.trim().is_empty()
                        || (!question.is_other && !question.options.iter().any(|o| &o.label == label))
                })
                || labels.iter().collect::<std::collections::HashSet<_>>().len() != labels.len()
            {
                return Err("Invalid selection for this question");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_forms_validate_unique_questions_choices_and_secret_flags() {
        let choices = json!([{"label":"Yes","description":"Accept"}]);
        let raw =
            json!([{"id":"q","header":"Select","question":"Which?","isSecret":true,"isOther":false,"options":choices}]);
        let questions = UserQuestion::from_native(&raw, false).unwrap();
        assert!(questions[0].is_secret && !questions[0].is_other && !questions[0].optional);
        assert!(
            QuestionAnswer::validate(
                &questions,
                &[QuestionAnswer {
                    question: "q".into(),
                    labels: vec!["invented".into()]
                }]
            )
            .is_err()
        );
        let mut bad = raw.clone();
        bad[0]["options"] = json!([{"label":"Yes"},{"label":"Yes"}]);
        assert!(UserQuestion::from_native(&bad, false).is_none());
        let mut dup = raw.clone();
        dup.as_array_mut().unwrap().push(raw[0].clone());
        assert!(UserQuestion::from_native(&dup, false).is_none());
        let mut optional = questions.clone();
        optional[0].optional = true;
        assert!(QuestionAnswer::validate(&optional, &[]).is_ok());
        assert!(
            QuestionAnswer::validate(
                &optional,
                &[QuestionAnswer {
                    question: "q".into(),
                    labels: vec![]
                }]
            )
            .is_ok()
        );
    }
}
