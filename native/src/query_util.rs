//! JSON の値の読み取り（Python の int()・float()・bool() に近い寛容さで）
use serde_json::Value;

pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

pub fn ints(v: &Value) -> Vec<i64> {
    v.as_array().map(|a| a.iter().filter_map(|x| num(x).map(|f| f as i64)).collect()).unwrap_or_default()
}

pub fn floats(v: &Value) -> Vec<f64> {
    v.as_array().map(|a| a.iter().filter_map(num).collect()).unwrap_or_default()
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
