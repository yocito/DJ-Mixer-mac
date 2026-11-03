//! 区間の候補（曲の構造の区切り。djmix/analysis/sections.py）。
//!
//! 小節ごとの特徴量（音量・低音・和音の濃さ・クロマ）から、前後 8 小節の平均の差（ノベルティ）が大きい小節を区切りにする。
//! 細かすぎないよう、区切りの弱いものから外して「各区間 12 小節以上（または 20 秒以上）、区間数は約 35 秒に 1 つ以下」にする。
//! 区間には性格の札（静か／中くらい／盛り上がり・低音少なめ・和音うすめ・拍なし・イントロ候補／アウトロ候補）を付ける。
use super::cache;
use super::types::{Analysis, BarFeats, Section, COL_CHROMA, COL_HARM, COL_LOW, COL_RMS};
use crate::util::{percentile, round, searchsorted_left};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

const WIN: usize = 8;
const MIN_BARS: usize = 12;
const MIN_SEC: f64 = 20.0;
const SEC_PER_SECTION: f64 = 35.0;

fn z(x: &[f64]) -> Vec<f64> {
    let m = crate::util::mean(x);
    let s = crate::util::std(x) + 1e-9;
    x.iter().map(|v| (v - m) / s).collect()
}

pub fn compute(a: &Analysis, feats: &BarFeats) -> Vec<Section> {
    let n = a.bars.len();
    if n < 4 || feats.len() != n {
        return vec![Section {
            bar0: 0,
            bar1: n,
            t0: 0.0,
            t1: round(a.duration, 2),
            bars: n,
            tags: vec!["曲全体".into()],
        }];
    }
    let times: Vec<f64> = a.bar_times();
    let rms: Vec<f64> = feats.iter().map(|r| r[COL_RMS] as f64).collect();
    let low: Vec<f64> = feats.iter().map(|r| r[COL_LOW] as f64).collect();
    let harm: Vec<f64> = feats.iter().map(|r| r[COL_HARM] as f64).collect();
    let (zr, zl, zh) = (z(&rms), z(&low), z(&harm));
    // X = [z(rms)*1.5, z(low)*1.2, z(harm), 正規化したクロマ 12]
    let dim = 15;
    let mut x = vec![0f64; n * dim];
    for i in 0..n {
        let ch: Vec<f64> = (0..12).map(|k| feats[i][COL_CHROMA + k] as f64).collect();
        let nrm = ch.iter().map(|v| v * v).sum::<f64>().sqrt() + 1e-9;
        x[i * dim] = zr[i] * 1.5;
        x[i * dim + 1] = zl[i] * 1.2;
        x[i * dim + 2] = zh[i];
        for k in 0..12 {
            x[i * dim + 3 + k] = ch[k] / nrm;
        }
    }
    let mut nov = vec![0f64; n];
    if n >= 2 * WIN {
        for i in WIN..=(n - WIN) {
            let mut d2 = 0.0;
            for k in 0..dim {
                let mut before = 0.0;
                let mut after = 0.0;
                for j in (i - WIN)..i {
                    before += x[j * dim + k];
                }
                for j in i..(i + WIN) {
                    after += x[j * dim + k];
                }
                let d = before / WIN as f64 - after / WIN as f64;
                d2 += d * d;
            }
            nov[i] = d2.sqrt();
        }
    }
    // 4 小節の頭（フレーズの頭）を少し優遇
    let score: Vec<f64> = nov.iter().enumerate().map(|(i, v)| v * if i % 4 == 0 { 1.08 } else { 1.0 }).collect();
    let pos: Vec<f64> = score.iter().copied().filter(|&v| v > 0.0).collect();
    let thr = if pos.is_empty() { f64::INFINITY } else { percentile(&pos, 80.0) };

    let mut cuts: Vec<usize> = vec![];
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&p, &q| score[q].partial_cmp(&score[p]).unwrap_or(std::cmp::Ordering::Equal).then(p.cmp(&q)));
    for i in order {
        if score[i] < thr {
            break;
        }
        if cuts.iter().all(|&c| (i as i64 - c as i64).abs() >= WIN as i64) && WIN <= i && i <= n - WIN / 2 {
            cuts.push(i);
        }
    }
    let mut nb_cuts: BTreeSet<usize> = BTreeSet::new();
    for &(s, e) in &a.no_beat {
        for t in [s, e] {
            let k = searchsorted_left(&times, t);
            if 0 < k && k < n {
                nb_cuts.insert(k);
                if cuts.iter().all(|&c| (k as i64 - c as i64).abs() >= 4) {
                    cuts.push(k);
                }
            }
        }
    }
    let mut set: BTreeSet<usize> = cuts.into_iter().collect();
    set.insert(0);
    set.insert(n);
    let mut cuts: Vec<usize> = set.into_iter().collect();

    // 弱い区切りから外す
    let max_segs = 4usize.max((a.duration / SEC_PER_SECTION) as usize);
    let mut strength: HashMap<usize, f64> =
        cuts.iter().map(|&c| (c, if 0 < c && c < n { score[c] } else { f64::INFINITY })).collect();
    for c in &nb_cuts {
        if let Some(v) = strength.get_mut(c) {
            *v = v.max(thr);
        }
    }
    let end_t = |j: usize| if j < n { times[j] } else { a.duration };
    loop {
        let lens: Vec<usize> = cuts.windows(2).map(|w| w[1] - w[0]).collect();
        let secs: Vec<f64> = (0..lens.len()).map(|i| end_t(cuts[i + 1]) - times[cuts[i]]).collect();
        // 短さは小節数と秒数の両方で見る（テンポが自由なイントロは 30 秒あっても数小節しかない）
        let short: Vec<usize> = (0..lens.len()).filter(|&i| lens[i] < MIN_BARS && secs[i] < MIN_SEC).collect();
        if cuts.len() <= 2 || (short.is_empty() && cuts.len() - 1 <= max_segs) {
            break;
        }
        let cand: Vec<usize> = if let Some(&s0) = short.first() {
            [cuts[s0], cuts[s0 + 1]].into_iter().filter(|&c| 0 < c && c < n).collect()
        } else {
            cuts[1..cuts.len() - 1].to_vec()
        };
        if cand.is_empty() {
            break;
        }
        let mut best = cand[0];
        for &c in &cand[1..] {
            if strength[&c] < strength[&best] {
                best = c;
            }
        }
        cuts.retain(|&c| c != best);
    }

    let q1 = percentile(&rms, 33.0);
    let q2 = percentile(&rms, 66.0);
    let low25 = percentile(&low, 25.0);
    let harm25 = percentile(&harm, 25.0);
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let mut segs = vec![];
    for w in cuts.windows(2) {
        let (s, e) = (w[0], w[1]);
        let (t0, t1) = (times[s], end_t(e));
        let r = mean(&rms[s..e]);
        let mut tags = vec![if r < q1 { "静か" } else if r > q2 { "盛り上がり" } else { "中くらい" }.to_string()];
        if mean(&low[s..e]) < low25 {
            tags.push("低音少なめ".into());
        }
        if mean(&harm[s..e]) < harm25 {
            tags.push("和音うすめ".into());
        }
        if a.nobeat_overlap(t0, t1) > 0.5 * (t1 - t0) {
            tags.push("拍なし".into());
        }
        segs.push(Section { bar0: s, bar1: e, t0: round(t0, 3), t1: round(t1, 3), bars: e - s, tags });
    }
    if segs.len() > 1 {
        segs[0].tags.insert(0, "イントロ候補".into());
        let l = segs.len() - 1;
        segs[l].tags.insert(0, "アウトロ候補".into());
    }
    segs
}

static MEMO: Lazy<Mutex<HashMap<i64, Arc<Vec<Section>>>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 解析済みなら区間の候補（同じ解析結果には計算結果を使い回す）。
pub fn for_track(track_id: i64) -> Option<Arc<Vec<Section>>> {
    if let Some(v) = MEMO.lock().get(&track_id) {
        return Some(v.clone());
    }
    let a = cache::read(track_id)?;
    let feats = cache::bars_array(track_id).unwrap_or_default();
    let segs = Arc::new(compute(&a, &feats));
    let mut m = MEMO.lock();
    if m.len() > 256 {
        if let Some(k) = m.keys().next().copied() {
            m.remove(&k);
        }
    }
    m.insert(track_id, segs.clone());
    Some(segs)
}

/// 再解析したら呼ぶ。
pub fn forget(track_id: i64) {
    MEMO.lock().remove(&track_id);
}
