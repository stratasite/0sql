use std::path::Path;

use serde_json::Value as Json;
use serde_yaml::Value as Yaml;

use crate::error::{Error, Result};

pub(crate) fn read_yaml(path: &Path) -> Result<Yaml> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    let v: Yaml = serde_yaml::from_str(&text).map_err(|e| Error::Yaml(path.display().to_string(), e))?;
    Ok(v)
}

pub(crate) fn as_map(v: &Yaml) -> Option<&serde_yaml::Mapping> {
    v.as_mapping()
}

pub(crate) fn get<'a>(m: &'a serde_yaml::Mapping, key: &str) -> Option<&'a Yaml> {
    m.get(key).filter(|v| !v.is_null())
}

pub(crate) fn text(v: &Yaml) -> Option<String> {
    match v {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Number(n) => Some(n.to_string()),
        Yaml::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub(crate) fn str_of(m: &serde_yaml::Mapping, key: &str) -> Option<String> {
    get(m, key).and_then(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub(crate) fn bool_of(m: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    match get(m, key)? {
        Yaml::Bool(b) => Some(*b),
        Yaml::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        Yaml::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

pub(crate) fn int_of(m: &serde_yaml::Mapping, key: &str) -> Option<i64> {
    match get(m, key)? {
        Yaml::Number(n) => n.as_i64(),
        Yaml::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn strs_of(m: &serde_yaml::Mapping, key: &str) -> Vec<String> {
    match get(m, key) {
        Some(Yaml::Sequence(items)) => items.iter().filter_map(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(v) => text(v).map(|s| vec![s.trim().to_string()]).unwrap_or_default(),
        None => vec![],
    }
}

pub(crate) fn yaml_to_json(v: &Yaml) -> Json {
    serde_json::to_value(v).unwrap_or(Json::Null)
}
