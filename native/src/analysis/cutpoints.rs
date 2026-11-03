//! カットインの切り替えポイントの候補と、音量合わせ（djmix/analysis/cutpoints.py。design_cutin.md）。
//!
//! 候補の点数（前の曲の「ここで切る」= 小節 b の頭。直前の小節 b-1 までが鳴る）:
//!   区間の切れ目 +3 ／ 区間の頭から 8 小節の区切り +1.5（4 小節 +0.5）／ 直前 4 小節が曲の中で大きい +1
//!   ／ 直前の小節に歌が無い +1、歌が小節をまたいで続く −2 ／ 拍の取れない所 −3
//! 次の曲の「ここから入る」= 小節 b の頭:
//!   区間の頭 +3 ／ 曲の最初の小節 +2 ／ その後 4 小節が大きい +1.5 ／ 歌い出し +0.5 ／ 8 小節の区切り +1（4 小節 +0.5）
use super::stems::{StemLevels, VOCAL_ON};
use super::types::{Analysis, BarFeats, CutBar, CutPoint, CutPoints, Section, COL_RMS};
use super::{cache, sections, stems};
use crate::util::{median, round};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub const TRIM_MAX_DB: f64 = 6.0;
pub const TRIM_DEAD_DB: f64 = 1.0;
pub const LEVEL_BARS: f64 = 4.0;
pub const TRIM_BACK_BARS: f64 = 16.0;
pub const GOOD: f64 = 3.0;

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-6).log10()
}

pub fn compute(a: &Analysis, feats: &BarFeats, segs: &[Section], vocals: Option<&StemLevels>) -> CutPoints {
    let n = a.bars.len();
    let times: Vec<f64> = a.bar_times();
    let rms: Vec<f64> = if feats.len() == n { feats.iter().map(|r| r[COL_RMS] as f64).collect() } else { vec![0.0; n] };
    let bar_db: Vec<f64> = rms.iter().map(|&v| round(db(v), 2)).collect();
    let bars_out: Vec<CutBar> = times.iter().zip(&bar_db).map(|(&t, &d)| CutBar { t, db: d }).collect();
    if n < 8 {
        return CutPoints { bars: bars_out, outs: vec![], ins: vec![] };
    }
    let med = median(&bar_db);
    let mut starts: Vec<usize> = segs.iter().map(|s| s.bar0).filter(|&b| 0 < b && b < n).collect();
    starts.sort();
    starts.dedup();
    let sec_of = |b: usize| starts.iter().copied().filter(|&s| s <= b).max().unwrap_or(0);
    let mut voc: Option<Vec<f32>> = vocals.filter(|v| v.len() == n).map(|v| v.iter().map(|r| r[0]).collect());
    if let Some(v) = &voc {
        if v.iter().filter(|&&x| x >= VOCAL_ON).count() < 4 {
            voc = None; // 歌の無い曲（インスト）は歌の切れ目を見ない
        }
    }
    let nb = |b0: i64, b1: usize| -> bool {
        let t0 = times[b0.max(0) as usize];
        let t1 = if b1 < n { times[b1] } else { a.duration };
        a.no_beat.iter().any(|&(s, e)| t1.min(e) - t0.max(s) > 0.2)
    };
    let lvl = |b0: i64, b1: i64| -> f64 {
        let lo = b0.max(0) as usize;
        let hi = b1.min(n as i64).max(0) as usize;
        if hi <= lo {
            return -120.0;
        }
        let seg = &rms[lo..hi];
        db((seg.iter().map(|v| v * v).sum::<f64>() / seg.len() as f64).sqrt())
    };

    let mut outs = vec![];
    for b in 4..n {
        let mut sc = 0.0;
        let mut tags: Vec<String> = vec![];
        if starts.contains(&b) {
            sc += 3.0;
            tags.push("区間の切れ目".into());
        }
        let ph = b - sec_of(b);
        if ph != 0 && ph % 8 == 0 {
            sc += 1.5;
            tags.push("8 小節の区切り".into());
        } else if ph != 0 && ph % 4 == 0 {
            sc += 0.5;
        }
        let (before, after) = (lvl(b as i64 - 4, b as i64), lvl(b as i64, b as i64 + 4));
        if before > med + 1.5 && after < before - 1.0 {
            sc += 1.0;
            tags.push("盛り上がりの終わり".into());
        }
        if let Some(v) = &voc {
            if v[b - 1] < VOCAL_ON {
                sc += 1.0;
                tags.push("歌の切れ目".into());
            } else if v[b] >= VOCAL_ON {
                sc -= 2.0;
                tags.push("歌の途中".into());
            }
        }
        if nb(b as i64 - 2, n.min(b + 1)) {
            sc -= 3.0;
            tags.push("拍なし".into());
        }
        outs.push(CutPoint { bar: b, t: round(times[b], 3), score: round(sc, 2), tags });
    }
    let mut ins = vec![];
    for b in 0..(n - 8) {
        let mut sc = 0.0;
        let mut tags: Vec<String> = vec![];
        if b == 0 {
            sc += 2.0;
            tags.push("曲の頭".into());
        } else if starts.contains(&b) {
            sc += 3.0;
            tags.push("区間の頭".into());
        }
        let ph = b - sec_of(b);
        if ph != 0 && ph % 8 == 0 {
            sc += 1.0;
            tags.push("8 小節の区切り".into());
        } else if ph != 0 && ph % 4 == 0 {
            sc += 0.5;
        }
        let bi = b as i64;
        if lvl(bi, bi + 4) > med + 1.5 && lvl(bi - 4, bi) < lvl(bi, bi + 4) - 1.0 {
            sc += 1.5;
            tags.push("サビ頭の候補".into());
        }
        if let Some(v) = &voc {
            if v[b] >= VOCAL_ON && (b == 0 || v[b - 1] < VOCAL_ON) {
                sc += 0.5;
                tags.push("歌い出し".into());
            }
        }
        if nb(bi, b + 2) {
            sc -= 3.0;
            tags.push("拍なし".into());
        }
        ins.push(CutPoint { bar: b, t: round(times[b], 3), score: round(sc, 2), tags });
    }
    CutPoints { bars: bars_out, outs, ins }
}

static MEMO: Lazy<Mutex<HashMap<(i64, bool), Arc<CutPoints>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 解析済みなら候補を返す（同じ解析結果には計算結果を使い回す）。
pub fn for_track(track_id: i64) -> Option<Arc<CutPoints>> {
    let voc = stems::read(track_id);
    let key = (track_id, voc.is_some());
    if let Some(v) = MEMO.lock().get(&key) {
        return Some(v.clone());
    }
    let a = cache::read(track_id)?;
    let feats = cache::bars_array(track_id).unwrap_or_default();
    let segs = sections::for_track(track_id).map(|s| (*s).clone()).unwrap_or_default();
    let res = Arc::new(compute(&a, &feats, &segs, voc.as_ref()));
    let mut m = MEMO.lock();
    if m.len() > 256 {
        if let Some(k) = m.keys().next().copied() {
            m.remove(&k);
        }
    }
    m.insert(key, res.clone());
    Some(res)
}

pub fn forget(track_id: i64) {
    MEMO.lock().retain(|k, _| k.0 != track_id);
}

// ---- 音量合わせ（transition.js の levelDb / cutTrimDb と同じ式） ----

/// [t0, t1) に頭がある小節の音量（RMS の二乗平均。dB）。小節が無ければ None。
pub fn level_db(bars: &[CutBar], t0: f64, t1: f64) -> Option<f64> {
    let v: Vec<f64> = bars.iter().filter(|b| t0 - 1e-3 <= b.t && b.t < t1 - 1e-3).map(|b| 10f64.powf(b.db / 10.0)).collect();
    if v.is_empty() {
        None
    } else {
        Some(10.0 * (v.iter().sum::<f64>() / v.len() as f64).log10())
    }
}

/// 曲ごとの音量の揃え（mixer.js の setLoudness と同じ。±の上限つき）。
pub fn loud_db(lufs: f64, target: f64) -> f64 {
    (target - lufs).clamp(-12.0, 9.0)
}

/// 次の曲の入りに掛ける音量（dB）: 前の曲の最後の LEVEL_BARS 小節と、次の曲の最初の LEVEL_BARS 小節の聞こえる大きさをそろえる。
#[allow(clippy::too_many_arguments)]
pub fn auto_trim_db(
    bars_out: &[CutBar],
    t_cut: f64,
    bar_out: f64,
    lufs_out: f64,
    bars_in: &[CutBar],
    t_in: f64,
    bar_in: f64,
    lufs_in: f64,
    target: f64,
) -> f64 {
    let lo = level_db(bars_out, t_cut - LEVEL_BARS * bar_out, t_cut);
    let li = level_db(bars_in, t_in, t_in + LEVEL_BARS * bar_in);
    let (Some(lo), Some(li)) = (lo, li) else { return 0.0 };
    let d = (lo + loud_db(lufs_out, target)) - (li + loud_db(lufs_in, target));
    if d.abs() < TRIM_DEAD_DB {
        return 0.0;
    }
    round(d.clamp(-TRIM_MAX_DB, TRIM_MAX_DB), 1)
}

/// 繋ぎの設定 cutTrim（'auto' / 'off' / dB の数）から、実際に掛ける dB。
pub fn trim_db(params: &serde_json::Map<String, Value>, auto: f64) -> f64 {
    match params.get("cutTrim") {
        Some(Value::String(s)) if s == "off" => 0.0,
        Some(Value::Number(x)) => x.as_f64().unwrap_or(0.0).clamp(-TRIM_MAX_DB, TRIM_MAX_DB),
        _ => auto,
    }
}
