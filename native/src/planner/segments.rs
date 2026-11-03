//! 1 曲の使う範囲の候補（djmix/planner/segments.py。使ってみての改善 I-7。おまかせの「1 曲あたりの長さ」「使う区間」と、結果の曲ごとの「区間」）。
//!
//! おまかせの要約（desc）は「区間の区切りで決めた範囲（おまかせ）」1 つだけを持つ。長さ・位置を指定されたら、解析キャッシュから
//! その範囲と、繋ぐ部分（始まり・終わりの 16 小節）の響き・拍・ボーカルを計算し直し、要約の use / in / out / vocals を差し替えて使う。
//! - pos: 'auto'（区間の区切りで決めた範囲。長さを指定したらその長さに合わせる）/ 'head'（頭から）/ 'peak'（盛り上がり中心）/
//!   'tail'（終わりまで）/ 'full'（曲全体。拍の取れない頭と終わりは除く）
//! - 範囲の端は区間の区切り（無ければ 4 小節の区切り）にそろえ、繋ぐ部分は拍の取れる所へ（desc::stable_bounds）
use super::desc::{self, np, Desc, Region, Use};
use crate::analysis::stems::{self, StemLevels};
use crate::analysis::types::{BarFeats, Section, COL_RMS};
use crate::analysis::{cache, sections};
use crate::util::searchsorted_left;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub const POSITIONS: [(&str, &str); 6] = [
    ("auto", "おまかせ"),
    ("intro", "冒頭（0:00）から"),
    ("head", "前奏を飛ばして頭から"),
    ("peak", "盛り上がり中心"),
    ("tail", "終わりまで"),
    ("full", "曲全体（0:00〜最後）"),
];
/// ミックスの最初の曲は、飛ばす前奏がこれ以下なら冒頭（0:00）から鳴らす
pub const FIRST_FROM_TOP_SEC: f64 = 30.0;
pub const MIN_SEC: f64 = 60.0;
/// これ以下の長さは「短くたくさん繋ぐ」用（2026-10-25）: 盛り上がりの所を使い、範囲の下限も長さに合わせて下げる
pub const SHORT_SEC: f64 = 90.0;
pub const SHORT_MIN_SEC: f64 = 20.0;
const MEMO_MAX: usize = 2000;

/// 1 曲あたりの長さ・使う区間の指定（要約の seg。結果の曲ごとの seg として画面に返す）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Seg {
    pub len: Option<f64>,
    pub pos: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub t0: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub t1: Option<f64>,
}

impl Default for Seg {
    fn default() -> Self {
        Seg { len: None, pos: "auto".into(), t0: None, t1: None }
    }
}

/// 要約の use / in / out / vocals を差し替える値（variant の戻り値）
#[derive(Debug, Clone)]
pub struct Variant {
    pub use_: Use,
    pub in_: Region,
    pub out: Region,
    pub vocals: Option<Vec<f64>>,
    pub seg: Seg,
}

impl Variant {
    /// 要約に差し替えを当てる（Python の {**d, **v}）
    pub fn apply(&self, d: &mut Desc) {
        d.use_ = self.use_.clone();
        d.in_ = self.in_.clone();
        d.out = self.out.clone();
        d.vocals = self.vocals.clone();
        d.seg = Some(self.seg.clone());
    }
}

/// 結果の曲ごとの「区間」の候補
#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    pub pos: String,
    pub len: Option<f64>,
    pub label: String,
    #[serde(rename = "use")]
    pub use_: Use,
}

pub fn is_short(length: Option<f64>) -> bool {
    matches!(length, Some(l) if l != 0.0 && l <= SHORT_SEC)
}

fn len_label(sec: f64) -> String {
    if sec < 90.0 {
        format!("約{}秒", np::round_int(sec))
    } else {
        format!("約{:.1}分", np::round(sec / 60.0, 1))
    }
}

/// 解析キャッシュから読んだもの（拍の取れない区間・小節の頭の時刻＋曲の終わり・小節ごとの特徴量・区間・パートの量）
#[derive(Debug)]
pub struct Loaded {
    pub no_beat: Vec<(f64, f64)>,
    pub bt: Vec<f64>,
    pub feats: BarFeats,
    pub secs: Vec<Section>,
    pub stems: Option<StemLevels>,
}

struct Memo {
    map: HashMap<i64, (Arc<Loaded>, u64)>,
    order: BTreeMap<u64, i64>,
    tick: u64,
}

static MEMO: Lazy<Mutex<Memo>> = Lazy::new(|| Mutex::new(Memo { map: HashMap::new(), order: BTreeMap::new(), tick: 0 }));

/// 再解析したら呼ぶ（Python 版には無い。古い範囲を使わないように）
pub fn forget(tid: i64) {
    let mut m = MEMO.lock();
    if let Some((_, t)) = m.map.remove(&tid) {
        m.order.remove(&t);
    }
}

/// 読み込んだもの（最近使った 2000 曲分を覚えておく）
pub fn load(tid: i64) -> Option<Arc<Loaded>> {
    {
        let mut m = MEMO.lock();
        m.tick += 1;
        let tick = m.tick;
        if let Some((v, t)) = m.map.get_mut(&tid) {
            let (v, old) = (v.clone(), *t);
            *t = tick;
            m.order.remove(&old);
            m.order.insert(tick, tid);
            return Some(v);
        }
    }
    let a = cache::read(tid)?;
    let feats = cache::bars_array(tid)?;
    if feats.len() != a.bars.len() || feats.len() < 16 {
        return None;
    }
    let st = stems::read(tid).filter(|s| s.len() == a.bars.len());
    let v = Arc::new(Loaded {
        no_beat: a.no_beat.clone(),
        bt: a.bar_edges(),
        secs: sections::compute(&a, &feats),
        feats,
        stems: st,
    });
    let mut m = MEMO.lock();
    m.tick += 1;
    let tick = m.tick;
    if let Some((_, old)) = m.map.insert(tid, (v.clone(), tick)) {
        m.order.remove(&old);
    }
    m.order.insert(tick, tid);
    while m.map.len() > MEMO_MAX {
        let Some((&t, &id)) = m.order.iter().next() else { break };
        m.order.remove(&t);
        m.map.remove(&id);
    }
    Some(v)
}

fn has(s: &Section, tag: &str) -> bool {
    s.tags.iter().any(|t| t == tag)
}

/// 曲全体（拍の取れない頭・終わりの区間は除く）。
fn full(secs: &[Section], bt: &[f64]) -> (i64, i64) {
    let n = bt.len() as i64 - 1;
    let ok: Vec<&Section> = secs.iter().filter(|s| !has(s, "拍なし")).collect();
    let ok: Vec<&Section> = if ok.is_empty() { secs.iter().collect() } else { ok };
    (ok[0].bar0 as i64, n.min(ok[ok.len() - 1].bar1 as i64))
}

/// 時刻 t に近い区切り（区間の区切りが tol 秒以内に無ければ、phrase_from から 4 小節ごとの区切り）の小節番号。
fn snap(bt: &[f64], cuts: &[i64], t: f64, tol: f64, lo: i64, hi: i64, phrase_from: i64) -> i64 {
    let mut best: Option<i64> = None;
    for &c in cuts {
        if lo <= c && c <= hi && (bt[c as usize] - t).abs() <= tol {
            match best {
                Some(b) if (bt[b as usize] - t).abs() <= (bt[c as usize] - t).abs() => {}
                _ => best = Some(c),
            }
        }
    }
    if let Some(b) = best {
        return b;
    }
    let k = searchsorted_left(bt, t) as i64;
    let k = lo.max(hi.min(k));
    let k = phrase_from + np::round_int((k - phrase_from) as f64 / 4.0) * 4;
    lo.max(hi.min(k))
}

fn bar_at(bt: &[f64], t: f64) -> i64 {
    let mut best = 0usize;
    for k in 1..bt.len() {
        if (bt[k] - t).abs() < (bt[best] - t).abs() {
            best = k;
        }
    }
    best as i64
}

/// float32 の音量の平均（numpy の rms[s:s+span].mean()）
fn rms_mean(feats: &BarFeats, s: usize, span: usize) -> f64 {
    let hi = (s + span).min(feats.len());
    np::mean_f32_col(&feats[s..hi], COL_RMS) as f64
}

/// 使う範囲 {t0, t1, bar0, bar1, notes} と読み込んだもの。作れなければ None（要約の範囲のまま）。
/// pos='custom' は t0〜t1（秒）を小節にそろえて使う（冒頭 0:00・曲の最後はそのまま）。
pub fn bounds(
    tid: i64,
    base_use: &Use,
    length: Option<f64>,
    pos: &str,
    t0: Option<f64>,
    t1: Option<f64>,
) -> Option<(Use, Arc<Loaded>)> {
    let l = load(tid)?;
    let bt = &l.bt;
    let n = bt.len() as i64 - 1;
    let (f0, f1) = full(&l.secs, bt);
    let mut cuts: Vec<i64> = l.secs.iter().map(|s| s.bar0 as i64).collect();
    cuts.push(f0);
    cuts.push(f1);
    cuts.sort();
    cuts.dedup();
    let at = |k: i64| bt[k as usize];
    let mut notes: Vec<String> = vec![];
    if pos == "full" {
        // 本当に曲全体（拍の取れない頭・終わりも含める）
        let u = Use { t0: 0.0, t1: np::round(at(n), 3), bar0: 0, bar1: n, notes: vec!["曲全体（0:00〜最後）".into()] };
        return Some((u, l.clone()));
    }
    if pos == "custom" {
        // 自分で選んだ区間（そのまま使う。拍の取れる所へはずらさない）
        let s0 = t0.unwrap_or(0.0).max(0.0);
        let s1 = at(n).min(t1.unwrap_or(at(n)));
        let top = s0 <= bt[0] + 0.3;
        let b0 = if top { 0 } else { bar_at(bt, s0) };
        let b1 = if s1 >= at(n) - 0.3 { n } else { bar_at(bt, s1) };
        if b1 - b0 < 4 || at(b1) - (if b0 == 0 && top { 0.0 } else { at(b0) }) < 20.0 {
            return None;
        }
        let u0 = if top { 0.0 } else { np::round(at(b0), 3) };
        let u = Use { t0: u0, t1: np::round(at(b1), 3), bar0: b0, bar1: b1, notes: vec!["自分で選んだ区間".into()] };
        return Some((u, l.clone()));
    }
    let base_len = base_use.t1 - base_use.t0;
    let tol_of = |want: f64| if !is_short(Some(want)) { 20f64.max(0.15 * want) } else { 4f64.max(0.25 * want) };
    if pos == "intro" {
        // 冒頭（0:00）から: 終わりだけ長さに合わせて拍の取れる所へ
        let want = match length {
            Some(x) if x != 0.0 => x,
            _ => base_len,
        };
        let b1 = snap(bt, &cuts, want, tol_of(want), 8, f1, 0);
        if b1 > n {
            return None;
        }
        let (_, b1) = desc::stable_bounds(&l.no_beat, bt, 0, b1 as usize, Some(&mut notes));
        notes.insert(0, format!("冒頭から{}", len_label(bt[b1])));
        let u = Use { t0: 0.0, t1: np::round(bt[b1], 3), bar0: 0, bar1: b1 as i64, notes: np::dedup(notes) };
        return Some((u, l.clone()));
    }
    let want = match length {
        Some(x) if x != 0.0 => x,
        _ => base_len,
    };
    let want = want.min(at(f1) - at(f0));
    // 区間の区切りへ吸着する幅。短いときは長さの 1/4 まで（30 秒の曲が 50 秒にならないように）
    let tol = tol_of(want);
    let mut pos = pos;
    if pos == "auto" && is_short(Some(want)) {
        pos = "peak"; // 短く使うときは、曲のいちばん聴かせ所（盛り上がり）を使う
    }
    let (b0, b1, label): (i64, i64, &str);
    if pos == "tail" {
        b1 = f1;
        b0 = snap(bt, &cuts, at(b1) - want, tol, f0, b1 - 8, b1);
        label = "終わりまで";
    } else if pos == "peak" {
        let span = 8i64.max(searchsorted_left(bt, at(f0) + want) as i64 - f0) as usize;
        let (mut best, mut bs) = (-1e9f64, f0);
        for s in f0..(f0 + 1).max(f1 - span as i64 + 1) {
            let m = rms_mean(&l.feats, s as usize, span);
            if m > best {
                best = m;
                bs = s;
            }
        }
        b0 = snap(bt, &cuts, at(bs), tol, f0, f1 - 8, bs);
        if b0 > n {
            return None;
        }
        b1 = snap(bt, &cuts, at(b0) + want, tol, b0 + 8, f1, b0);
        label = "盛り上がり中心に";
    } else {
        // head / auto
        b0 = if pos == "auto" { base_use.bar0 } else { f0 };
        if b0 < 0 || b0 > n {
            return None;
        }
        b1 = snap(bt, &cuts, at(b0) + want, tol, b0 + 8, f1, b0);
        label = if pos == "head" { "頭から" } else { "" };
    }
    // Python 版では範囲外（IndexError）になる組み合わせ。作れないものとして扱う
    if b0 < 0 || b1 > n || b0 > n || b1 < 0 {
        return None;
    }
    let min_len = if is_short(Some(want)) { MIN_SEC.min(SHORT_MIN_SEC.max(0.6 * want)) } else { MIN_SEC };
    if at(b1) - at(b0) < min_len {
        return None;
    }
    let (b0, b1) = desc::stable_bounds(&l.no_beat, bt, b0 as usize, b1 as usize, Some(&mut notes));
    // 長さの説明は、拍の取れる所へずらした後の長さで
    notes.insert(0, format!("{}{}", label, len_label(bt[b1] - bt[b0])));
    let u = Use { t0: np::round(bt[b0], 3), t1: np::round(bt[b1], 3), bar0: b0 as i64, bar1: b1 as i64, notes: np::dedup(notes) };
    Some((u, l.clone()))
}

/// 要約 base の use / in / out / vocals を、長さ length（秒。None ならおまかせ）・位置 pos の範囲に差し替える値。
pub fn variant(
    tid: i64,
    base: &Desc,
    length: Option<f64>,
    pos: Option<&str>,
    t0: Option<f64>,
    t1: Option<f64>,
) -> Option<Variant> {
    let has_len = matches!(length, Some(x) if x != 0.0);
    if !has_len && matches!(pos, None | Some("auto")) {
        return None;
    }
    let p = match pos {
        Some(s) if !s.is_empty() => s,
        _ => "auto",
    };
    let (use_, l) = bounds(tid, &base.use_, length, p, t0, t1)?;
    let mut seg = Seg { len: length, pos: p.to_string(), t0: None, t1: None };
    if pos == Some("custom") {
        seg.t0 = Some(use_.t0);
        seg.t1 = Some(use_.t1);
    }
    let vocals = l.stems.as_ref().map(|st| {
        let lo = (use_.bar0.max(0) as usize).min(st.len());
        let hi = (use_.bar1.max(0) as usize).min(st.len());
        st[lo..hi.max(lo)].iter().map(|r| np::round(r[0] as f64, 2)).collect()
    });
    Some(Variant {
        in_: desc::region(&l.no_beat, &l.bt, &l.feats, &use_, "in"),
        out: desc::region(&l.no_beat, &l.bt, &l.feats, &use_, "out"),
        use_,
        vocals,
        seg,
    })
}

/// 結果の曲ごとの「区間」の候補（おまかせ・頭から・盛り上がり中心・終わりまで・曲全体）。同じ範囲になるものはまとめる。
pub fn choices(tid: i64, base: &Desc, length: Option<f64>) -> Vec<Choice> {
    let has_len = matches!(length, Some(x) if x != 0.0);
    let mut out = vec![];
    let mut seen: Vec<(f64, f64)> = vec![];
    for (pos, label) in POSITIONS {
        let use_ = if pos == "auto" && !has_len {
            base.use_.clone()
        } else {
            match bounds(tid, &base.use_, length, pos, None, None) {
                Some((u, _)) => u,
                None => continue,
            }
        };
        let key = (np::round(use_.t0, 1), np::round(use_.t1, 1));
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(Choice { pos: pos.into(), len: length, label: label.into(), use_ });
    }
    out
}
