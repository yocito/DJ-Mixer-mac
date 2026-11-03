//! 繋ぎの計算（djmix/export/transition.py。frontend/engine/transition.js と同じ値を返す。tests/export_plan.rs で一致を確かめる）。
//!
//! 繋ぎの設定（params）は画面の JSON そのまま（serde_json::Map）。数の値は `num()` で読む。
use super::render::BeatMap;
use serde::Serialize;
use serde_json::{Map, Value};

/// テンプレートと既定の小節数
pub const TEMPLATES: [(&str, i64); 6] = [("long", 16), ("short", 8), ("bass", 16), ("filter", 16), ("echo", 2), ("cut", 0)];
/// パートごとに鳴らす（Phase 5 B2。transition.js の PARTS）
pub const PARTS: [&str; 4] = ["none", "drums", "vocal", "swap"];

/// 繋ぎの設定の既定値（transition.py の DEFAULT_PARAMS）
pub fn default_params() -> Map<String, Value> {
    let v = serde_json::json!({"swapAt": 0.5, "xfStart": 0.0, "xfEnd": 1.0, "tempo": "sync", "loop": 0, "shift": 0,
                               "parts": "none", "cutTrim": "auto"});
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

pub fn template_bars(tpl: &str) -> Option<i64> {
    TEMPLATES.iter().find(|(k, _)| *k == tpl).map(|(_, b)| *b)
}

/// 繋ぎ（join_of の戻り値）
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Join {
    pub tpl: String,
    pub bars: i64,
    pub params: Map<String, Value>,
}

impl Join {
    pub fn param(&self, k: &str) -> Option<&Value> {
        self.params.get(k)
    }
    pub fn num(&self, k: &str) -> f64 {
        num(&self.params, k)
    }
    pub fn str_param(&self, k: &str) -> Option<&str> {
        self.params.get(k).and_then(|v| v.as_str())
    }
}

/// 設定の数の値（Python の float(x)。数でなければ 0、真偽は 0/1、数の文字列は読む）
pub fn num(p: &Map<String, Value>, k: &str) -> f64 {
    value_f64(p.get(k)).unwrap_or(0.0)
}

pub fn value_f64(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Python の bool(x)（値の真偽）
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub fn uses_parts(join: Option<&Join>) -> bool {
    match join {
        Some(j) => {
            ["long", "short", "bass"].contains(&j.tpl.as_str())
                && j.str_param("parts").map(|p| PARTS[1..].contains(&p)).unwrap_or(false)
        }
        None => false,
    }
}

pub fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0).max(0.0)
}

/// キューの項目（join_tpl・join_bars・join_params）→ 繋ぎ
pub fn join_of(item: Option<&Map<String, Value>>) -> Join {
    let empty = Map::new();
    let item = item.unwrap_or(&empty);
    let tpl = item
        .get("join_tpl")
        .and_then(|v| v.as_str())
        .filter(|t| template_bars(t).is_some())
        .unwrap_or("long")
        .to_string();
    let bars = match item.get("join_bars") {
        None | Some(Value::Null) => template_bars(&tpl).unwrap_or(16),
        Some(v) => value_f64(Some(v)).map(|x| x.trunc() as i64).unwrap_or_else(|| template_bars(&tpl).unwrap_or(16)),
    };
    let mut params = default_params();
    if let Some(Value::Object(p)) = item.get("join_params") {
        for (k, v) in p {
            params.insert(k.clone(), v.clone());
        }
    }
    let bars = if tpl == "cut" { 0 } else { bars.clamp(1, 64) };
    Join { tpl, bars, params }
}

fn step(u: f64, at: f64, w: f64) -> f64 {
    if w <= 0.0 {
        if u >= at {
            1.0
        } else {
            0.0
        }
    } else {
        clamp01((u - at) / w + 0.5)
    }
}

/// 自動の動き（levels() の戻り値。キーは transition.js と同じ）
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Levels {
    pub xf: f64,
    #[serde(rename = "lowOut")]
    pub low_out: f64,
    #[serde(rename = "lowIn")]
    pub low_in: f64,
    #[serde(rename = "hpOut")]
    pub hp_out: f64,
    #[serde(rename = "echoOut")]
    pub echo_out: f64,
}

pub fn levels(join: &Join, u: f64, beats: f64) -> Levels {
    let tpl = join.tpl.as_str();
    let u = clamp01(u);
    let one_beat = 1.0 / beats.max(1.0);
    let neutral = uses_parts(Some(join)); // パートごとに鳴らすときは EQ の LOW を動かさない（Phase 5 B2）
    let swap_at = join.num("swapAt");
    let swap_out = if neutral { 1.0 } else { 1.0 - step(u, swap_at, one_beat) };
    let swap_in = if neutral { 1.0 } else { step(u, swap_at, one_beat) };
    let last_bar = (4.0 * one_beat).min(1.0);
    match tpl {
        "bass" => Levels {
            xf: if u < 1.0 - last_bar { 0.5 } else { 0.5 + 0.5 * clamp01((u - (1.0 - last_bar)) / last_bar) },
            low_out: swap_out,
            low_in: swap_in,
            hp_out: 0.0,
            echo_out: 0.0,
        },
        "filter" => Levels {
            xf: if u < 0.8 { 0.5 } else { 0.5 + 0.5 * clamp01((u - 0.8) / 0.2) },
            low_out: 1.0 - u,
            low_in: clamp01(u * 2.0),
            hp_out: u * 0.9,
            echo_out: 0.0,
        },
        "echo" => Levels {
            xf: if u < 1.0 - one_beat / 2.0 { 0.0 } else { 1.0 },
            low_out: 1.0,
            low_in: 1.0,
            hp_out: 0.0,
            echo_out: clamp01((u - (1.0 - last_bar)) / last_bar),
        },
        "cut" => Levels { xf: if u < 1.0 { 0.0 } else { 1.0 }, low_out: 1.0, low_in: 1.0, hp_out: 0.0, echo_out: 0.0 },
        _ => {
            let span = (join.num("xfEnd") - join.num("xfStart")).max(1e-6);
            Levels {
                xf: clamp01((u - join.num("xfStart")) / span),
                low_out: swap_out,
                low_in: swap_in,
                hp_out: 0.0,
                echo_out: 0.0,
            }
        }
    }
}

/// パートごとの音量 {out: [v, d, b, o], in: [...]}（transition.js の partGains と同じ値）
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PartGains {
    pub out: [f64; 4],
    #[serde(rename = "in")]
    pub inn: [f64; 4],
}

pub fn part_gains(join: &Join, u: f64, beats: f64) -> Option<PartGains> {
    if !uses_parts(Some(join)) {
        return None;
    }
    let mode = join.str_param("parts").unwrap_or("none");
    let u = clamp01(u);
    let one_beat = 1.0 / beats.max(1.0);
    let bar = (4.0 * one_beat).min(0.25);
    let mut out = [1.0; 4];
    let mut inn = [1.0; 4];
    if mode == "vocal" || mode == "swap" {
        out[0] = 1.0 - clamp01(u / 0.25);
    }
    if mode == "swap" {
        out[2] = 1.0 - step(u, join.num("swapAt"), one_beat);
    }
    if mode == "drums" || mode == "swap" {
        inn[3] = clamp01((u - 0.25) / bar.max(0.125));
        inn[2] = step(u, join.num("swapAt"), one_beat);
        inn[0] = clamp01((u - 0.75) / 0.25);
    }
    Some(PartGains { out, inn })
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Overlap {
    #[serde(rename = "startTime")]
    pub start_time: f64,
    #[serde(rename = "startBar")]
    pub start_bar: usize,
    #[serde(rename = "endBar")]
    pub end_bar: usize,
}

pub fn overlap_start(bar_times: &[f64], out_end: f64, bars: i64) -> Overlap {
    if bar_times.is_empty() {
        // 小節が無い（Python では落ちる）。前の曲の終わりで入れる
        return Overlap { start_time: out_end, start_bar: 0, end_bar: 0 };
    }
    let mut end = 0usize;
    for i in 0..bar_times.len() {
        if (bar_times[i] - out_end).abs() < (bar_times[end] - out_end).abs() {
            end = i;
        }
    }
    let start = (end as i64 - bars).max(0) as usize;
    Overlap { start_time: bar_times[start], start_bar: start, end_bar: end }
}

pub fn in_start_bar(bar_times: &[f64], t0: f64) -> usize {
    let mut b = 0usize;
    for i in 1..bar_times.len() {
        if (bar_times[i] - t0).abs() < (bar_times[b] - t0).abs() {
            b = i;
        }
    }
    b
}

pub fn enters_at_end(tpl: &str) -> bool {
    tpl == "echo" || tpl == "cut"
}

/// Python の round(x)（偶数への丸め）
pub fn py_round(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 {
        2.0 * (x / 2.0).round()
    } else {
        r
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Judge {
    pub k: f64,
    pub rate: f64,
    pub reason: Option<String>,
}

pub fn judge(bm_out: &BeatMap, bm_in: &BeatMap, ov_start: f64, ov_end: f64, in_start: f64, rate_out: f64) -> Judge {
    let bpm_out = 60.0 / bm_out.beat_len(ov_start) * rate_out;
    let bpm_in = 60.0 / bm_in.beat_len(in_start);
    let mut best: Option<(f64, f64)> = None;
    for k in [1.0, 2.0, 0.5] {
        let rate = k * bpm_out / bpm_in;
        if best.map(|b| rate.ln().abs() < b.1.ln().abs()).unwrap_or(true) {
            best = Some((k, rate));
        }
    }
    let (k, rate) = best.unwrap();
    let mut reason = None;
    if !(0.7..=1.3).contains(&rate) {
        reason = Some(format!("テンポの差が大きすぎます（{}%）", py_round((rate - 1.0) * 100.0) as i64));
    } else {
        let mut t = ov_start;
        while t < ov_end {
            if bm_out.in_no_beat(t) {
                reason = Some("前の曲の繋ぐ部分に拍がありません".to_string());
                break;
            }
            t += 0.25;
        }
        if reason.is_none() {
            let mut t = in_start;
            let end = in_start + (ov_end - ov_start).max(2.0);
            while t < end {
                if bm_in.in_no_beat(t) {
                    reason = Some("次の曲の入る部分に拍がありません".to_string());
                    break;
                }
                t += 0.25;
            }
        }
    }
    Judge { k, rate, reason }
}

/// 繋ぎの終わりの次の曲の速度（直前 4 拍の平均。transition.js の syncedRate と同じ式）
pub fn synced_rate(bm_in: &BeatMap, bm_out: &BeatMap, pos_in: f64, pos_out: f64, k: f64, rate_out: f64) -> f64 {
    let (p_i, p_o) = (bm_in.pos(pos_in), bm_out.pos(pos_out));
    let in_sec = bm_in.time(p_i) - bm_in.time(p_i - 4.0 * k);
    let out_sec = bm_out.time(p_o) - bm_out.time(p_o - 4.0);
    if out_sec > 0.0 {
        in_sec * rate_out / out_sec
    } else {
        1.0
    }
}

/// 重なりの小節（ループを含む。transition.js の overlapBars と同じ）
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct OverlapBars {
    #[serde(rename = "startTime")]
    pub start_time: f64,
    #[serde(rename = "startBar")]
    pub start_bar: usize,
    #[serde(rename = "endBar")]
    pub end_bar: usize,
    #[serde(rename = "loop")]
    pub loop_bars: i64,
    #[serde(rename = "loopStartBar")]
    pub loop_start_bar: usize,
}

pub fn overlap_bars(bar_times: &[f64], out_end: f64, join: &Join) -> OverlapBars {
    let lp = if enters_at_end(&join.tpl) {
        0
    } else {
        let l = value_f64(join.param("loop")).map(|x| x.trunc() as i64).unwrap_or(0);
        l.min(join.bars - 1).max(0)
    };
    let ov = overlap_start(bar_times, out_end, join.bars - lp);
    OverlapBars {
        start_time: ov.start_time,
        start_bar: ov.start_bar,
        end_bar: ov.end_bar,
        loop_bars: lp,
        // Python では負の番号（後ろから数える）になりうるが、ここでは 0 で止める
        loop_start_bar: (ov.end_bar as i64 - lp).max(0) as usize,
    }
}

pub fn ramp_target(bm_out: &BeatMap, bm_in: &BeatMap, pos_out_end: f64, pos_in_end: f64, k: f64) -> f64 {
    let (p_o, p_i) = (bm_out.pos(pos_out_end), bm_in.pos(pos_in_end));
    let out_sec = bm_out.time(p_o) - bm_out.time(p_o - 4.0);
    let in_sec = bm_in.time(p_i) - bm_in.time(p_i - 4.0 * k);
    if in_sec > 0.0 {
        out_sec / in_sec
    } else {
        1.0
    }
}
