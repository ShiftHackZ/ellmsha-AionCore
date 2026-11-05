//! MCP form elicitations have typed results. Keep their field IDs and enum values
//! separate from display titles; tool approvals and URL-mode requests use another path.
use aionui_common::{QuestionAnswer, QuestionOption, UserQuestion};
use serde_json::{Map, Number, Value};
use std::collections::HashSet;

const INVALID: &str = "Unsupported MCP form schema";
const ANSWER: &str = "Answer does not match the requested field";

fn choices(schema: &Value, array: bool) -> Result<Vec<(String, String)>, &'static str> {
    let item = if array { &schema["items"] } else { schema };
    if array
        && !item
            .as_object()
            .is_some_and(|o| o.keys().all(|k| matches!(k.as_str(), "type" | "enum" | "anyOf")))
    {
        return Err(INVALID);
    }
    let named = item.get(if array { "anyOf" } else { "oneOf" });
    if named.is_some() && item.get("enum").is_some() {
        return Err(INVALID);
    }
    let mut values = if let Some(list) = named {
        list.as_array()
            .ok_or(INVALID)?
            .iter()
            .map(|v| {
                if !v
                    .as_object()
                    .is_some_and(|o| o.keys().all(|k| matches!(k.as_str(), "const" | "title")))
                {
                    return Err(INVALID);
                }
                let value = v["const"].as_str().ok_or(INVALID)?;
                Ok((v["title"].as_str().unwrap_or(value).to_owned(), value.to_owned()))
            })
            .collect::<Result<Vec<_>, &'static str>>()?
    } else if let Some(list) = item.get("enum") {
        list.as_array()
            .ok_or(INVALID)?
            .iter()
            .map(|v| {
                let value = v.as_str().ok_or(INVALID)?.to_owned();
                Ok((value.clone(), value))
            })
            .collect::<Result<Vec<_>, &'static str>>()?
    } else {
        vec![]
    };
    if let Some(titles) = item.get("enumNames") {
        let titles = titles.as_array().filter(|t| t.len() == values.len()).ok_or(INVALID)?;
        if item.get("enum").is_none() || array {
            return Err(INVALID);
        }
        for ((label, _), title) in values.iter_mut().zip(titles) {
            *label = title.as_str().ok_or(INVALID)?.to_owned();
        }
    }
    if values.len() > 32
        || (array || named.is_some() || item.get("enum").is_some()) && values.is_empty()
        || values.iter().map(|(k, _)| k).collect::<HashSet<_>>().len() != values.len()
        || values.iter().map(|(_, v)| v).collect::<HashSet<_>>().len() != values.len()
    {
        return Err(INVALID);
    }
    Ok(values)
}

fn default_labels(field: &Value, opts: &[(String, String)]) -> Result<Vec<String>, &'static str> {
    let Some(default) = field.get("default") else {
        return Ok(vec![]);
    };
    let labels = if field["type"] == "array" {
        default
            .as_array()
            .ok_or(INVALID)?
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or(INVALID))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![match default {
            Value::String(s) => s.clone(),
            Value::Number(_) | Value::Bool(_) => default.to_string(),
            _ => return Err(INVALID),
        }]
    };
    if labels.len() > 32 || labels.iter().any(|s| s.len() > 16_384) {
        return Err(INVALID);
    }
    if opts.is_empty() {
        return Ok(labels);
    }
    labels
        .iter()
        .map(|v| {
            opts.iter()
                .find(|(_, value)| value == v)
                .map(|(label, _)| label.clone())
                .ok_or(INVALID)
        })
        .collect()
}

fn properties(input: &Value) -> Result<&Map<String, Value>, &'static str> {
    let schema = &input["requestedSchema"];
    if schema["type"] != "object" {
        return Err(INVALID);
    }
    schema["properties"].as_object().ok_or(INVALID)
}

pub(super) fn questions(input: &Value) -> Result<Option<Vec<UserQuestion>>, &'static str> {
    if let Some(mode) = input.get("mode") {
        match mode.as_str() {
            Some("url") => return Ok(None),
            Some("form") => {}
            _ => return Err(INVALID),
        }
    }
    let props = properties(input)?;
    if props.len() > 12 {
        return Err(INVALID);
    }
    let schema = &input["requestedSchema"];
    if !schema.as_object().is_some_and(|o| {
        o.keys().all(|k| {
            matches!(
                k.as_str(),
                "$schema" | "type" | "title" | "description" | "properties" | "required" | "additionalProperties"
            )
        })
    }) || schema.get("additionalProperties").is_some_and(|v| !v.is_boolean())
    {
        return Err(INVALID);
    }
    let required = schema
        .get("required")
        .map(|r| r.as_array().ok_or(INVALID))
        .transpose()?;
    if required.is_some_and(|r| r.iter().any(|k| !k.as_str().is_some_and(|id| props.contains_key(id)))) {
        return Err(INVALID);
    }
    if props.is_empty() {
        return Ok(None);
    }
    let server = input["serverName"].as_str().filter(|s| !s.is_empty()).ok_or(INVALID)?;
    let message = input["message"].as_str().unwrap_or_default();
    let mut form = Vec::new();
    for (id, field) in props {
        let t = field["type"].as_str().ok_or(INVALID)?;
        if !field.as_object().is_some_and(|obj| {
            obj.keys().all(|k| match k.as_str() {
                "type" | "title" | "description" | "default" => true,
                "enum" | "enumNames" | "oneOf" | "format" | "pattern" | "minLength" | "maxLength" => t == "string",
                "items" | "minItems" | "maxItems" => t == "array",
                "minimum" | "maximum" => matches!(t, "number" | "integer"),
                _ => false,
            })
        }) {
            return Err(INVALID);
        }
        if ["minLength", "maxLength", "minItems", "maxItems"]
            .iter()
            .any(|k| field.get(k).is_some_and(|n| n.as_u64().is_none()))
            || ["minimum", "maximum"]
                .iter()
                .any(|k| field.get(k).is_some_and(|n| !n.is_number()))
        {
            return Err(INVALID);
        }
        let array = t == "array";
        let mut opts = choices(field, array)?;
        match t {
            "boolean" => opts = vec![("true".into(), "true".into()), ("false".into(), "false".into())],
            "array" if field["items"].get("type").is_none_or(|v| v == "string") => {}
            "string" | "integer" | "number" => {}
            _ => return Err(INVALID),
        }
        if field
            .get("format")
            .is_some_and(|f| !matches!(f.as_str(), Some("email" | "uri" | "date" | "date-time")))
            || field
                .get("pattern")
                .is_some_and(|p| p.as_str().is_none_or(|p| regex::Regex::new(p).is_err()))
        {
            return Err(INVALID);
        }
        let title = field["title"].as_str().unwrap_or(id);
        let description = field["description"].as_str().filter(|s| !s.is_empty());
        form.push(UserQuestion {
            id: id.clone(),
            header: format!("{server} · {message}"),
            question: description.map_or_else(|| title.to_owned(), |d| format!("{title}\n{d}")),
            multi_select: array,
            is_other: opts.is_empty(),
            is_secret: false,
            allow_empty: array && field["minItems"].as_u64().unwrap_or(0) == 0,
            default_labels: default_labels(field, &opts)?,
            optional: !required.is_some_and(|r| r.iter().any(|key| key.as_str() == Some(id))),
            options: opts
                .into_iter()
                .map(|(label, _)| QuestionOption {
                    label,
                    description: String::new(),
                })
                .collect(),
        });
    }
    if !UserQuestion::valid_form(&form) {
        return Err(INVALID);
    }
    Ok(Some(form))
}

fn in_bounds(n: f64, field: &Value, low: &str, high: &str) -> bool {
    field.get(low).is_none_or(|v| v.as_f64().is_some_and(|v| n >= v))
        && field.get(high).is_none_or(|v| v.as_f64().is_some_and(|v| n <= v))
}

fn integer(n: &Number) -> Option<i128> {
    n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from))
}

fn number_in_range(n: &Number, field: &Value) -> bool {
    ["minimum", "maximum"].iter().all(|key| {
        let Some(bound) = field.get(key) else { return true };
        let Some(bound) = bound.as_number() else { return false };
        let ordering = if let (Some(n), Some(b)) = (integer(n), integer(bound)) {
            Some(n.cmp(&b))
        } else {
            // Mixed integers and fractions outside the exact f64 integer range cannot be compared safely.
            n.as_f64()
                .zip(bound.as_f64())
                .filter(|(n, b)| n.abs() < 9_007_199_254_740_992. && b.abs() < 9_007_199_254_740_992.)
                .and_then(|(n, b)| n.partial_cmp(&b))
        };
        ordering.is_some_and(|order| {
            if *key == "minimum" {
                !order.is_lt()
            } else {
                !order.is_gt()
            }
        })
    })
}

fn valid_string(value: &str, field: &Value) -> bool {
    in_bounds(value.chars().count() as f64, field, "minLength", "maxLength")
        && field["pattern"]
            .as_str()
            .is_none_or(|p| regex::Regex::new(p).is_ok_and(|r| r.is_match(value)))
        && match field["format"].as_str() {
            Some("date") => chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok(),
            Some("date-time") => chrono::DateTime::parse_from_rfc3339(value).is_ok(),
            Some("email") => value.split_once('@').is_some_and(|(local, host)| {
                !local.is_empty() && !host.is_empty() && !host.contains('@') && !value.chars().any(char::is_whitespace)
            }),
            Some("uri") => url::Url::parse(value).is_ok(),
            None => true,
            _ => false,
        }
}

pub(super) fn content(input: &Value, answers: &[QuestionAnswer]) -> Result<Value, &'static str> {
    let form = questions(input)?.ok_or(INVALID)?;
    QuestionAnswer::validate(&form, answers)?;
    let props = properties(input)?;
    let mut result = Map::new();
    for a in answers {
        let field = &props[&a.question];
        if a.labels.is_empty() {
            if form.iter().any(|q| q.id == a.question && !q.optional && q.allow_empty) {
                result.insert(a.question.clone(), Value::Array(vec![]));
            }
            continue;
        }
        let value = &a.labels[0];
        let ty = field["type"].as_str().ok_or(INVALID)?;
        let opts = choices(field, ty == "array")?;
        let enum_value = |label: &str| {
            opts.iter()
                .find(|(l, _)| l == label)
                .map(|(_, v)| Value::String(v.clone()))
                .ok_or(ANSWER)
        };
        let typed = match ty {
            "array" if in_bounds(a.labels.len() as f64, field, "minItems", "maxItems") => {
                Value::Array(a.labels.iter().map(|l| enum_value(l)).collect::<Result<Vec<_>, _>>()?)
            }
            "string" => {
                let wire = if opts.is_empty() {
                    Value::String(value.clone())
                } else {
                    enum_value(value)?
                };
                if !wire.as_str().is_some_and(|text| valid_string(text, field)) {
                    return Err(ANSWER);
                }
                wire
            }
            "boolean" => Value::Bool(value.parse().map_err(|_| ANSWER)?),
            "integer" | "number" => {
                let num = value.parse::<Number>().map_err(|_| ANSWER)?;
                if (ty == "integer" && !(num.is_i64() || num.is_u64())) || !number_in_range(&num, field) {
                    return Err(ANSWER);
                }
                Value::Number(num)
            }
            _ => return Err(ANSWER),
        };
        result.insert(a.question.clone(), typed);
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn empty_arrays_and_integer_bounds_keep_their_json_meaning() {
        let input = json!({"serverName":"srv","requestedSchema":{"type":"object","required":["flags","cap"],"properties":{
            "flags":{"type":"array","items":{"enum":["one"]}},"cap":{"type":"integer","maximum":9007199254740992_u64,"default":0},
            "code":{"type":"string","enum":["x"],"minLength":2}}}});
        let q = questions(&input).unwrap().unwrap();
        assert!(q.iter().any(|q| q.id == "flags" && q.allow_empty));
        assert!(q.iter().any(|q| q.id == "cap" && q.default_labels == ["0"]));
        let mut values = vec![
            QuestionAnswer {
                question: "flags".into(),
                labels: vec![],
            },
            QuestionAnswer {
                question: "cap".into(),
                labels: vec!["9007199254740992".into()],
            },
        ];
        let out = content(&input, &values).unwrap();
        assert_eq!(out["flags"], json!([]));
        assert_eq!(out["cap"], json!(9007199254740992_u64));
        values[1].labels = vec!["9007199254740993".into()];
        assert!(content(&input, &values).is_err());
        values[1].labels = vec!["0".into()];
        values.push(QuestionAnswer {
            question: "code".into(),
            labels: vec!["x".into()],
        });
        assert!(content(&input, &values).is_err());
    }

    #[test]
    fn unsupported_schema_constraints_are_never_silently_dropped() {
        for field in [
            json!({"type":"integer","enum":["7"]}),
            json!({"type":"array","items":{"enum":["a"],"minLength":3}}),
            json!({"type":"string","oneOf":[{"const":"a","minLength":3}]}),
        ] {
            let input = json!({"serverName":"fixture","requestedSchema":{"type":"object","properties":{"x":field}}});
            assert!(questions(&input).is_err());
        }
        let input =
            json!({"serverName":"fixture","requestedSchema":{"type":"object","properties":{},"not":{"type":"object"}}});
        assert!(questions(&input).is_err());
    }

    #[test]
    fn legacy_enum_titles_are_displayed_and_the_underlying_value_is_returned() {
        let input = json!({"serverName":"fixture","requestedSchema":{"type":"object","properties":{
            "format":{"type":"string","enum":["md","txt"],"enumNames":["Markdown","Plain text"],"default":"md"}}}});
        let form = questions(&input).unwrap().unwrap();
        assert_eq!(form[0].default_labels, ["Markdown"]);
        let answer = QuestionAnswer {
            question: "format".into(),
            labels: vec!["Plain text".into()],
        };
        assert_eq!(content(&input, &[answer]).unwrap(), json!({"format":"txt"}));
    }

    #[test]
    fn round_trips_typed_values_and_keeps_missing_optional_fields_out() {
        let input = json!({"serverName":"fixture","message":"Settings","requestedSchema":{"type":"object","required":["count","enabled","color","features"],"properties":{
            "count":{"title":"Count","type":"integer","minimum":0},"enabled":{"type":"boolean"},"note":{"type":"string"},
            "color":{"type":"string","oneOf":[{"const":"#f00","title":"Red"}]},"features":{"type":"array","minItems":1,"items":{"anyOf":[{"const":"a","title":"Alpha"}]}}
        }}});
        let form = questions(&input).unwrap().unwrap();
        assert!(form.iter().all(|q| q.header.starts_with("fixture")));
        let answers = [
            ("count", vec!["0"]),
            ("enabled", vec!["false"]),
            ("color", vec!["Red"]),
            ("features", vec!["Alpha"]),
            ("note", vec![]),
        ]
        .map(|(id, l)| QuestionAnswer {
            question: id.into(),
            labels: l.into_iter().map(str::to_owned).collect(),
        });
        assert_eq!(
            content(&input, &answers).unwrap(),
            json!({"count":0,"enabled":false,"color":"#f00","features":["a"]})
        );
        let mut invalid = answers.clone();
        invalid[0].labels = vec!["-1".into()];
        assert!(content(&input, &invalid).is_err());
        assert_eq!(questions(&json!({"mode":"url"})).unwrap(), None);
        assert!(
            questions(
                &json!({"serverName":"srv","requestedSchema":{"type":"object","properties":{"data":{"type":"object"}}}})
            )
            .is_err()
        );
    }
}
