//! ユーザー修正（user_edits）を解析結果に重ねる（djmix/edits.py）。
//! 解析結果（自動）は書き換えず、読み出すときに適用するので、再解析してもユーザーの修正は消えない。
//!
//! - meter: 拍子の手動指定 / downbeat_anchors: 「ここを 1 拍目に」（拍番号のリスト。その拍を含む区間の位相を合わせる）
//! - grid_offset_ms: 拍全体を前後にずらす / bpm_scale: BPM の表示を ×½ / ×2 にする
//! - hotcues / cue_point: ホットキュー 4 つとキュー位置（秒）
use crate::analysis::bars::{bar_phases, downbeat_strength};
use crate::analysis::beats::segment_ids;
use crate::analysis::cache;
use crate::analysis::types::{Analysis, Bar};
use crate::db;
use crate::util::round;
use anyhow::Result;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Default)]
pub struct Edits {
    pub meter: Option<i64>,
    pub bpm_scale: Option<f64>,
    pub downbeat_anchors: Option<Vec<i64>>,
    pub grid_offset_ms: Option<f64>,
    pub hotcues: Option<Value>,
    pub cue_point: Option<Value>,
    pub memo: Option<String>,
    pub exists: bool,
}

pub fn get_row(track_id: i64) -> Result<Edits> {
    db::with(|c| {
        let r = c.query_row(
            "SELECT meter, bpm_scale, downbeat_anchors, grid_offset_ms, hotcues, cue_point, memo FROM user_edits WHERE track_id = ?",
            [track_id],
            |r| {
                Ok(Edits {
                    meter: r.get(0)?,
                    bpm_scale: r.get(1)?,
                    downbeat_anchors: r
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| serde_json::from_str::<Vec<i64>>(&s).ok()),
                    grid_offset_ms: r.get(3)?,
                    hotcues: r.get::<_, Option<String>>(4)?.and_then(|s| serde_json::from_str(&s).ok()),
                    cue_point: r.get::<_, Option<f64>>(5)?.map(|v| json!(v)),
                    memo: r.get(6)?,
                    exists: true,
                })
            },
        );
        match r {
            Ok(e) => Ok(e),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Edits::default()),
            Err(e) => Err(e.into()),
        }
    })
}

/// GET /api/tracks/{id}/edits と同じ形（無ければ {}）。
pub fn get(track_id: i64) -> Result<Map<String, Value>> {
    let e = get_row(track_id)?;
    let mut m = Map::new();
    if !e.exists {
        return Ok(m);
    }
    m.insert("track_id".into(), json!(track_id));
    m.insert("meter".into(), json!(e.meter));
    m.insert("bpm_scale".into(), json!(e.bpm_scale.unwrap_or(1.0)));
    m.insert("downbeat_anchors".into(), json!(e.downbeat_anchors));
    m.insert("grid_offset_ms".into(), json!(e.grid_offset_ms.unwrap_or(0.0)));
    m.insert("hotcues".into(), e.hotcues.clone().unwrap_or(Value::Null));
    m.insert("cue_point".into(), e.cue_point.clone().unwrap_or(Value::Null));
    m.insert("memo".into(), json!(e.memo));
    Ok(m)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        Value::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

/// patch のキー: meter(null で自動に戻す), bpm_scale, grid_offset_ms, add_anchor(拍番号), clear_anchors, hotcues, cue_point。
pub fn update(track_id: i64, patch: &Map<String, Value>) -> Result<Edits> {
    let mut cur = get_row(track_id)?;
    if let Some(m) = patch.get("meter") {
        cur.meter = num(m).filter(|x| *x != 0.0).map(|x| x as i64);
    }
    if let Some(v) = patch.get("bpm_scale") {
        let x = num(v).unwrap_or(1.0);
        cur.bpm_scale = Some(if [0.5, 1.0, 2.0].contains(&x) { x } else { 1.0 });
    }
    if let Some(v) = patch.get("grid_offset_ms") {
        cur.grid_offset_ms = Some(num(v).unwrap_or(0.0).clamp(-200.0, 200.0));
    }
    if patch.get("clear_anchors").map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false) {
        cur.downbeat_anchors = None;
    }
    if let Some(a) = patch.get("add_anchor").and_then(num) {
        let a = a as i64;
        let mut anchors: Vec<i64> =
            cur.downbeat_anchors.clone().unwrap_or_default().into_iter().filter(|x| (x - a).abs() > 2).collect();
        anchors.push(a);
        anchors.sort();
        cur.downbeat_anchors = Some(anchors);
    }
    if let Some(h) = patch.get("hotcues") {
        cur.hotcues = if h.is_null() { None } else { Some(h.clone()) };
    }
    if let Some(c) = patch.get("cue_point") {
        cur.cue_point = if c.is_null() { None } else { Some(c.clone()) };
    }
    let anchors = cur.downbeat_anchors.as_ref().filter(|v| !v.is_empty()).map(|v| serde_json::to_string(v).unwrap());
    let hot = cur.hotcues.as_ref().map(|v| v.to_string());
    let cue = cur.cue_point.as_ref().and_then(num);
    db::tx(|c| {
        c.execute(
            "INSERT INTO user_edits(track_id, meter, bpm_scale, downbeat_anchors, grid_offset_ms, hotcues, cue_point, memo)
             VALUES(?,?,?,?,?,?,?,?)
             ON CONFLICT(track_id) DO UPDATE SET meter=excluded.meter, bpm_scale=excluded.bpm_scale,
               downbeat_anchors=excluded.downbeat_anchors, grid_offset_ms=excluded.grid_offset_ms,
               hotcues=excluded.hotcues, cue_point=excluded.cue_point, memo=excluded.memo",
            rusqlite::params![
                track_id,
                cur.meter,
                cur.bpm_scale.filter(|x| *x != 0.0).unwrap_or(1.0),
                anchors,
                cur.grid_offset_ms.unwrap_or(0.0),
                hot,
                cue,
                cur.memo
            ],
        )?;
        // ライブラリ・キューの BPM 表示にも倍/半を反映する
        if let Some(a) = cache::read(track_id) {
            c.execute(
                "UPDATE tracks SET bpm = ? WHERE id = ?",
                rusqlite::params![round(a.bpm * cur.bpm_scale.filter(|x| *x != 0.0).unwrap_or(1.0), 2), track_id],
            )?;
        }
        Ok(())
    })?;
    cur.exists = true;
    Ok(cur)
}

/// 1 拍目の指定を反映する。指定した拍を含む区間（拍の途切れまで）で、指定の拍から前後に meter 拍ごとを小節頭にする。
fn anchor_bars(beats: &[f64], bars: &[Bar], anchors: &[i64], meter: i64) -> Vec<Bar> {
    let n = beats.len() as i64;
    let seg = segment_ids(beats);
    let mut out: Vec<Bar> = bars.to_vec();
    let mut sids: Vec<usize> = anchors.iter().filter(|&&a| 0 <= a && a < n).map(|&a| seg[a as usize]).collect();
    sids.sort();
    sids.dedup();
    for sid in sids {
        let idx: Vec<i64> = (0..n).filter(|&i| seg[i as usize] == sid).collect();
        let (lo, hi) = (idx[0], *idx.last().unwrap());
        let mut ancs: Vec<i64> = anchors.iter().copied().filter(|&a| lo <= a && a <= hi).collect();
        ancs.sort();
        let mut new = vec![];
        for (i, &a) in ancs.iter().enumerate() {
            let start = if i == 0 { lo } else { a };
            let end = if i + 1 < ancs.len() { ancs[i + 1] - 1 } else { hi };
            let first = a - ((a - start).div_euclid(meter)) * meter;
            let mut b = first;
            while b <= end {
                new.push(Bar { beat: b, time: round(beats[b as usize], 4), conf: 1.0, meter: None });
                b += meter;
            }
        }
        out.retain(|x| !(lo <= x.beat && x.beat <= hi));
        out.extend(new);
    }
    out.sort_by_key(|b| b.beat);
    out
}

/// 修正を反映した解析結果（拍・小節・拍子・テンポの曲線）。
pub fn apply_typed(track_id: i64, analysis: &Analysis, e: &Edits) -> Analysis {
    let mut out = analysis.clone();
    let beats = &analysis.beats;
    let meter = e.meter.filter(|m| *m > 0).unwrap_or(analysis.meter as i64);
    if meter != analysis.meter as i64 {
        if let Some(dp) = cache::downbeat_prob(track_id) {
            out.bars = bar_phases(beats, &downbeat_strength(beats, &dp), meter as u32);
        }
    }
    out.meter = meter as u32;
    if let Some(anchors) = e.downbeat_anchors.as_ref().filter(|v| !v.is_empty()) {
        out.bars = anchor_bars(beats, &out.bars, anchors, meter);
    }
    let off = e.grid_offset_ms.unwrap_or(0.0) / 1000.0;
    if off != 0.0 {
        out.beats = beats.iter().map(|b| round(b + off, 4)).collect();
        for b in out.bars.iter_mut() {
            b.time = round(b.time + off, 4);
        }
        out.tempo_curve.t = analysis.tempo_curve.t.iter().map(|t| round(t + off, 3)).collect();
    }
    out
}

pub fn apply(track_id: i64, analysis: &Analysis) -> Analysis {
    let e = get_row(track_id).unwrap_or_default();
    apply_typed(track_id, analysis, &e)
}

/// 画面に返す形（GET /api/tracks/{id}/analysis）: 修正を反映した解析結果 + edits・meter_source・bpm_scale・bpm_display。
pub fn apply_value(track_id: i64, analysis: &Analysis) -> Result<Value> {
    let e = get_row(track_id)?;
    let out = apply_typed(track_id, analysis, &e);
    let mut v = serde_json::to_value(&out)?;
    let m = v.as_object_mut().unwrap();
    let mut ed = get(track_id)?;
    ed.remove("track_id");
    ed.retain(|_, v| !v.is_null());
    m.insert("edits".into(), Value::Object(ed));
    m.insert("meter_source".into(), json!(if e.meter.filter(|x| *x > 0).is_some() { "user" } else { "auto" }));
    let scale = e.bpm_scale.filter(|x| *x != 0.0).unwrap_or(1.0);
    m.insert("bpm_scale".into(), json!(scale));
    m.insert("bpm_display".into(), json!(round(analysis.bpm * scale, 2)));
    Ok(v)
}
