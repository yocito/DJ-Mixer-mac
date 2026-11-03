//! ミックスキュー（djmix/queues.py）。作業中のキューは名前 "__current__" で保存し、起動し直しても復元する。
use crate::library::query::get_tracks;
use crate::{db, util};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const CURRENT: &str = "__current__";

/// キューの 1 曲（画面とやりとりする形。track は読み出すときに付ける曲の情報）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct QueueItem {
    pub track_id: i64,
    #[serde(default)]
    pub use_start: Option<f64>,
    #[serde(default)]
    pub use_end: Option<f64>,
    #[serde(default)]
    pub use_label: Option<String>,
    #[serde(default)]
    pub join_next: Option<i64>,
    #[serde(default)]
    pub join_bars: Option<i64>,
    #[serde(default)]
    pub join_tpl: Option<String>,
    #[serde(default)]
    pub join_params: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Map<String, Value>>,
}

fn queue_id(c: &rusqlite::Connection, name: &str, create: bool) -> Result<Option<i64>> {
    if let Ok(id) = c.query_row("SELECT id FROM queues WHERE name = ?", [name], |r| r.get::<_, i64>(0)) {
        return Ok(Some(id));
    }
    if !create {
        return Ok(None);
    }
    c.execute("INSERT INTO queues(name, updated) VALUES(?, ?)", rusqlite::params![name, util::now()])?;
    Ok(Some(c.last_insert_rowid()))
}

pub fn load(name: &str) -> Result<Vec<QueueItem>> {
    let rows: Vec<QueueItem> = db::with(|c| {
        let Some(qid) = queue_id(c, name, false)? else { return Ok(vec![]) };
        let mut st = c.prepare(
            "SELECT track_id, use_start, use_end, use_label, join_next, join_bars, join_tpl, join_params FROM queue_items \
             WHERE queue_id = ? ORDER BY pos",
        )?;
        let v = st
            .query_map([qid], |r| {
                Ok(QueueItem {
                    track_id: r.get(0)?,
                    use_start: r.get(1)?,
                    use_end: r.get(2)?,
                    use_label: r.get(3)?,
                    join_next: r.get(4)?,
                    join_bars: r.get(5)?,
                    join_tpl: r.get(6)?,
                    join_params: r.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str(&s).ok()),
                    track: None,
                })
            })?
            .flatten()
            .collect();
        Ok(v)
    })?;
    let ids: Vec<i64> = rows.iter().map(|r| r.track_id).collect();
    let tracks = get_tracks(&ids)?;
    Ok(rows
        .into_iter()
        .filter_map(|mut it| {
            let t = tracks.get(&it.track_id)?.clone();
            it.track = Some(t);
            Some(it)
        })
        .collect())
}

/// つなぎは「次の曲」とセット。並べ替えで次の曲が変わった（または最後の曲になった）つなぎは標準に戻す。
pub fn normalize_joins(items: &[QueueItem]) -> Vec<QueueItem> {
    let mut out = vec![];
    for (i, it) in items.iter().enumerate() {
        let mut it = it.clone();
        let nxt = items.get(i + 1).map(|x| x.track_id);
        if it.join_next != nxt || nxt.is_none() {
            it.join_next = nxt;
            it.join_bars = None;
            it.join_tpl = None;
            it.join_params = None;
        }
        out.push(it);
    }
    out
}

pub fn save(items: &[QueueItem], name: &str) -> Result<()> {
    let items = normalize_joins(items);
    db::tx(|c| {
        let qid = queue_id(c, name, true)?.unwrap();
        c.execute("DELETE FROM queue_items WHERE queue_id = ?", [qid])?;
        for (i, it) in items.iter().enumerate() {
            c.execute(
                "INSERT INTO queue_items(queue_id, pos, track_id, use_start, use_end, use_label, join_next, join_bars, join_tpl, join_params) \
                 VALUES(?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    qid,
                    i as i64,
                    it.track_id,
                    it.use_start,
                    it.use_end,
                    it.use_label,
                    it.join_next,
                    it.join_bars,
                    it.join_tpl,
                    it.join_params.as_ref().map(|p| serde_json::to_string(p).unwrap())
                ],
            )?;
        }
        c.execute("UPDATE queues SET updated = ? WHERE id = ?", rusqlite::params![util::now(), qid])?;
        Ok(())
    })
}

pub fn names() -> Result<Vec<Value>> {
    db::with(|c| {
        let mut st = c.prepare(
            "SELECT q.name, q.updated, COUNT(i.track_id) FROM queues q LEFT JOIN queue_items i ON i.queue_id = q.id \
             WHERE q.name != ? GROUP BY q.id ORDER BY q.updated DESC",
        )?;
        let v = st
            .query_map([CURRENT], |r| {
                Ok(serde_json::json!({"name": r.get::<_, String>(0)?, "updated": r.get::<_, f64>(1)?, "count": r.get::<_, i64>(2)?}))
            })?
            .flatten()
            .collect();
        Ok(v)
    })
}

/// JSON の値 → キューの項目（画面から送られた形。track は無視する）
pub fn items_from_json(v: &Value) -> Vec<QueueItem> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| serde_json::from_value::<QueueItem>(x.clone()).ok()).map(|mut x| {
            x.track = None;
            x
        }).collect())
        .unwrap_or_default()
}
