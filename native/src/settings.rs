//! アプリの設定（SQLite の settings 表。値は JSON。djmix/settings.py）。
use crate::db;
use anyhow::Result;
use serde_json::{json, Map, Value};

pub fn defaults() -> Map<String, Value> {
    let mr = crate::config::paths().music_root.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let v = json!({
        "output_device": "",
        "lufs_target": -14.0,
        "tempo_range": 0.08,
        "xfade_curve": "smooth",
        "queue_visible": true,
        "queue_autoload": true,
        "tempo_return": true,
        "prefer_rating": true,
        "skip_rating1": true,
        "avoid_recent_days": 14,
        "current_mix": null,
        "music_root": mr,
        "music_roots": [],
        "midi_mapping": "default",
        "itunes_xml": "",
        "itunes_enabled": true,
        "itunes_use_rating": true,
        "itunes_status": {},
        "prefer_plays": true,
        "auto_parts": true,
        "lib_view": {},
        "auto_cond": {},
        "export_pref": {},
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

pub fn load() -> Map<String, Value> {
    let mut out = defaults();
    let _ = db::with(|c| {
        let mut st = c.prepare("SELECT key, value FROM settings")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        for (k, v) in rows.flatten() {
            if out.contains_key(&k) {
                if let Some(v) = v.and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                    out.insert(k, v);
                }
            }
        }
        Ok(())
    });
    out
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

const BOOLS: &[&str] = &[
    "queue_visible", "queue_autoload", "tempo_return", "prefer_rating", "skip_rating1", "itunes_enabled",
    "itunes_use_rating", "prefer_plays", "auto_parts",
];

/// 送られたキーだけ変える（知らないキー・不正な値は無視する）。保存後の設定全体を返す。
pub fn save(patch: &Map<String, Value>) -> Result<Map<String, Value>> {
    let mut cur = load();
    let defs = defaults();
    let mut changed: Vec<String> = vec![];
    for (k, v) in patch {
        if !defs.contains_key(k) {
            continue;
        }
        let mut v = v.clone();
        match k.as_str() {
            "lufs_target" => match as_f64(&v) {
                Some(x) => v = json!(x.clamp(-23.0, -8.0)),
                None => continue,
            },
            "tempo_range" => match as_f64(&v) {
                Some(x) if [0.08, 0.16, 0.5].contains(&x) => v = json!(x),
                _ => continue,
            },
            "xfade_curve" => {
                if !matches!(v.as_str(), Some("smooth" | "linear" | "cut")) {
                    continue;
                }
            }
            "music_roots" => {
                let list: Vec<Value> = v
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|x| match x {
                                Value::String(s) => s.trim().to_string(),
                                other => other.to_string().trim().to_string(),
                            })
                            .filter(|s| !s.is_empty())
                            .map(Value::String)
                            .collect()
                    })
                    .unwrap_or_default();
                v = Value::Array(list);
            }
            "lib_view" | "auto_cond" | "export_pref" | "itunes_status" => {
                if !v.is_object() {
                    continue;
                }
            }
            "avoid_recent_days" => {
                let x = as_f64(&v).unwrap_or(0.0) as i64;
                v = json!(x.clamp(0, 365));
            }
            _ => {}
        }
        if BOOLS.contains(&k.as_str()) {
            v = Value::Bool(truthy(&v));
        }
        cur.insert(k.clone(), v);
        changed.push(k.clone());
    }
    db::tx(|c| {
        for k in &changed {
            c.execute(
                "INSERT INTO settings(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![k, serde_json::to_string(&cur[k])?],
            )?;
        }
        Ok(())
    })?;
    Ok(cur)
}

pub fn save_one(key: &str, v: Value) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    m.insert(key.to_string(), v);
    save(&m)
}

pub fn get_bool(s: &Map<String, Value>, k: &str, default: bool) -> bool {
    s.get(k).map(truthy).unwrap_or(default)
}

pub fn get_f64(s: &Map<String, Value>, k: &str, default: f64) -> f64 {
    s.get(k).and_then(as_f64).unwrap_or(default)
}

pub fn get_str<'a>(s: &'a Map<String, Value>, k: &str) -> &'a str {
    s.get(k).and_then(|v| v.as_str()).unwrap_or("")
}
