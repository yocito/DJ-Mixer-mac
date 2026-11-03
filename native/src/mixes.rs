//! ミックス = 名前付きの保存したキュー（djmix/mixes.py）。作業中のキュー（__current__）は含めない。
use crate::queues::{self, QueueItem, CURRENT};
use crate::{db, prefs, util};
use anyhow::Result;
use serde_json::{json, Map, Value};

fn tpl_bars(t: &str) -> i64 {
    match t {
        "short" => 8,
        "echo" => 2,
        "cut" => 0,
        _ => 16,
    }
}

fn minutes(items: &[QueueItem]) -> f64 {
    let mut sec = 0.0;
    for (i, it) in items.iter().enumerate() {
        let t = it.track.clone().unwrap_or_default();
        let dur = t.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let t0 = it.use_start.unwrap_or(0.0);
        let t1 = it.use_end.unwrap_or(dur);
        sec += (t1 - t0).max(0.0);
        if i + 1 < items.len() {
            let bars = it.join_bars.unwrap_or_else(|| tpl_bars(it.join_tpl.as_deref().unwrap_or("long")));
            let bpm = t.get("bpm").and_then(|v| v.as_f64()).filter(|b| *b != 0.0).unwrap_or(120.0);
            sec -= bars as f64 * 4.0 * 60.0 / bpm;
        }
    }
    util::round(sec.max(0.0) / 60.0, 1)
}

struct Row {
    id: i64,
    name: String,
    kind: Option<String>,
    created: Option<f64>,
    updated: f64,
    plays: Option<i64>,
    last_played: Option<f64>,
    exports: Option<String>,
}

fn row_json(r: &Row) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("id".into(), json!(r.id));
    m.insert("name".into(), json!(r.name));
    m.insert("kind".into(), json!(r.kind.clone().filter(|k| !k.is_empty()).unwrap_or_else(|| "hand".into())));
    m.insert("created".into(), json!(r.created.filter(|x| *x != 0.0).unwrap_or(r.updated)));
    m.insert("updated".into(), json!(r.updated));
    m.insert("plays".into(), json!(r.plays.unwrap_or(0)));
    m.insert("last_played".into(), json!(r.last_played));
    m.insert(
        "exports".into(),
        r.exports.as_ref().and_then(|s| serde_json::from_str::<Value>(s).ok()).unwrap_or_else(|| json!([])),
    );
    m
}

fn read_rows(sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Row>> {
    db::with(|c| {
        let mut st = c.prepare(sql)?;
        let v = st
            .query_map(params, |r| {
                Ok(Row {
                    id: r.get("id")?,
                    name: r.get("name")?,
                    kind: r.get("kind")?,
                    created: r.get("created")?,
                    updated: r.get("updated")?,
                    plays: r.get("plays")?,
                    last_played: r.get("last_played")?,
                    exports: r.get("exports")?,
                })
            })?
            .flatten()
            .collect();
        Ok(v)
    })
}

pub fn unique_name(name: &str) -> Result<String> {
    let base = if name.trim().is_empty() { "ミックス".to_string() } else { name.trim().to_string() };
    let mut cand = base.clone();
    let mut n = 2;
    loop {
        let exists = db::with(|c| Ok(c.query_row("SELECT 1 FROM queues WHERE name = ?", [&cand], |_| Ok(())).is_ok()))?;
        if cand != CURRENT && !exists {
            return Ok(cand);
        }
        cand = format!("{base} ({n})");
        n += 1;
    }
}

pub fn list_mixes(sort: &str) -> Result<Vec<Value>> {
    let order = match sort {
        "new" => "COALESCE(created, updated) DESC",
        "name" => "name COLLATE NOCASE",
        "plays" => "plays DESC, updated DESC",
        _ => "updated DESC",
    };
    let rows = read_rows(&format!("SELECT * FROM queues WHERE name != ? ORDER BY {order}"), &[&CURRENT])?;
    let mut out = vec![];
    for r in rows {
        let mut m = row_json(&r);
        let items = queues::load(&r.name)?;
        m.insert("count".into(), json!(items.len()));
        m.insert("minutes".into(), json!(minutes(&items)));
        m.insert(
            "titles".into(),
            json!(items.iter().take(4).map(|it| it.track.as_ref().and_then(|t| t.get("title").cloned()).unwrap_or(Value::Null)).collect::<Vec<_>>()),
        );
        out.push(Value::Object(m));
    }
    Ok(out)
}

pub fn get(mix_id: i64) -> Result<Option<Value>> {
    let rows = read_rows("SELECT * FROM queues WHERE id = ? AND name != ?", &[&mix_id, &CURRENT])?;
    let Some(r) = rows.into_iter().next() else { return Ok(None) };
    let items = queues::load(&r.name)?;
    let mut m = row_json(&r);
    m.insert("count".into(), json!(items.len()));
    m.insert("minutes".into(), json!(minutes(&items)));
    m.insert("items".into(), serde_json::to_value(&items)?);
    Ok(Some(Value::Object(m)))
}

pub fn get_items(mix_id: i64) -> Result<Option<Vec<QueueItem>>> {
    let rows = read_rows("SELECT * FROM queues WHERE id = ? AND name != ?", &[&mix_id, &CURRENT])?;
    match rows.into_iter().next() {
        Some(r) => Ok(Some(queues::load(&r.name)?)),
        None => Ok(None),
    }
}

/// キューの中身を新しいミックスとして残す（同じ名前があれば (2) を付ける）。
pub fn save(items: &[QueueItem], name: &str, kind: &str) -> Result<Value> {
    let name = unique_name(name)?;
    queues::save(items, &name)?;
    let mid: i64 = db::tx(|c| {
        c.execute(
            "UPDATE queues SET kind = ?, created = ? WHERE name = ?",
            rusqlite::params![if kind == "auto" { "auto" } else { "hand" }, util::now(), name],
        )?;
        Ok(c.query_row("SELECT id FROM queues WHERE name = ?", [&name], |r| r.get(0))?)
    })?;
    Ok(get(mid)?.unwrap_or(Value::Null))
}

pub fn rename(mix_id: i64, name: &str) -> Result<Option<Value>> {
    let Some(m) = get(mix_id)? else { return Ok(None) };
    if !name.trim().is_empty() && name.trim() != m["name"].as_str().unwrap_or("") {
        let n = unique_name(name)?;
        db::tx(|c| {
            c.execute("UPDATE queues SET name = ? WHERE id = ?", rusqlite::params![n, mix_id])?;
            Ok(())
        })?;
    }
    get(mix_id)
}

pub fn duplicate(mix_id: i64) -> Result<Option<Value>> {
    let Some(m) = get(mix_id)? else { return Ok(None) };
    let items = get_items(mix_id)?.unwrap_or_default();
    Ok(Some(save(&items, &format!("{} のコピー", m["name"].as_str().unwrap_or("")), m["kind"].as_str().unwrap_or("hand"))?))
}

pub fn delete(mix_id: i64) -> Result<bool> {
    db::tx(|c| Ok(c.execute("DELETE FROM queues WHERE id = ? AND name != ?", rusqlite::params![mix_id, CURRENT])? > 0))
}

pub fn record_play(mix_id: i64) -> Result<()> {
    db::tx(|c| {
        c.execute("UPDATE queues SET plays = plays + 1, last_played = ? WHERE id = ?", rusqlite::params![util::now(), mix_id])?;
        Ok(())
    })
}

pub fn add_export(mix_id: i64, info: Map<String, Value>) -> Result<()> {
    let Some(m) = get(mix_id)? else { return Ok(()) };
    let mut ex: Vec<Value> = m["exports"].as_array().cloned().unwrap_or_default();
    let mut info = info;
    info.insert("at".into(), json!(util::now()));
    ex.push(Value::Object(info));
    let keep: Vec<Value> = ex[ex.len().saturating_sub(20)..].to_vec();
    db::tx(|c| {
        c.execute("UPDATE queues SET exports = ? WHERE id = ?", rusqlite::params![serde_json::to_string(&keep)?, mix_id])?;
        Ok(())
    })
}

type Sig = Vec<(i64, i64, Option<i64>, Option<String>, Option<i64>)>;

fn signature(items: &[QueueItem]) -> Sig {
    // round(x, 1) を 10 倍の整数で比べる
    items
        .iter()
        .map(|it| {
            (
                it.track_id,
                (it.use_start.unwrap_or(0.0) * 10.0).round() as i64,
                it.use_end.map(|e| (e * 10.0).round() as i64),
                it.join_tpl.clone(),
                it.join_bars,
            )
        })
        .collect()
}

/// 書き出しの後: キューが保存したミックスのままならそのミックスに記録し、変えていれば（保存していなければ）
/// キューの中身を書き出しの名前で新しいミックスとして残す。書き出した曲を「かけた」と記録する。
pub fn record_export(info: &Map<String, Value>, current_mix: Option<i64>) -> Result<Value> {
    let items = queues::load(CURRENT)?;
    let cur = match current_mix {
        Some(id) => get_items(id)?.map(|its| (id, its)),
        None => None,
    };
    let mid = match cur {
        Some((id, its)) if signature(&its) == signature(&items) => id,
        _ => {
            let stem = info
                .get("path")
                .and_then(|v| v.as_str())
                .and_then(|p| std::path::Path::new(p).file_stem().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "ミックス".into());
            save(&items, &stem, "hand")?["id"].as_i64().unwrap_or(0)
        }
    };
    let mut keep = Map::new();
    for k in ["path", "format", "seconds", "tracklist"] {
        keep.insert(k.into(), info.get(k).cloned().unwrap_or(Value::Null));
    }
    add_export(mid, keep)?;
    prefs::record_plays(&items.iter().map(|i| i.track_id).collect::<Vec<_>>(), "export", None)?;
    Ok(get(mid)?.unwrap_or(Value::Null))
}

/// キューが、そのミックスを開いたままの中身か。
pub fn is_current(mix_id: Option<i64>) -> Result<bool> {
    let Some(id) = mix_id else { return Ok(false) };
    let Some(its) = get_items(id)? else { return Ok(false) };
    Ok(signature(&its) == signature(&queues::load(CURRENT)?))
}
