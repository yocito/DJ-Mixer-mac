//! 相性と並びの探索（djmix/planner/engine.py。design_phase3.md §4）。試作 work/phase3/planner_proto.py を本体にしたもの。
//!
//! 候補の曲どうしの相性を行列でまとめて計算し（rayon で行ごとに並べて）、ビームサーチはその行列を引くだけにする。
//! 1 万曲の解析済みライブラリでも、候補を 600 曲に絞れば 3 案をすぐに返せる。
//!
//! 戻り値の JSON は Python 版と同じキー・同じ型（整数は整数、丸めも同じ）。画面（frontend/ui/auto-view.js ほか）がそのまま使う。
//! Python 版で ValueError になる所は `PlanError`（サーバーは 400 で返す）。
use super::desc::{self, np, Desc, Use};
use super::genres;
use super::segments::{self, Seg};
use crate::analysis::cache;
use crate::analysis::cutpoints;
use crate::analysis::types::{CutPoint, CutPoints};
use crate::{db, prefs, settings};
use once_cell::sync::Lazy;
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

// 相性の配分（打合せ14・試作）
pub const W_TEMPO: f64 = 0.30;
pub const W_KEY: f64 = 0.20;
pub const W_CHROMA: f64 = 0.10;
pub const W_BEAT: f64 = 0.15;
pub const W_MOOD: f64 = 0.25;
// 並びの評価: 相性と流れ
pub const W_PAIR: f64 = 0.75;
pub const W_FLOW: f64 = 0.25;
pub const SAME_ARTIST_PENALTY: f64 = 0.15;
// アルバムで選ぶ（2026-10-25）: 同じアルバムが続くのを避け、まだ使っていないアルバムを優先する（最後に、入らなかったアルバムの曲を差し込む）
pub const SAME_ALBUM_PENALTY: f64 = 0.12;
pub const ALBUM_COVER_BONUS: f64 = 0.15;
pub const MIN_ALBUMS: usize = 3;
pub const MAX_ALL_TRACKS: usize = 200;
pub const TPL_BARS: [(&str, i64); 4] = [("long", 16), ("short", 8), ("echo", 2), ("cut", 0)];
/// つなぎ方（design_cutin.md）: 'auto' = おまかせ（拍を合わせられない組み合わせは、切れ目が良ければカットイン）、'mix' = 重ねて繋ぐ、'cut' = カットイン
pub const JOIN_MODES: [&str; 3] = ["auto", "mix", "cut"];
/// カットインの切り替えポイントを、使う範囲の終わりの何小節前〜後から探すか
pub const CUT_SEARCH_OUT: (i64, i64) = (-8, 4);
/// 次の曲の入りを、使う範囲の頭の何小節前〜後から探すか
pub const CUT_SEARCH_IN: (i64, i64) = (-4, 8);
/// 切り替えポイントへ寄せても、使う範囲は 60 秒以上残す
pub const CUT_MIN_USE: f64 = 60.0;
pub const MAX_CANDS: usize = 600;
pub const BEAM: usize = 60;
pub const FLOWS: [&str; 4] = ["up", "keep", "peak", "down"];
pub const FLOW_NAMES: [(&str, &str); 4] = [("up", "上げていく"), ("keep", "キープ"), ("peak", "山を作る"), ("down", "落ち着かせる")];
/// テンポランプを使うテンポの差（log。design_phase4.md §2.1）
pub const RAMP_MIN: f64 = 0.06;
pub const RAMP_MAX: f64 = 0.15;
/// ボーカルがある小節（analysis/stems.rs と同じ）
pub const VOCAL_ON: f64 = 0.25;
/// ボーカルの重なりの減点（重なる小節の割合 × これ。design_phase4.md §3.2）
pub const W_VOCAL: f64 = 0.15;
/// 相性で比べる小節数（繋ぐ部分）
pub const VBARS: usize = 16;
/// よく聴く曲を少し優先（design_phase5.md §E-3。再生回数の対数を 95 パーセンタイルで 0..1 に）
pub const PLAYS_BONUS: f64 = 0.03;

/// ★ の多い曲を優先（design_phase4.md §1.5）
pub fn rating_bonus(r: i64) -> f64 {
    match r {
        5 => 0.05,
        4 => 0.025,
        2 => -0.025,
        1 => -0.05,
        _ => 0.0,
    }
}

pub fn tpl_bars(tpl: &str) -> i64 {
    TPL_BARS.iter().find(|(k, _)| *k == tpl).map(|(_, v)| *v).unwrap_or(0)
}

/// Python 版の ValueError にあたる（入力が足りない・選べる曲が無いなど。サーバーは 400 で返す）
#[derive(Debug, Clone, PartialEq)]
pub struct PlanError(pub String);

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PlanError {}

fn fail<T>(msg: impl Into<String>) -> Result<T, PlanError> {
    Err(PlanError(msg.into()))
}

// ---------------------------------------------------------------- キー（Camelot）
fn camelot(s: &str) -> (i64, char) {
    let l = s.chars().last().unwrap_or(' ');
    let num = &s[..s.len() - l.len_utf8()];
    (num.trim().parse().unwrap_or(0), l)
}

fn key_rel_p((n1, l1): (i64, char), (n2, l2): (i64, char)) -> (f64, &'static str) {
    let dn = (n1 - n2).rem_euclid(12).min((n2 - n1).rem_euclid(12));
    if dn == 0 && l1 == l2 {
        return (1.0, "同じキー");
    }
    if (dn == 1 && l1 == l2) || (dn == 0 && l1 != l2) {
        return (0.85, "隣のキー");
    }
    if dn == 2 && l1 == l2 {
        return (0.5, "2つ隣のキー");
    }
    (0.15, "キーが離れている")
}

/// 2 つのキーの相性（1 / 0.85 / 0.5 / 0.15）と説明
pub fn key_rel(k1: &str, k2: &str) -> (f64, &'static str) {
    key_rel_p(camelot(k1), camelot(k2))
}

pub static CAMELOTS: Lazy<Vec<String>> =
    Lazy::new(|| (1..=12).flat_map(|n| ["A", "B"].into_iter().map(move |l| format!("{n}{l}"))).collect());

/// Camelot 表記を s 半音ずらす（1 半音で円の上を 7 つ進む。transition.js の shiftCamelot と同じ）。
pub fn shift_camelot(c: &str, s: i64) -> String {
    if s == 0 {
        return c.to_string();
    }
    let (n, l) = camelot(c);
    format!("{}{}", ((n - 1 + 7 * s).rem_euclid(12) + 12).rem_euclid(12) + 1, l)
}

fn z(v: &[f64]) -> Vec<f64> {
    let m = np::mean(v);
    let s = np::std(v) + 1e-9;
    v.iter().map(|x| (x - m) / s).collect()
}

/// 並びの中で「同じアーティスト」とみなす形（Python の strip().lower()）
fn norm_artist_lower(a: Option<&str>) -> String {
    a.unwrap_or("").trim().to_lowercase()
}

/// アーティストの比べ方（大文字小文字・前後の空白は区別しない。Python の casefold）
fn norm_artist(a: Option<&str>) -> String {
    a.unwrap_or("").trim().to_lowercase()
}

fn tim_dist(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

// ---------------------------------------------------------------- 曲の集まり
/// 曲 id → 要約（Matrix が見る曲。★・1 曲あたりの長さの差し替えが入ったもの）
pub type View = HashMap<i64, Arc<Desc>>;

/// 解析済みの曲の要約と、曲どうしを比べるための標準化した値（ライブラリ全体で標準化）。
pub struct Pool {
    pub d: HashMap<i64, Arc<Desc>>,
    /// 読んだ順（Python の dict の順。候補の順・同点のときの順に効く）
    pub ids: Vec<i64>,
    /// 最後に apply_prefs で読んだ ★（Python 版は要約に書き込む。作り直すまで残る）
    ratings: RwLock<HashMap<i64, i64>>,
}

impl Pool {
    pub fn new(descs: Vec<Desc>) -> Pool {
        let mut ids: Vec<i64> = vec![];
        let mut map: HashMap<i64, Desc> = HashMap::new();
        for d in descs {
            if !map.contains_key(&d.id) {
                ids.push(d.id);
            }
            map.insert(d.id, d);
        }
        if !ids.is_empty() {
            let cen: Vec<f64> = ids.iter().map(|t| map[t].extra.centroid.max(1.0).ln()).collect();
            let low: Vec<f64> = ids.iter().map(|t| map[t].extra.low_ratio).collect();
            let ons: Vec<f64> = ids.iter().map(|t| map[t].extra.onset_rate).collect();
            let bpm: Vec<f64> = ids.iter().map(|t| map[t].bpm.max(1.0).ln()).collect();
            let (zc, zl, zo, zb) = (z(&cen), z(&low), z(&ons), z(&bpm));
            for (k, t) in ids.iter().enumerate() {
                let raw = 0.5 * zo[k] + 0.3 * zc[k] + 0.2 * zb[k];
                let d = map.get_mut(t).unwrap();
                // 0..1（極端な曲に引っぱられないよう、なだらかに）
                d.energy = 1.0 / (1.0 + (-1.2 * raw).exp());
                d.timbre = [zc[k], zl[k], zo[k]];
                d.group = genres::group_of(d.genre.as_deref());
            }
        }
        Pool { d: map.into_iter().map(|(k, v)| (k, Arc::new(v))).collect(), ids, ratings: RwLock::new(HashMap::new()) }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn contains(&self, t: i64) -> bool {
        self.d.contains_key(&t)
    }

    pub fn rating(&self, t: i64) -> i64 {
        self.ratings.read().get(&t).copied().unwrap_or(0)
    }

    /// ids の要約（★ を入れたもの）
    pub fn view(&self, ids: &[i64]) -> View {
        let r = self.ratings.read();
        let mut v = View::new();
        for &t in ids {
            if v.contains_key(&t) {
                continue;
            }
            if let Some(d) = self.d.get(&t) {
                let mut d = (**d).clone();
                d.rating = r.get(&t).copied().unwrap_or(0);
                v.insert(t, Arc::new(d));
            }
        }
        v
    }
}

/// 要約の表の (行数, 最大の rowid, 今の版の行数)
type PoolKey = (i64, i64, Option<i64>);
type PoolCache = Option<(PoolKey, Arc<Pool>)>;
static POOL: Lazy<Mutex<PoolCache>> = Lazy::new(|| Mutex::new(None));

/// 要約が増えたら作り直す（まとめて解析の途中でも最新の曲が候補に入る）。
pub fn get_pool() -> Arc<Pool> {
    // 今の版の要約の数も見る（版を上げて作り直している途中は、行の数は変わらずに中身だけ入れ替わるため。Phase 5 F で見つけた）
    let key: PoolKey = db::with(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*), COALESCE(MAX(rowid), 0), SUM(version = ?) FROM track_desc",
            [desc::DESC_VERSION],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    })
    .unwrap_or((0, 0, None));
    let mut g = POOL.lock();
    if let Some((k, p)) = &*g {
        if *k == key {
            return p.clone();
        }
    }
    let p = Arc::new(Pool::new(desc::load_all()));
    *g = Some((key, p.clone()));
    p
}

/// 次に get_pool を呼んだとき作り直す（要約の中身だけ変えたときなど）
pub fn invalidate_pool() {
    *POOL.lock() = None;
}

// ---------------------------------------------------------------- 相性の行列
/// 曲ごとの、相性の計算に使う値
struct Side {
    bpm: f64,
    tail: (i64, char),
    head: (i64, char),
    head_m1: (i64, char),
    head_p1: (i64, char),
    harm_o: f64,
    harm_i: f64,
    co: Vec<f64>,
    cn: Vec<f64>,
    nb_o: bool,
    nb_i: bool,
    tim: [f64; 3],
    vo: u32,
    vi: u32,
}

fn unit(v: &[f64]) -> Vec<f64> {
    let nrm = np::pairwise_f64(v.len(), &|i| v[i] * v[i]).sqrt() + 1e-9;
    v.iter().map(|x| x / nrm).collect()
}

/// 前の曲の最後の 16 小節・次の曲の最初の 16 小節でボーカルがある小節（ビット）
fn vmask(v: Option<&Vec<f64>>, side_out: bool) -> u32 {
    let mut m = 0u32;
    if let Some(v) = v.filter(|v| !v.is_empty()) {
        if side_out {
            let seg = &v[v.len().saturating_sub(VBARS)..];
            let off = VBARS - seg.len();
            for (k, x) in seg.iter().enumerate() {
                if *x > VOCAL_ON {
                    m |= 1 << (off + k);
                }
            }
        } else {
            for (k, x) in v.iter().take(VBARS).enumerate() {
                if *x > VOCAL_ON {
                    m |= 1 << k;
                }
            }
        }
    }
    m
}

/// a→b の相性の内訳（Python の Matrix の各行列の 1 要素）
#[derive(Debug, Clone, Copy)]
pub struct Pair {
    pub rate: f64,
    pub k: f64,
    pub dlog: f64,
    pub ts: f64,
    pub ks: f64,
    pub cos: f64,
    pub beat: bool,
    pub ramp: bool,
    pub mood: f64,
    pub vclash: f64,
    pub blend: bool,
    pub s: f64,
}

const TPL_ECHO: u8 = 0;
const TPL_SHORT: u8 = 1;
const TPL_LONG: u8 = 2;

fn tpl_name(c: u8) -> &'static str {
    match c {
        TPL_LONG => "long",
        TPL_SHORT => "short",
        _ => "echo",
    }
}

/// 繋ぎ方・相性・内訳・理由（Matrix::join の戻り値。並びが決まった後に _finish_joins で仕上げる）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct JoinParams {
    pub tempo: String,
    #[serde(rename = "loop")]
    pub loop_: i64,
    pub shift: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub parts: Option<String>,
    #[serde(rename = "cutTrim", skip_serializing_if = "Option::is_none", default)]
    pub cut_trim: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Join {
    pub tpl: String,
    pub bars: i64,
    pub score: f64,
    pub params: JoinParams,
    pub tempo: f64,
    pub key: f64,
    pub chroma: f64,
    pub mood: f64,
    pub why: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mode: Option<String>,
}

/// 候補の曲 ids の、あらゆる並び (a→b) の相性と内訳。
pub struct Matrix {
    pub ids: Vec<i64>,
    /// 曲 id → 行列の番号（同じ曲が 2 回あれば後の方。Python と同じ）
    pub pos: HashMap<i64, usize>,
    pub join_mode: String,
    /// パートごとに鳴らす繋ぎを選ぶか（設定 auto_parts）
    pub auto_parts: bool,
    /// Python の M.pool.d
    pub view: View,
    pub n: usize,
    /// 相性（n×n、行 = 前の曲）。対角は -1
    pub s: Vec<f64>,
    tplc: Vec<u8>,
    sides: Vec<Side>,
    pub energy: Vec<f64>,
    pub use_len: Vec<f64>,
    pub use_bars: Vec<i64>,
    pub short: Vec<bool>,
    pub bar_sec: Vec<f64>,
    pub artist: Vec<i64>,
}

impl Matrix {
    pub fn new(view: &View, ids: &[i64], join_mode: &str) -> Matrix {
        let join_mode = if JOIN_MODES.contains(&join_mode) { join_mode } else { "auto" }.to_string();
        let pos: HashMap<i64, usize> = ids.iter().enumerate().map(|(k, &t)| (t, k)).collect();
        let dd: Vec<Arc<Desc>> = ids.iter().map(|t| view[t].clone()).collect();
        let n = dd.len();
        let sides: Vec<Side> = dd
            .iter()
            .map(|d| {
                let head = camelot(&d.key_head);
                Side {
                    bpm: d.bpm,
                    tail: camelot(&d.key_tail),
                    head,
                    head_m1: camelot(&shift_camelot(&d.key_head, -1)),
                    head_p1: camelot(&shift_camelot(&d.key_head, 1)),
                    harm_o: d.out.harm,
                    harm_i: d.in_.harm,
                    co: unit(&d.out.chroma),
                    cn: unit(&d.in_.chroma),
                    nb_o: d.out.nobeat.v < 1.0,
                    nb_i: d.in_.nobeat.v < 1.0,
                    tim: d.timbre,
                    vo: vmask(d.vocals.as_ref(), true),
                    vi: vmask(d.vocals.as_ref(), false),
                }
            })
            .collect();
        let arts: Vec<String> = dd.iter().map(|d| norm_artist_lower(d.artist.as_deref())).collect();
        let mut uniq: Vec<&String> = arts.iter().collect();
        uniq.sort();
        uniq.dedup();
        let au: HashMap<&String, i64> = uniq.into_iter().enumerate().map(|(j, a)| (a, j as i64)).collect();
        let mut m = Matrix {
            ids: ids.to_vec(),
            pos,
            join_mode,
            auto_parts: true,
            view: view.clone(),
            n,
            s: vec![0.0; n * n],
            tplc: vec![0; n * n],
            energy: dd.iter().map(|d| d.energy).collect(),
            use_len: dd.iter().map(|d| d.use_.t1 - d.use_.t0).collect(),
            use_bars: dd.iter().map(|d| d.use_.bar1 - d.use_.bar0).collect(),
            // 「1 曲あたりの長さ」で短く（90 秒以下）を選んだ曲。重なりを曲の長さに合わせて短くする（ふつうの長さはループで延ばす。Phase 4）
            short: dd.iter().map(|d| segments::is_short(d.seg.as_ref().and_then(|s| s.len))).collect(),
            bar_sec: dd.iter().map(|d| 240.0 / d.bpm.max(1.0)).collect(),
            artist: arts.iter().enumerate().map(|(j, a)| if a.is_empty() { -1 - j as i64 } else { au[a] }).collect(),
            sides,
        };
        let mut s = std::mem::take(&mut m.s);
        let mut tc = std::mem::take(&mut m.tplc);
        if n > 0 {
            s.par_chunks_mut(n).zip(tc.par_chunks_mut(n)).enumerate().for_each(|(i, (row, trow))| {
                for j in 0..n {
                    let p = m.pair(i, j);
                    row[j] = p.s;
                    trow[j] = Self::tpl_of(&p);
                }
            });
        }
        m.s = s;
        m.tplc = tc;
        m
    }

    /// a→b（行列の番号）の相性と内訳
    pub fn pair(&self, i: usize, j: usize) -> Pair {
        let (a, b) = (&self.sides[i], &self.sides[j]);
        // テンポ: 倍/半を考慮して、速度の変化が一番小さい倍率
        let mut best = 1.0 * a.bpm / b.bpm;
        let mut kk = 1.0;
        for k in [2.0, 0.5] {
            let r = k * a.bpm / b.bpm;
            if r.ln().abs() < best.ln().abs() {
                best = r;
                kk = k;
            }
        }
        let dlog = best.ln().abs();
        let ts0 = if dlog < 0.03 { 1.0 } else { (1.0 - (dlog - 0.03) / 0.2).clamp(0.0, 1.0) };
        // キー（前の曲の末尾 30 秒と次の曲の冒頭 30 秒）。和音がうすい部分どうしなら影響を弱める
        // キー合わせ（Phase 4）: 次の曲を ±1 半音ずらすと合うなら、少し割り引いてその値を使う
        let mut ks0 = key_rel_p(a.tail, b.head).0;
        ks0 = ks0.max(0.95 * key_rel_p(a.tail, b.head_m1).0);
        ks0 = ks0.max(0.95 * key_rel_p(a.tail, b.head_p1).0);
        let hw = 1f64.min(3.0 * a.harm_o.min(b.harm_i));
        let ks = 1.0 - hw * (1.0 - ks0);
        // 繋ぐ部分の響き（クロマの近さ）
        let cos = a.co.iter().zip(&b.cn).map(|(x, y)| x * y).fold(0.0, |acc, v| acc + v);
        // 拍を合わせられるか
        let beat = a.nb_o && b.nb_i && dlog < 1.3f64.ln();
        // テンポランプ（Phase 4）: テンポの差が 6〜15% でも、重なりの間にテンポを移せば無理なく繋げる
        let ramp = beat && (RAMP_MIN..=RAMP_MAX).contains(&dlog);
        let ts = if ramp { ts0.max(0.8) } else { ts0 };
        // 音の雰囲気の近さ
        let mood = (1.0 - tim_dist(&a.tim, &b.tim) / 2.5).clamp(0.0, 1.0);
        // エコーアウトになる組み合わせは音が重ならないので、ボーカルの重なりを数えない
        let blend = beat && (ramp || ts > 0.4);
        // ボーカルの重なり（Phase 4 B1）: 前の曲の最後の 16 小節と次の曲の最初の 16 小節で、両方にボーカルがある小節の割合
        let vclash = if blend { (a.vo & b.vi).count_ones() as f64 / VBARS as f64 } else { 0.0 };
        // カットイン（design_cutin.md）は重ねないので、テンポの差・拍の取れなさはあまり効かない。
        // 'cut' はすべての繋ぎで、'auto' は重ねて繋げない組み合わせ（カットインで繋ぐ候補）で、テンポの点を底上げする
        let (ts_s, beat_s) = match self.join_mode.as_str() {
            "cut" => (ts.max(0.7), 1.0),
            "auto" => (if blend { ts } else { ts.max(0.4) }, if beat { 1.0 } else { 0.0 }),
            _ => (ts, if beat { 1.0 } else { 0.0 }),
        };
        let s = if i == j {
            -1.0
        } else {
            W_TEMPO * ts_s + W_KEY * ks + W_CHROMA * cos + W_BEAT * beat_s + W_MOOD * mood - W_VOCAL * vclash
        };
        Pair { rate: best, k: kk, dlog, ts, ks, cos, beat, ramp, mood, vclash, blend, s }
    }

    fn tpl_of(p: &Pair) -> u8 {
        if !p.beat {
            return TPL_ECHO;
        }
        if p.ramp {
            return TPL_LONG; // テンポランプは長く重ねてテンポを移す
        }
        if p.ts > 0.8 && p.ks > 0.6 {
            return TPL_LONG;
        }
        if p.ts > 0.4 {
            TPL_SHORT
        } else {
            TPL_ECHO
        }
    }

    #[inline]
    pub fn s_at(&self, i: usize, j: usize) -> f64 {
        self.s[i * self.n + j]
    }

    pub fn tpl(&self, i: usize, j: usize) -> &'static str {
        tpl_name(self.tplc[i * self.n + j])
    }

    /// a→b（曲 id）の繋ぎ方・相性・内訳・理由。
    pub fn join(&self, a: i64, b: i64) -> Join {
        let (i, j) = (self.pos[&a], self.pos[&b]);
        let (da, db_) = (&self.view[&a], &self.view[&b]);
        let p = self.pair(i, j);
        let mut tpl = self.tpl(i, j);
        let kname = key_rel(&da.key_tail, &db_.key_head).1;
        let kx = if p.k != 1.0 { format!("・×{}", if p.k == 2.0 { "2" } else { "½" }) } else { String::new() };
        let mut why = vec![
            format!("テンポ {:.0}→{:.0}（{:+.1}%{}）", da.bpm, db_.bpm, (p.rate - 1.0) * 100.0, kx),
            format!("キー {}→{} {}", da.key_tail, db_.key_head, kname)
                + if p.ks > 0.6 && key_rel(&da.key_tail, &db_.key_head).0 < 0.85 { "（和音がうすく影響小）" } else { "" },
        ];
        if !p.beat {
            why.push("拍を合わせられないのでエコーアウト".into());
        }
        if p.ramp {
            why.push("テンポランプで繋ぐ（重なりの間にテンポを移す）".into());
        }
        if p.mood < 0.35 {
            why.push("音の雰囲気が違う".into());
        }
        if p.vclash > 0.0 {
            why.push(format!("ボーカルが重なる所がある（{} 小節）", np::round_int(p.vclash * VBARS as f64)));
        }
        let bars = self.ov_bars(i, j, tpl);
        if tpl == "long" && bars < 12 {
            tpl = "short"; // 曲が短くて長く重ねられない
            why.push(format!("曲が短いので重なりを {bars} 小節に"));
        } else if tpl == "short" && bars < tpl_bars("short") {
            why.push(format!("曲が短いので重なりを {bars} 小節に"));
        }
        Join {
            tpl: tpl.into(),
            bars,
            score: np::round(self.s_at(i, j), 3),
            params: JoinParams { tempo: if p.ramp { "ramp" } else { "sync" }.into(), loop_: 0, shift: 0, parts: None, cut_trim: None },
            tempo: np::round(p.ts, 2),
            key: np::round(p.ks, 2),
            chroma: np::round(p.cos, 2),
            mood: np::round(p.mood, 2),
            why,
            mode: None,
        }
    }

    /// 重なりの小節数: 繋ぎ方の標準（16/8/2）を、2 曲の短い方の使う範囲の 1/3 までに抑える（4 小節単位。短くたくさん繋ぐとき。2026-10-25）。
    pub fn ov_bars(&self, i: usize, j: usize, tpl: &str) -> i64 {
        let base = tpl_bars(tpl);
        if tpl == "echo" || tpl == "cut" || !(self.short[i] || self.short[j]) {
            return base;
        }
        let cap = self.use_bars[i].min(self.use_bars[j]).div_euclid(3).div_euclid(4) * 4;
        4.max(base.min(cap))
    }

    pub fn overlap_sec(&self, i: usize, j: usize) -> f64 {
        let tpl = self.tpl(i, j);
        self.ov_bars(i, j, tpl) as f64 * self.bar_sec[i]
    }
}

// ---------------------------------------------------------------- 探索
/// 流れ（エネルギーの目標）。x = 0..1（並びの中の位置）、e0 = 起点の曲のエネルギー
#[derive(Debug, Clone)]
pub enum Flow {
    Up,
    Keep,
    Peak,
    Down,
    /// 自分で描いた流れ（P3-9）: 0..1 のエネルギーを時間に等間隔で並べたもの
    Curve { grid: Vec<f64>, c: Vec<f64> },
}

impl Flow {
    pub fn named(name: &str) -> Option<Flow> {
        match name {
            "up" => Some(Flow::Up),
            "keep" => Some(Flow::Keep),
            "peak" => Some(Flow::Peak),
            "down" => Some(Flow::Down),
            _ => None,
        }
    }

    pub fn at(&self, x: f64, e0: f64) -> f64 {
        match self {
            Flow::Up => e0 + (1f64.min(e0 + 0.45) - e0) * x,
            Flow::Keep => e0,
            Flow::Peak => e0 + 0.4 * (std::f64::consts::PI * x).sin(),
            Flow::Down => e0 - (e0 - 0f64.max(e0 - 0.45)) * x,
            Flow::Curve { grid, c } => np::interp(x, grid, c),
        }
    }
}

/// 自分で描いた流れ（design_phase3.md §7。0..1 のエネルギーを時間に等間隔で並べたもの）→ 流れ。起点の曲のエネルギーには合わせない。
pub fn curve_flow(curve: &[f64]) -> Flow {
    let c: Vec<f64> = curve.iter().map(|v| v.clamp(0.0, 1.0)).collect();
    Flow::Curve { grid: np::linspace01(c.len()), c }
}

/// Python の `curve_flow(curve) if flow == "custom" and curve else flow`（知らない流れは KeyError と同じ文言）
fn resolve_flow(flow: &str, curve: Option<&[f64]>) -> Result<Flow, PlanError> {
    if flow == "custom" {
        if let Some(c) = curve.filter(|c| !c.is_empty()) {
            return Ok(curve_flow(c));
        }
    }
    Flow::named(flow).ok_or_else(|| PlanError(format!("'{flow}'")))
}

/// ビームサーチの状態 = (合計点, 並び(行列の番号), 長さ秒)
pub type Beam = (f64, Vec<usize>, f64);

/// ビームサーチの設定（Python の _beam の省略できる引数）
pub struct BeamOpts<'a> {
    pub avoid: &'a [f64],
    pub allowed: Option<&'a [bool]>,
    pub width: usize,
    pub flow_w: f64,
    pub bonus: Option<&'a [f64]>,
    pub album: Option<&'a [usize]>,
}

impl<'a> BeamOpts<'a> {
    pub fn new(avoid: &'a [f64]) -> Self {
        BeamOpts { avoid, allowed: None, width: BEAM, flow_w: W_FLOW, bonus: None, album: None }
    }
}

/// ビームサーチ。stop(seq, length) が真なら完了。
pub fn beam(m: &Matrix, start: usize, stop: &dyn Fn(&[usize], f64) -> bool, flow: &Flow, target_sec: f64, o: &BeamOpts) -> Vec<Beam> {
    let n = m.n;
    let e0 = m.energy[start];
    let mut beams: Vec<Beam> = vec![(0.0, vec![start], m.use_len[start])];
    let mut done: Vec<Beam> = vec![];
    let base_mask: Vec<bool> = o.allowed.map(|a| a.to_vec()).unwrap_or_else(|| vec![true; n]);
    let n_alb = o.album.map(|a| a.iter().copied().max().map(|x| x + 1).unwrap_or(0)).unwrap_or(0);
    let mut tot = vec![0f64; n];
    for _ in 0..200 {
        let mut cand: Vec<Beam> = vec![];
        for (score, seq, length) in &beams {
            let last = *seq.last().unwrap();
            let mut mask = base_mask.clone();
            for &s in seq {
                mask[s] = false;
            }
            if !mask.iter().any(|&b| b) {
                continue;
            }
            let row = &m.s[last * n..(last + 1) * n];
            let recent: Vec<i64> = seq[seq.len().saturating_sub(2)..].iter().map(|&s| m.artist[s]).collect();
            let mut used = vec![false; n_alb];
            if let Some(alb) = o.album {
                for &s in seq {
                    used[alb[s]] = true;
                }
            }
            let tdiv = 1f64.max(target_sec);
            let mut finite: Vec<usize> = Vec::with_capacity(n);
            for j in 0..n {
                if !mask[j] {
                    continue;
                }
                let x = 1f64.min((length + m.use_len[j] / 2.0) / tdiv);
                let ef = if o.flow_w != 0.0 { 1.0 - 1f64.min((m.energy[j] - flow.at(x, e0)).abs() / 0.35) } else { 0.0 };
                let b = o.bonus.map(|b| b[j]).unwrap_or(0.0);
                let mut t = W_PAIR * (row[j] + b) + o.flow_w * ef - o.avoid[j];
                t -= SAME_ARTIST_PENALTY * (recent.contains(&m.artist[j]) as i64 as f64);
                if let Some(alb) = o.album {
                    // アルバムで選ぶ: 同じアルバムが続かない・まだ使っていないアルバムを優先
                    t = t - SAME_ALBUM_PENALTY * ((alb[j] == alb[last]) as i64 as f64)
                        + ALBUM_COVER_BONUS * x * ((!used[alb[j]]) as i64 as f64);
                }
                tot[j] = t;
                finite.push(j);
            }
            // 点の高い 12 曲（np.argpartition と同じ集まり。同点のときの選び方だけは違うことがある）
            if finite.len() > 12 {
                finite.select_nth_unstable_by(11, |&p, &q| tot[q].partial_cmp(&tot[p]).unwrap_or(std::cmp::Ordering::Equal));
                finite.truncate(12);
                finite.sort_unstable();
            }
            for &j in &finite {
                let ov = m.overlap_sec(last, j);
                let mut s2 = seq.clone();
                s2.push(j);
                cand.push((score + tot[j], s2, length + m.use_len[j] - ov));
            }
        }
        if cand.is_empty() {
            if done.is_empty() {
                done = beams; // 候補を使い切っても目標に届かない（アルバムで選ぶなど）: そこまでの並びを使う
            }
            break;
        }
        let mut keyed: Vec<(f64, Beam)> = cand.into_iter().map(|b| (-b.0 / b.1.len() as f64, b)).collect();
        keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        beams = vec![];
        let mut seen: HashSet<(usize, Vec<usize>)> = HashSet::new();
        for (_, b) in keyed {
            let mut set = b.1.clone();
            set.sort_unstable();
            if !seen.insert((*b.1.last().unwrap(), set)) {
                continue;
            }
            if stop(&b.1, b.2) {
                done.push(b);
            } else {
                beams.push(b);
            }
            if beams.len() >= o.width {
                break;
            }
        }
        if beams.is_empty() {
            break;
        }
    }
    let mut keyed: Vec<(f64, Beam)> = done.into_iter().map(|b| (-b.0 / (b.1.len().max(2) - 1) as f64, b)).collect();
    keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    keyed.into_iter().map(|(_, b)| b).collect()
}

/// 結果の 1 曲
#[derive(Debug, Clone, Serialize, Default)]
pub struct TrackRow {
    pub id: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub rating: i64,
    pub bpm: f64,
    pub key: String,
    pub energy: f64,
    pub duration: f64,
    #[serde(rename = "use")]
    pub use_: Use,
    pub seg: Seg,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join: Option<Join>,
}

/// 1 案（{score, minutes, tracks}）
#[derive(Debug, Clone, Serialize)]
pub struct Packed {
    pub score: f64,
    pub minutes: f64,
    pub tracks: Vec<TrackRow>,
}

impl Packed {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

pub fn pack(m: &Matrix, seq: &[usize], with_env: bool, modes: &HashMap<usize, String>) -> Packed {
    let ids: Vec<i64> = seq.iter().map(|&k| m.ids[k]).collect();
    let mut tracks: Vec<TrackRow> = vec![];
    let mut length = 0.0;
    for (n, (&k, &tid)) in seq.iter().zip(&ids).enumerate() {
        let d = &m.view[&tid];
        let mut use_ = d.use_.clone();
        let seg = d.seg.clone().unwrap_or_default();
        // ミックスの最初の曲は前から繋ぐ曲が無いので、短い前奏（拍の取れない所・最初の小節頭の前）を飛ばさず冒頭（0:00）から鳴らす
        // 短くたくさん繋ぐとき（1 曲 90 秒以下）は盛り上がりの所を使うので、冒頭からにはしない
        let pos = d.seg.as_ref().map(|s| s.pos.as_str()).unwrap_or("auto");
        if n == 0
            && (pos == "auto" || pos == "head")
            && 0.0 < use_.t0
            && use_.t0 <= segments::FIRST_FROM_TOP_SEC
            && !segments::is_short(d.seg.as_ref().and_then(|s| s.len))
        {
            use_.t0 = 0.0;
            use_.notes.retain(|x| !x.contains("イントロ") && !x.contains("始める"));
            use_.notes.push("最初の曲なので冒頭から".into());
        }
        let mut row = TrackRow {
            id: tid,
            title: d.title.clone(),
            artist: d.artist.clone(),
            album: d.album.clone(),
            genre: d.genre.clone(),
            rating: d.rating,
            bpm: d.bpm,
            key: d.key.clone(),
            energy: np::round(d.energy, 3),
            duration: d.duration,
            use_,
            seg,
            env: None,
            join: None,
        };
        if with_env {
            row.env = Some(envelope(tid, &row.use_));
        }
        if n + 1 < seq.len() {
            row.join = Some(m.join(tid, ids[n + 1]));
            length += m.use_len[k] - m.overlap_sec(k, seq[n + 1]);
        } else {
            length += m.use_len[k];
        }
        tracks.push(row);
    }
    finish_joins(m, &mut tracks, modes);
    let pairs: Vec<f64> = seq.windows(2).map(|w| m.s_at(w[0], w[1])).collect();
    Packed {
        score: if pairs.is_empty() { 0.0 } else { np::round(np::mean(&pairs), 3) },
        minutes: np::round(length / 60.0, 1),
        tracks,
    }
}

/// 並びが決まった後の繋ぎの仕上げ（Phase 4。design_phase4.md §2.1）:
/// キー合わせ — 前の曲のずらし（あれば）を引き継いだ末尾のキーと、次の曲の冒頭のキーが「同じ／隣」でなければ、次の曲を ±1 半音ずらして合うか試す。
/// ループ — 前の曲の使う範囲が短い（重なり＋8 小節未満）なら、最後の 4 小節を繰り返して重なりを取る。
pub fn finish_joins(m: &Matrix, tracks: &mut [TrackRow], modes: &HashMap<usize, String>) {
    let mut shift_out = 0i64;
    for n in 0..tracks.len().saturating_sub(1) {
        let (left, right) = tracks.split_at_mut(n + 1);
        let (ta, tb) = (&mut left[n], &mut right[0]);
        let a = m.view[&ta.id].clone();
        let b = m.view[&tb.id].clone();
        let Some(mut j) = ta.join.take() else { continue };
        // カットインにするか（つなぎ目ごとの指定 → 全体のつなぎ方 → おまかせ）。カットインはキー合わせ・歌・パート・ループを使わない
        let mode = modes.get(&n).filter(|s| !s.is_empty()).map(|s| s.as_str()).unwrap_or(m.join_mode.as_str());
        if (mode == "cut" || (mode == "auto" && j.tpl == "echo")) && make_cut(ta, tb, &mut j, mode == "cut") {
            shift_out = 0;
            ta.join = Some(j);
            continue;
        }
        j.mode = Some("mix".into());
        let tail = shift_camelot(&a.key_tail, shift_out);
        let base = key_rel(&tail, &b.key_head).0;
        let mut best = 0i64;
        if base < 0.85 && j.tpl != "echo" {
            for s in [1, -1] {
                if key_rel(&tail, &shift_camelot(&b.key_head, s)).0 >= 0.85 {
                    best = s;
                    break;
                }
            }
        }
        j.params.shift = best;
        if best != 0 {
            j.why.retain(|w| !w.starts_with("キー "));
            j.why.push(format!("キー {}→{}（次の曲を {:+} 半音ずらして合わせる）", tail, shift_camelot(&b.key_head, best), best));
        } else if shift_out != 0 {
            j.why.retain(|w| !w.starts_with("キー "));
            j.why.push(format!("キー {}→{} {}", tail, b.key_head, key_rel(&tail, &b.key_head).1));
        }
        avoid_vocal_clash(ta, tb, &a, &b, &mut j);
        choose_parts(m.auto_parts, &a, &b, &mut j);
        let use_bars = ta.use_.bar1 - ta.use_.bar0;
        if (j.tpl == "long" || j.tpl == "short") && use_bars < j.bars + 8 && !segments::is_short(ta.seg.len) {
            j.params.loop_ = 4;
            j.why.push("前の曲の範囲が短いので、最後の 4 小節を繰り返して重ねる".into());
        }
        shift_out = best;
        ta.join = Some(j);
    }
}

/// center の前後 [lo, hi] 小節の候補から、点数が高く center に近いもの。
fn cut_pick(cands: &[CutPoint], center: i64, lo: i64, hi: i64, ok: impl Fn(&CutPoint) -> bool) -> Option<&CutPoint> {
    let mut best: Option<(f64, &CutPoint)> = None;
    for c in cands {
        let d = c.bar as i64 - center;
        if lo <= d && d <= hi && ok(c) {
            let v = c.score - 0.12 * d.abs() as f64;
            if best.map(|b| v > b.0).unwrap_or(true) {
                best = Some((v, c));
            }
        }
    }
    best.map(|b| b.1)
}

/// a→b をカットインにする（design_cutin.md ②）: 前の曲の終わりを「キレイに切れる小節の頭」へ、次の曲の頭を「入りの良い小節の頭」へ寄せる。
/// おまかせ（forced=false）は、良い切り替えポイントが見つかったときだけカットインにする（無ければエコーアウトのまま）。
pub fn make_cut(ta: &mut TrackRow, tb: &mut TrackRow, j: &mut Join, forced: bool) -> bool {
    let ca = cutpoints::for_track(ta.id);
    let cb = cutpoints::for_track(tb.id);
    make_cut_with(ta, tb, j, forced, ca.as_deref(), cb.as_deref())
}

/// make_cut の本体（切り替えポイントの候補を渡す）
pub fn make_cut_with(
    ta: &mut TrackRow,
    tb: &mut TrackRow,
    j: &mut Join,
    forced: bool,
    ca: Option<&CutPoints>,
    cb: Option<&CutPoints>,
) -> bool {
    let good = matches!((ca, cb), (Some(a), Some(b)) if !a.outs.is_empty() && !b.ins.is_empty());
    if !good && !forced {
        return false;
    }
    let empty = CutPoints::default();
    let (ca, cb) = (ca.unwrap_or(&empty), cb.unwrap_or(&empty));
    let mut ua = ta.use_.clone();
    let mut ub = tb.use_.clone();
    let keep_a = ta.seg.pos == "custom"; // 自分で選んだ区間は動かさない
    let keep_b = tb.seg.pos == "custom";
    let (nbt_a, nbt_b) = (ca.bars.len() as i64, cb.bars.len() as i64);
    // 使う範囲は 60 秒（短い曲は今の長さの 6 割）以上残す
    let keep_ua = CUT_MIN_USE.min(0.6 * (ua.t1 - ua.t0));
    let keep_ub = CUT_MIN_USE.min(0.6 * (ub.t1 - ub.t0));
    let mut co = if keep_a {
        None
    } else {
        cut_pick(&ca.outs, ua.bar1, CUT_SEARCH_OUT.0, CUT_SEARCH_OUT.1, |c| c.t - ua.t0 >= keep_ua)
    };
    let mut ci = if keep_b {
        None
    } else {
        cut_pick(&cb.ins, ub.bar0, CUT_SEARCH_IN.0, CUT_SEARCH_IN.1, |c| ub.t1 - c.t >= keep_ub)
    };
    if !forced
        && (co.map(|c| c.score < cutpoints::GOOD).unwrap_or(true) || ci.map(|c| c.score < 2.0).unwrap_or(true))
    {
        return false;
    }
    let mut tags: Vec<String> = vec![];
    match co {
        Some(c) if c.bar as i64 != ua.bar1 => {
            ua.bar1 = c.bar as i64;
            ua.t1 = np::round(c.t, 3);
            ua.notes.push("カットインの切り替えポイントで終える".into());
            ta.use_ = ua.clone();
        }
        _ => {
            if nbt_a > 0 && ua.bar1 < nbt_a {
                co = ca.outs.iter().find(|c| c.bar as i64 == ua.bar1);
            }
        }
    }
    if let Some(c) = co {
        tags.extend(c.tags.iter().cloned());
    }
    match ci {
        Some(c) if c.bar as i64 != ub.bar0 => {
            ub.bar0 = c.bar as i64;
            ub.t0 = np::round(c.t, 3);
            ub.notes.push("カットインの入りから始める".into());
            tb.use_ = ub.clone();
        }
        _ => {
            if nbt_b > 0 && ub.bar0 < nbt_b {
                ci = cb.ins.iter().find(|c| c.bar as i64 == ub.bar0);
            }
        }
    }
    j.tpl = "cut".into();
    j.bars = 0;
    j.mode = Some("cut".into());
    j.params.tempo = "sync".into();
    j.params.loop_ = 0;
    j.params.shift = 0;
    j.params.parts = Some("none".into());
    j.params.cut_trim = Some("auto".into());
    j.why.retain(|w| !(w.starts_with("拍を合わせられない") || w.starts_with("テンポランプ") || w.starts_with("ボーカルが重なる")));
    let mut msg = "カットイン（前の曲の小節の頭で切り、次の曲を 1 拍目から。音量は自動で合わせる）".to_string();
    if !tags.is_empty() || ci.is_some() {
        let mut all = tags;
        if let Some(c) = ci {
            all.extend(c.tags.iter().cloned());
        }
        msg += &format!("：{}", np::dedup(all).join("・"));
    }
    j.why.push(msg);
    true
}

/// パートごとに鳴らす繋ぎ（Phase 5 B2。design_phase5.md §B2）を選ぶ。設定 auto_parts が入のときだけ。
/// ロングブレンド → 入れ替え（前の曲の歌を先に抜き、次の曲はドラムから。ベースは入れ替えの位置で交代）、
/// ショートブレンドで歌がまだ重なる → 前の曲の歌を先に抜く。分離した音は、再生・書き出しのときに用意する（1 曲 20 秒ほど）。
pub fn choose_parts(auto_parts: bool, a: &Desc, b: &Desc, j: &mut Join) {
    if j.params.parts.is_none() {
        j.params.parts = Some("none".into());
    }
    if !auto_parts || !(j.tpl == "long" || j.tpl == "short") || j.bars < 8 {
        return; // 4 小節の重なりでは、パートを順に入れる時間が無い
    }
    let va = a.vocals.as_deref().filter(|v| !v.is_empty());
    let vb = b.vocals.as_deref().filter(|v| !v.is_empty());
    if j.tpl == "long" && j.bars >= 12 {
        j.params.parts = Some("swap".into());
        j.why.push("パートごとに入れ替える（前の曲の歌を先に抜き、次の曲はドラムから）".into());
    } else if let (Some(va), Some(vb)) = (va, vb) {
        if best_vocal_fit(va, vb, j.bars, 16, 8).clash > 0 {
            j.params.parts = Some("vocal".into());
            j.why.push("歌が重なるので、前の曲の歌を先に抜く".into());
        }
    }
}

/// best_vocal_fit の戻り値 {bars, trim, clash, clash0}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VocalFit {
    pub bars: i64,
    pub trim: i64,
    pub clash: i64,
    pub clash0: i64,
    pub cost: f64,
}

/// ボーカルが重ならない重なりの長さと位置を探す（design_phase4.md §3.2、つなぎ目エディタの「ずらす」と同じ）。
/// vo: 前の曲の使う範囲の小節ごとのボーカルの量、vi: 次の曲の同じもの。前の曲の終わりを最大 max_trim 小節早め、重なりを 16/12/8 小節から選ぶ。
/// 点数 = 歌の重なり 1 小節 10 ＋ 前の曲を 1 小節削る 0.2 ＋ 重なりを 1 小節短くする 0.3。
pub fn best_vocal_fit(vo: &[f64], vi: &[f64], bars: i64, max_trim: i64, min_left: i64) -> VocalFit {
    let (lo, li) = (vo.len() as i64, vi.len() as i64);
    let clash = |trim: i64, n: i64| -> i64 {
        let end = lo - trim;
        (0..n)
            .filter(|&k| {
                let x = end - n + k;
                0 <= x && x < lo && k < li && vo[x as usize] > VOCAL_ON && vi[k as usize] > VOCAL_ON
            })
            .count() as i64
    };
    let c0 = clash(0, bars);
    let mut best = VocalFit { bars, trim: 0, clash: c0, clash0: c0, cost: (c0 * 10) as f64 };
    if c0 == 0 {
        return best;
    }
    let mut ns: Vec<i64> = [bars, 12, 8].into_iter().filter(|&x| x <= bars).collect();
    ns.sort_unstable_by(|a, b| b.cmp(a));
    ns.dedup();
    for n in ns {
        for trim in 0..=max_trim {
            if lo - trim < n + min_left {
                break;
            }
            let c = clash(trim, n);
            let cost = (c * 10) as f64 + trim as f64 * 0.2 + (bars - n) as f64 * 0.3;
            if cost < best.cost {
                best = VocalFit { bars: n, trim, clash: c, clash0: c0, cost };
            }
        }
    }
    best
}

/// 並びが決まった後: 歌もの同士のつなぎで、ボーカルの重なりの少ない位置・長さへ寄せる（前の曲の使う範囲の終わりと重なりの小節数を変える）。
pub fn avoid_vocal_clash(ta: &mut TrackRow, _tb: &mut TrackRow, a: &Desc, b: &Desc, j: &mut Join) {
    if !(j.tpl == "long" || j.tpl == "short" || j.tpl == "bass") {
        return;
    }
    let (Some(va), Some(vb)) = (a.vocals.as_deref().filter(|v| !v.is_empty()), b.vocals.as_deref().filter(|v| !v.is_empty()))
    else {
        return;
    };
    let ub = ta.use_.bar1 - ta.use_.bar0;
    let fit = best_vocal_fit(va, vb, j.bars, 16.min(ub.div_euclid(4)), 8.min(4.max(ub.div_euclid(2))));
    if fit.clash >= fit.clash0 || (fit.trim == 0 && fit.bars == j.bars) {
        return;
    }
    if fit.trim != 0 {
        let Some(ca) = cache::read(ta.id) else { return };
        let bt = ca.bar_edges();
        let mut u = ta.use_.clone();
        u.bar1 -= fit.trim;
        if u.bar1 < 0 || u.bar1 as usize >= bt.len() {
            return;
        }
        u.t1 = np::round(bt[u.bar1 as usize], 3);
        u.notes.push(format!("ボーカルの重なりを避けて {} 小節早く終える", fit.trim));
        ta.use_ = u;
    }
    j.bars = fit.bars;
    j.why.retain(|w| !w.starts_with("ボーカルが重なる"));
    j.why.push(format!(
        "ボーカルの重なりを {} → {} 小節に（重なり {} 小節{}）",
        fit.clash0,
        fit.clash,
        fit.bars,
        if fit.trim != 0 { format!("・前の曲を {} 小節早く終える", fit.trim) } else { String::new() }
    ));
}

/// タイムラインの帯に描く、使う範囲の小節ごとの音量（最大 160 点）。
pub fn envelope(tid: i64, use_: &Use) -> Vec<f64> {
    let Some(f) = cache::bars_array(tid) else { return vec![] };
    let nf = f.len() as i64;
    let lo = use_.bar0.clamp(0, nf) as usize;
    let hi = (use_.bar1.clamp(0, nf) as usize).max(lo);
    let mut v: Vec<f32> = f[lo..hi].iter().map(|r| r[0]).collect();
    if v.len() > 160 {
        let step = v.len() / 160 + 1;
        v.truncate(v.len() / step * step);
        v = v.chunks(step).map(|c| c.iter().copied().fold(f32::NEG_INFINITY, f32::max)).collect();
    }
    v.into_iter().map(|x| np::round(x as f64, 3)).collect()
}

/// 好み（Phase 4）: 使わない曲（★1・最近かけた曲）と、★ による相性の上乗せ。pool の ★ も読み直す。
pub fn apply_prefs(pool: &Pool, prefs_opt: Option<&Map<String, Value>>) -> (HashSet<i64>, HashMap<i64, f64>) {
    let empty = Map::new();
    let o = prefs_opt.unwrap_or(&empty);
    let ratings = prefs::ratings();
    *pool.ratings.write() = ratings.clone();
    let mut skip: HashSet<i64> = HashSet::new();
    if settings::get_bool(o, "skip_rating1", true) {
        skip.extend(ratings.iter().filter(|(_, &r)| r == 1).map(|(&t, _)| t));
    }
    let days = settings::get_f64(o, "avoid_recent_days", 0.0);
    if days > 0.0 {
        skip.extend(prefs::recent(days));
    }
    let mut bonus: HashMap<i64, f64> = if settings::get_bool(o, "prefer_rating", true) {
        ratings.iter().map(|(&t, &r)| (t, rating_bonus(r))).collect()
    } else {
        HashMap::new()
    };
    if settings::get_bool(o, "prefer_plays", true) {
        let pc = prefs::play_counts();
        if !pc.is_empty() {
            let vals: Vec<f64> = pc.values().map(|&n| (n as f64).ln_1p()).collect();
            let top = np::quantile(&vals, 0.95);
            let top = if top == 0.0 { 1.0 } else { top };
            for (&t, &n) in &pc {
                let e = bonus.entry(t).or_insert(0.0);
                *e += PLAYS_BONUS * 1f64.min((n as f64).ln_1p() / top);
            }
        }
    }
    (skip, bonus)
}

/// 1 曲あたりの長さ・使う区間（I-7）を指定されたら、ids の要約の使う範囲と繋ぐ部分を差し替えた見かけの曲の集まりを返す。
pub fn with_segments(view: View, ids: &[i64], track_len: Option<f64>, track_pos: &str) -> View {
    let has_len = matches!(track_len, Some(x) if x != 0.0);
    if !has_len && (track_pos.is_empty() || track_pos == "auto") {
        return view;
    }
    let mut out = View::new();
    for &t in ids {
        let base = &view[&t];
        let v = segments::variant(t, base, track_len, Some(track_pos), None, None);
        let mut d = (**base).clone();
        match v {
            Some(v) => v.apply(&mut d),
            None => d.seg = Some(Seg { len: track_len, pos: track_pos.to_string(), t0: None, t1: None }),
        }
        out.insert(t, Arc::new(d));
    }
    out
}

/// 結果の曲ごとの区間の指定（{id, len, pos, t0, t1, jm}。jm はその曲から次の曲へのつなぎ方 'cut' / 'mix'）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SegSpec {
    pub id: i64,
    #[serde(default)]
    pub len: Option<f64>,
    #[serde(default)]
    pub pos: Option<String>,
    #[serde(default)]
    pub t0: Option<f64>,
    #[serde(default)]
    pub t1: Option<f64>,
    #[serde(default)]
    pub jm: Option<String>,
}

/// 並びに入らなかったアルバムの曲を、前後の相性が一番落ちない所へ 1 曲ずつ差し込む（起点の曲の前には入れない）。
fn cover_albums(m: &Matrix, seq: &[usize], album: &[usize], allowed: &[bool]) -> Vec<usize> {
    let mut seq = seq.to_vec();
    let in_seq: HashSet<usize> = seq.iter().map(|&s| album[s]).collect();
    let mut want: Vec<usize> = (0..album.len()).filter(|&k| allowed[k]).map(|k| album[k]).filter(|a| !in_seq.contains(a)).collect();
    want.sort_unstable();
    want.dedup();
    for a in want {
        let mut best: Option<(f64, usize, usize)> = None;
        for t in (0..album.len()).filter(|&t| allowed[t] && album[t] == a) {
            for p in 1..=seq.len() {
                let prev = seq[p - 1];
                let gain = m.s_at(prev, t) + seq.get(p).map(|&nx| m.s_at(t, nx) - m.s_at(prev, nx)).unwrap_or(0.0);
                if best.map(|b| gain > b.0).unwrap_or(true) {
                    best = Some((gain, t, p));
                }
            }
        }
        if let Some((_, t, p)) = best {
            seq.insert(p, t);
        }
    }
    seq
}

/// 起点の曲を自動で選ぶ: 流れの始まりに合うエネルギーの曲（上げていく → 静かな曲、落ち着かせる → 盛り上がった曲）。
/// 3 案が違う始まりになるよう、別のアルバムから順に選ぶ。
fn auto_seeds(m: &Matrix, cand: &[usize], flow: &str, curve: Option<&[f64]>, album: &[usize], n: usize) -> Vec<usize> {
    let e: Vec<f64> = cand.iter().map(|&k| m.energy[k]).collect();
    let want = match flow {
        "up" => np::quantile(&e, 0.15),
        "down" => np::quantile(&e, 0.85),
        "custom" if curve.map(|c| !c.is_empty()).unwrap_or(false) => np::quantile(&e, curve.unwrap()[0].clamp(0.0, 1.0)),
        _ => np::quantile(&e, 0.5),
    };
    let dist: Vec<f64> = e.iter().map(|x| (x - want).abs()).collect();
    let order: Vec<usize> = crate::util::argsort(&dist).into_iter().map(|k| cand[k]).collect();
    let mut out: Vec<usize> = vec![];
    let mut seen: HashSet<usize> = HashSet::new();
    for &k in &order {
        if !seen.contains(&album[k]) {
            out.push(k);
            seen.insert(album[k]);
        }
        if out.len() >= n {
            break;
        }
    }
    let rest: Vec<usize> = order.iter().copied().filter(|k| !out.contains(k)).take(n.saturating_sub(out.len())).collect();
    out.extend(rest);
    out
}

fn with_avoid_zero(prefs_opt: Option<&Map<String, Value>>) -> Map<String, Value> {
    let mut o = prefs_opt.cloned().unwrap_or_default();
    o.insert("avoid_recent_days".into(), json!(0));
    o
}

fn elapsed(t0: Instant) -> f64 {
    np::round(t0.elapsed().as_secs_f64(), 2)
}

impl Pool {
    /// 並びはそのままで、曲ごとの区間（{id, len, pos}）を変えて繋ぎを計算し直す（結果の「区間」の切り替え。I-7）。
    pub fn rejoin(&self, spec: &[SegSpec], prefs_opt: Option<&Map<String, Value>>, join_mode: &str) -> Result<Value, PlanError> {
        apply_prefs(self, Some(&with_avoid_zero(prefs_opt))); // ★ を要約に入れる
        let spec: Vec<&SegSpec> = spec.iter().filter(|s| self.contains(s.id)).collect();
        if spec.is_empty() {
            return fail("解析済みの曲がありません");
        }
        let ids: Vec<i64> = spec.iter().map(|s| s.id).collect();
        let base = self.view(&ids);
        let mut view = View::new();
        for s in &spec {
            let b = &base[&s.id];
            let len = s.len.filter(|x| *x != 0.0);
            let pos = s.pos.as_deref().filter(|p| !p.is_empty()).unwrap_or("auto");
            let v = segments::variant(s.id, b, len, Some(pos), s.t0, s.t1);
            let mut d = (**b).clone();
            if let Some(v) = v {
                v.apply(&mut d);
            }
            view.insert(s.id, Arc::new(d));
        }
        let mut m = Matrix::new(&view, &ids, join_mode);
        m.auto_parts = prefs_opt.map(|o| settings::get_bool(o, "auto_parts", true)).unwrap_or(true);
        // つなぎ目ごとのつなぎ方（'cut' / 'mix'。s.jm はその曲から次の曲への繋ぎ）
        let modes: HashMap<usize, String> = spec
            .iter()
            .enumerate()
            .filter_map(|(n, s)| s.jm.as_deref().filter(|j| *j == "cut" || *j == "mix").map(|j| (n, j.to_string())))
            .collect();
        let seq: Vec<usize> = (0..ids.len()).collect();
        Ok(pack(&m, &seq, true, &modes).to_value())
    }

    /// 1 曲の区間の候補
    pub fn segment_choices(&self, tid: i64, length: Option<f64>) -> Value {
        match self.d.get(&tid) {
            None => json!([]),
            Some(d) => serde_json::to_value(segments::choices(tid, d, length)).unwrap_or(json!([])),
        }
    }

    /// アルバムで選ぶ（2026-10-25）: 指定したアルバム（3 枚以上）の曲だけで、おまかせの 3 案を作る。
    /// - テンポで候補を外さない（曲が少ないため。テンポの合わない所はエコーアウト・カットインで繋ぐ）
    /// - どのアルバムからも最低 1 曲入れ、同じアルバムが続きにくくする
    /// - 起点の曲は指定が無ければ自動（流れの始まりに合う曲。案ごとに別のアルバムから）
    /// - use_all: 選んだアルバムの曲をすべて並べる（全体の長さは使わない）
    #[allow(clippy::too_many_arguments)]
    pub fn plan_albums(
        &self,
        albums: &[String],
        minutes: f64,
        flow: &str,
        seed: Option<i64>,
        use_all: bool,
        avoid: &[i64],
        curve: Option<&[f64]>,
        prefs_opt: Option<&Map<String, Value>>,
        track_len: Option<f64>,
        track_pos: &str,
        join_mode: &str,
    ) -> Result<Value, PlanError> {
        let t0 = Instant::now();
        let albums: Vec<String> = np::dedup(albums.iter().filter(|a| !a.is_empty()).cloned().collect());
        if albums.len() < MIN_ALBUMS {
            return fail(format!("アルバムを {MIN_ALBUMS} 枚以上選んでください"));
        }
        // 自分で選んだアルバムなので「最近かけた曲は避ける」は使わない（★1 を使わない・★の優先はそのまま）
        let (skip, bonus_map) = apply_prefs(self, Some(&with_avoid_zero(prefs_opt)));
        let aset: HashSet<&String> = albums.iter().collect();
        let mut ids: Vec<i64> = self
            .ids
            .iter()
            .copied()
            .filter(|t| self.d[t].album.as_ref().map(|a| aset.contains(a)).unwrap_or(false) && !skip.contains(t))
            .collect();
        let have: HashSet<String> = ids.iter().filter_map(|t| self.d[t].album.clone()).collect();
        let missing: Vec<String> = albums.iter().filter(|a| !have.contains(*a)).cloned().collect();
        if have.len() < MIN_ALBUMS {
            return fail(format!(
                "解析済みの曲があるアルバムが {} 枚しかありません（{MIN_ALBUMS} 枚以上必要。解析を進めてください）",
                have.len()
            ));
        }
        if use_all && ids.len() > MAX_ALL_TRACKS {
            return fail(format!("全曲を並べられるのは {MAX_ALL_TRACKS} 曲までです（{} 曲）", ids.len()));
        }
        let seed = seed.filter(|s| ids.contains(s));
        if let Some(s) = seed {
            ids = std::iter::once(s).chain(ids.iter().copied().filter(|&t| t != s)).collect();
        }
        let sp = with_segments(self.view(&ids), &ids, track_len, track_pos);
        let mut m = Matrix::new(&sp, &ids, join_mode);
        m.auto_parts = prefs_opt.map(|o| settings::get_bool(o, "auto_parts", true)).unwrap_or(true);
        let mut names: Vec<String> = have.iter().cloned().collect();
        names.sort();
        let album: Vec<usize> =
            ids.iter().map(|t| names.iter().position(|n| Some(n) == self.d[t].album.as_ref()).unwrap()).collect();
        let allowed = vec![true; ids.len()];
        let bonus_v: Vec<f64> = ids.iter().map(|t| bonus_map.get(t).copied().unwrap_or(0.0)).collect();
        // 全曲を繋いだときのおおよその長さ
        let total = np::sum(&m.use_len) - ids.len() as f64 * np::median(&m.bar_sec) * 8.0;
        let target = if use_all { total } else { minutes * 60.0 };
        let cand_all: Vec<usize> = (0..ids.len()).collect();
        let seeds = if seed.is_some() { vec![0; 3] } else { auto_seeds(&m, &cand_all, flow, curve, &album, 3) };
        let mut avoid_v = vec![0f64; ids.len()];
        for t in avoid {
            if let Some(&k) = m.pos.get(t) {
                avoid_v[k] += 0.2;
            }
        }
        let fl = resolve_flow(flow, curve)?;
        let nids = ids.len();
        let stop_all = move |seq: &[usize], _l: f64| seq.len() >= nids;
        let stop_len = move |seq: &[usize], l: f64| l >= target || seq.len() >= nids;
        let stop: &dyn Fn(&[usize], f64) -> bool = if use_all { &stop_all } else { &stop_len };
        let mut plans: Vec<Value> = vec![];
        let mut packed_ids: Vec<Vec<i64>> = vec![];
        let rounds = if !use_all || seed.is_none() { 3 } else { 1 };
        for k in 0..rounds {
            let st = seeds[k % seeds.len()];
            let mut o = BeamOpts::new(&avoid_v);
            o.width = if use_all { BEAM.max(2 * ids.len()) } else { BEAM };
            o.bonus = Some(&bonus_v);
            o.album = Some(&album);
            let res = beam(&m, st, stop, &fl, target, &o);
            let Some(first) = res.first() else { break };
            let seq = cover_albums(&m, &first.1, &album, &allowed);
            let sids: Vec<i64> = seq.iter().map(|&s| ids[s]).collect();
            if packed_ids.iter().any(|p| !p.is_empty() && *p == sids) {
                continue;
            }
            let pk = pack(&m, &seq, true, &HashMap::new());
            packed_ids.push(pk.tracks.iter().map(|t| t.id).collect());
            let mut v = pk.to_value();
            let mut counts = Map::new();
            for (ai, a) in names.iter().enumerate() {
                counts.insert(a.clone(), json!(seq.iter().filter(|&&s| album[s] == ai).count()));
            }
            v["albums"] = Value::Object(counts);
            plans.push(v);
            for &s in &seq[1..] {
                avoid_v[s] += 0.25;
            }
        }
        Ok(json!({"plans": plans, "candidates": ids.len(), "albums": have.len(), "missing_albums": missing,
                  "max_minutes": np::round(total.max(0.0) / 60.0, 1), "flow": flow, "sec": elapsed(t0)}))
    }

    /// ライブラリから選んで作る: 互いに曲の半分以上が違う 3 案（design_phase3.md §4.3）。flow='custom' なら curve を目標にする。
    /// playlists を指定したら、選ぶ範囲はジャンルのまとまりではなく、その iTunes のプレイリストの曲（Phase 5 E）。
    #[allow(clippy::too_many_arguments)]
    pub fn plan_library(
        &self,
        seed: i64,
        groups: &[String],
        minutes: f64,
        flow: &str,
        avoid: &[i64],
        curve: Option<&[f64]>,
        prefs_opt: Option<&Map<String, Value>>,
        playlists: Option<&[i64]>,
        track_len: Option<f64>,
        track_pos: &str,
        join_mode: &str,
    ) -> Result<Value, PlanError> {
        let t0 = Instant::now();
        let Some(sd) = self.d.get(&seed).cloned() else {
            return fail("起点の曲がまだ解析されていません");
        };
        let gs: HashSet<&str> = groups.iter().map(|s| s.as_str()).collect();
        let (skip, bonus_map) = apply_prefs(self, prefs_opt);
        let auto_parts = prefs_opt.map(|o| settings::get_bool(o, "auto_parts", true)).unwrap_or(true);
        let mut cands: Vec<i64> = match playlists.filter(|p| !p.is_empty()) {
            Some(pl) => {
                let members = crate::library::itunes::playlist_tracks(pl);
                self.ids.iter().copied().filter(|t| *t != seed && !skip.contains(t) && members.contains(t)).collect()
            }
            None => self
                .ids
                .iter()
                .copied()
                .filter(|t| *t != seed && !skip.contains(t) && (gs.is_empty() || gs.contains(self.d[t].group)))
                .collect(),
        };
        // テンポが起点の曲から倍/半を考慮して ±25% 以内
        // カットインだけで繋ぐなら、テンポの違う曲も選ぶ（±50% 以内。design_cutin.md）
        let lim = if join_mode == "cut" { 0.41 } else { 0.23 };
        let tempo_ok = |d: &Desc| [1.0, 2.0, 0.5].iter().map(|k| (k * sd.bpm / d.bpm).ln().abs()).fold(f64::INFINITY, f64::min) < lim;
        cands.retain(|t| tempo_ok(&self.d[t]));
        if cands.len() > MAX_CANDS - 1 {
            // 多ければ、起点の曲に雰囲気の近い順
            let tv = sd.timbre;
            let mut keyed: Vec<(f64, i64)> = cands.iter().map(|t| (tim_dist(&self.d[t].timbre, &tv), *t)).collect();
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            cands = keyed.into_iter().take(MAX_CANDS - 1).map(|x| x.1).collect();
        }
        let mut ids: Vec<i64> = std::iter::once(seed).chain(cands).collect();
        if ids.len() < 3 {
            return fail("候補の曲が少なすぎます（範囲を広げるか、ライブラリの解析を進めてください）");
        }
        let sp = with_segments(self.view(&ids), &ids, track_len, track_pos);
        if segments::is_short(track_len) && track_pos != "full" {
            // 短い範囲を作れなかった曲（解析の小節が少ないなど）は、元の長い範囲のまま混ざらないよう候補から外す（起点の曲は残す）
            let tl = track_len.unwrap();
            let first = ids[0];
            ids = std::iter::once(first)
                .chain(ids[1..].iter().copied().filter(|t| sp[t].use_.t1 - sp[t].use_.t0 <= 2.5 * tl))
                .collect();
        }
        let mut m = Matrix::new(&sp, &ids, join_mode);
        m.auto_parts = auto_parts;
        let bonus_v: Vec<f64> = ids.iter().map(|t| bonus_map.get(t).copied().unwrap_or(0.0)).collect();
        let target = minutes * 60.0;
        let mut avoid_v = vec![0f64; ids.len()];
        for t in avoid {
            if let Some(&k) = m.pos.get(t) {
                if *t != seed {
                    avoid_v[k] += 0.2;
                }
            }
        }
        let fl = resolve_flow(flow, curve)?;
        let stop = |_seq: &[usize], l: f64| l >= target;
        let mut plans: Vec<Value> = vec![];
        for _ in 0..3 {
            let mut o = BeamOpts::new(&avoid_v);
            o.bonus = Some(&bonus_v);
            let res = beam(&m, 0, &stop, &fl, target, &o);
            let Some(first) = res.into_iter().next() else { break };
            let seq = first.1;
            plans.push(pack(&m, &seq, true, &HashMap::new()).to_value());
            for &k in &seq[1..] {
                avoid_v[k] += 0.25; // 次の案では、この案の曲を使いにくくする（互いに違う 3 案）
            }
        }
        let skipped = skip.iter().filter(|&&t| t != seed).count();
        Ok(json!({"plans": plans, "candidates": ids.len(), "skipped": skipped, "flow": flow, "sec": elapsed(t0)}))
    }

    /// キューを並べ替える: キューの曲だけで、最初の曲も含めて探す。今の並びも同じ物差しで返す。
    pub fn plan_queue(&self, ids: &[i64], join_mode: &str) -> Result<Value, PlanError> {
        let t0 = Instant::now();
        let have: Vec<i64> = ids.iter().copied().filter(|t| self.contains(*t)).collect();
        let missing: Vec<i64> = ids.iter().copied().filter(|t| !self.contains(*t)).collect();
        if have.len() < 2 {
            return fail("解析済みの曲が 2 曲以上必要です");
        }
        let mut m = Matrix::new(&self.view(&have), &have, join_mode);
        m.auto_parts = settings::get_bool(&settings::load(), "auto_parts", true);
        let n = have.len();
        let zeros = vec![0f64; n];
        let stop = move |seq: &[usize], _l: f64| seq.len() == n;
        let mut best: Option<(f64, Vec<usize>)> = None;
        for s in 0..n {
            let mut o = BeamOpts::new(&zeros);
            o.width = BEAM.max(3 * n);
            o.flow_w = 0.0;
            let res = beam(&m, s, &stop, &Flow::Keep, 1e9, &o);
            if let Some(first) = res.into_iter().next() {
                let seq = first.1;
                let pairs: Vec<f64> = seq.windows(2).map(|w| m.s_at(w[0], w[1])).collect();
                let sc = np::mean(&pairs);
                if best.as_ref().map(|b| sc > b.0).unwrap_or(true) {
                    best = Some((sc, seq));
                }
            }
        }
        let cur: Vec<usize> = (0..n).collect();
        let current = pack(&m, &cur, true, &HashMap::new());
        let best_seq = best.map(|b| b.1).unwrap_or_else(|| cur.clone());
        Ok(json!({"plans": [pack(&m, &best_seq, true, &HashMap::new()).to_value(), current.to_value()],
                  "missing": missing, "sec": elapsed(t0)}))
    }

    /// 次の曲の候補を絞り込む選択肢（解析済みの曲のアーティストとジャンルのまとまり、曲数つき）。
    pub fn candidate_facets(&self) -> Value {
        let mut arts: Vec<(String, String, i64)> = vec![]; // (比べる形, 表示, 曲数)
        let mut aidx: HashMap<String, usize> = HashMap::new();
        let mut groups: HashMap<&'static str, i64> = HashMap::new();
        for t in &self.ids {
            let d = &self.d[t];
            let a = d.artist.as_deref().unwrap_or("").trim();
            if !a.is_empty() {
                let k = norm_artist(Some(a));
                let i = *aidx.entry(k.clone()).or_insert_with(|| {
                    arts.push((k, a.to_string(), 0));
                    arts.len() - 1
                });
                arts[i].2 += 1;
            }
            *groups.entry(d.group).or_insert(0) += 1;
        }
        arts.sort_by_key(|a| a.1.to_lowercase());
        let names = genres::names();
        let mut order: Vec<&str> = names.clone();
        let mut extra: Vec<&str> = groups.keys().copied().filter(|g| !names.contains(g)).collect();
        extra.sort();
        order.extend(extra);
        json!({
            "artists": arts.iter().map(|a| json!({"name": a.1, "count": a.2})).collect::<Vec<_>>(),
            "groups": order.iter().filter(|g| groups.contains_key(*g)).map(|g| json!({"name": g, "count": groups[g]})).collect::<Vec<_>>(),
        })
    }

    /// 次の曲の候補（案Y）。prev の次に合う曲。nxt もあれば（差し替え）前後の相性の平均で並べる。
    /// artists を指定したら、そのアーティストの曲だけ（大文字小文字・前後の空白は区別しない）。
    #[allow(clippy::too_many_arguments)]
    pub fn next_candidates(
        &self,
        prev: Option<i64>,
        nxt: Option<i64>,
        groups: &[String],
        exclude: &[i64],
        n: usize,
        prefs_opt: Option<&Map<String, Value>>,
        artists: &[String],
    ) -> Value {
        let gs: HashSet<&str> = groups.iter().map(|s| s.as_str()).collect();
        let arts: HashSet<String> = artists.iter().map(|a| norm_artist(Some(a))).filter(|a| !a.is_empty()).collect();
        let mut ex: HashSet<i64> = exclude.iter().copied().collect();
        ex.extend(prev);
        ex.extend(nxt);
        let prev_in = prev.filter(|t| self.contains(*t));
        let nxt_in = nxt.filter(|t| self.contains(*t));
        let base: Vec<i64> = [prev_in, nxt_in].into_iter().flatten().collect();
        if base.is_empty() {
            return json!([]);
        }
        let rf = self.d[&base[0]].clone();
        let (skip, bonus_map) = apply_prefs(self, Some(&with_avoid_zero(prefs_opt))); // 自分で探すときは最近の曲も出す
        let mut cands: Vec<i64> = self
            .ids
            .iter()
            .copied()
            .filter(|t| {
                let d = &self.d[t];
                !ex.contains(t)
                    && !skip.contains(t)
                    && (gs.is_empty() || gs.contains(d.group))
                    && (arts.is_empty() || arts.contains(&norm_artist(d.artist.as_deref())))
                    && [1.0, 2.0, 0.5].iter().map(|k| (k * rf.bpm / d.bpm).ln().abs()).fold(f64::INFINITY, f64::min) < 0.3
            })
            .collect();
        if cands.len() > 1500 {
            let tv = rf.timbre;
            let mut keyed: Vec<(f64, i64)> = cands.iter().map(|t| (tim_dist(&self.d[t].timbre, &tv), *t)).collect();
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            cands = keyed.into_iter().take(1500).map(|x| x.1).collect();
        }
        let all: Vec<i64> = base.iter().copied().chain(cands.iter().copied()).collect();
        let view = self.view(&all);
        let m = Matrix::new(&view, &all, "auto");
        let off = base.len();
        let mut sc = vec![0f64; cands.len()];
        if let Some(p) = prev_in {
            let i = m.pos[&p];
            for (k, v) in sc.iter_mut().enumerate() {
                *v += m.s_at(i, off + k);
            }
        }
        if let Some(q) = nxt_in {
            let j = m.pos[&q];
            for (k, v) in sc.iter_mut().enumerate() {
                *v += m.s_at(off + k, j);
            }
        }
        for (k, v) in sc.iter_mut().enumerate() {
            *v /= base.len() as f64;
            *v += bonus_map.get(&cands[k]).copied().unwrap_or(0.0);
        }
        let neg: Vec<f64> = sc.iter().map(|v| -v).collect();
        let mut out: Vec<Value> = vec![];
        for k in crate::util::argsort(&neg).into_iter().take(n) {
            let t = cands[k];
            let d = &view[&t];
            let mut row = Map::new();
            row.insert("id".into(), json!(t));
            row.insert("title".into(), json!(d.title));
            row.insert("artist".into(), json!(d.artist));
            row.insert("album".into(), json!(d.album));
            row.insert("genre".into(), json!(d.genre));
            row.insert("bpm".into(), json!(d.bpm));
            row.insert("rating".into(), json!(d.rating));
            row.insert("key".into(), json!(d.key));
            row.insert("energy".into(), json!(np::round(d.energy, 3)));
            row.insert("use".into(), serde_json::to_value(&d.use_).unwrap_or(Value::Null));
            row.insert("duration".into(), json!(d.duration));
            row.insert("env".into(), json!(envelope(t, &d.use_)));
            row.insert("score".into(), json!(np::round(sc[k], 3)));
            if let Some(p) = prev_in {
                row.insert("join".into(), serde_json::to_value(m.join(p, t)).unwrap_or(Value::Null));
                row.insert("energy_delta".into(), json!(np::round(d.energy - self.d[&p].energy, 3)));
            }
            if let Some(q) = nxt_in {
                row.insert("join_next".into(), serde_json::to_value(m.join(t, q)).unwrap_or(Value::Null));
            }
            out.push(Value::Object(row));
        }
        Value::Array(out)
    }
}

// ---------------------------------------------------------------- 画面から呼ぶ（/api/auto/*）
/// 並びはそのままで、曲ごとの区間を変えて繋ぎを計算し直す（/api/auto/rejoin）
pub fn rejoin(spec: &[SegSpec], prefs_opt: Option<&Map<String, Value>>, join_mode: &str) -> Result<Value, PlanError> {
    get_pool().rejoin(spec, prefs_opt, join_mode)
}

/// 1 曲の区間の候補（/api/auto/segments。戻り値は items の配列）
pub fn segment_choices(tid: i64, length: Option<f64>) -> Value {
    get_pool().segment_choices(tid, length)
}

/// アルバムで選ぶ（/api/auto/plan mode=albums）
#[allow(clippy::too_many_arguments)]
pub fn plan_albums(
    albums: &[String],
    minutes: f64,
    flow: &str,
    seed: Option<i64>,
    use_all: bool,
    avoid: &[i64],
    curve: Option<&[f64]>,
    prefs_opt: Option<&Map<String, Value>>,
    track_len: Option<f64>,
    track_pos: &str,
    join_mode: &str,
) -> Result<Value, PlanError> {
    get_pool().plan_albums(albums, minutes, flow, seed, use_all, avoid, curve, prefs_opt, track_len, track_pos, join_mode)
}

/// ライブラリから選んで作る（/api/auto/plan mode=library）
#[allow(clippy::too_many_arguments)]
pub fn plan_library(
    seed: i64,
    groups: &[String],
    minutes: f64,
    flow: &str,
    avoid: &[i64],
    curve: Option<&[f64]>,
    prefs_opt: Option<&Map<String, Value>>,
    playlists: Option<&[i64]>,
    track_len: Option<f64>,
    track_pos: &str,
    join_mode: &str,
) -> Result<Value, PlanError> {
    get_pool().plan_library(seed, groups, minutes, flow, avoid, curve, prefs_opt, playlists, track_len, track_pos, join_mode)
}

/// キューを並べ替える（/api/auto/plan mode=queue）
pub fn plan_queue(ids: &[i64], join_mode: &str) -> Result<Value, PlanError> {
    get_pool().plan_queue(ids, join_mode)
}

/// 次の曲の候補の絞り込みの選択肢（/api/auto/facets）
pub fn candidate_facets() -> Value {
    get_pool().candidate_facets()
}

/// 次の曲の候補（/api/auto/next。戻り値は items の配列）
pub fn next_candidates(
    prev: Option<i64>,
    nxt: Option<i64>,
    groups: &[String],
    exclude: &[i64],
    n: usize,
    prefs_opt: Option<&Map<String, Value>>,
    artists: &[String],
) -> Value {
    get_pool().next_candidates(prev, nxt, groups, exclude, n, prefs_opt, artists)
}
