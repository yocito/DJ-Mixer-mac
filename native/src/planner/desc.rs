//! おまかせ用の要約（djmix/planner/desc.py。design_phase3.md §3.2・§4.1）。解析キャッシュから作り、track_desc テーブルに保存する。
//!
//! 要約: BPM・拍子・キー・曲全体の特徴量（extra）・自動で決めた使う範囲・その頭と終わりの繋ぐ部分（16 小節）の特徴。
//! 探索のたびに analysis.json（拍の列などで 1 曲数百 KB）を 1 万曲分読まずに済むようにする。
//!
//! 保存する JSON（track_desc.data）は Python 版と同じキー・同じ形（DESC_VERSION も同じ）。
use super::segments::Seg;
use crate::analysis::stems::{self, StemLevels};
use crate::analysis::types::{Analysis, BarFeats, Extra, Section, COL_CHROMA, COL_HARM, COL_LOW};
use crate::analysis::{cache, sections};
use crate::db;
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 2: 使う範囲の説明の重複を除いた / 3: 使う範囲の小節ごとのボーカルの量（Phase 4 B1。分離が済んだ曲だけ）
/// 4: 繋ぐ部分を拍の取れる所へ（Phase 5 F。design_phase5.md §F）
pub const DESC_VERSION: i64 = 4;
/// 繋ぐ部分を拍の取れる所へずらす上限（秒。使う範囲の 40% までは許す）
pub const STABLE_MOVE_SEC: f64 = 90.0;
/// ずらしても使う範囲はこれだけ残す
pub const STABLE_KEEP_SEC: f64 = 120.0;
/// 相性を比べる「繋ぐ部分」の長さ
pub const REGION_BARS: usize = 16;
/// 使う範囲の上限の目安（長い曲は区間の区切りで切る）
pub const MAX_USE_SEC: f64 = 330.0;
pub const MIN_USE_SEC: f64 = 150.0;

/// 使う範囲 {t0, t1, bar0, bar1, notes}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Use {
    pub t0: f64,
    pub t1: f64,
    pub bar0: i64,
    pub bar1: i64,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// 繋ぐ部分（使う範囲の頭・終わりの 16 小節）の特徴
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Region {
    pub chroma: Vec<f64>,
    pub harm: f64,
    pub low: f64,
    pub nobeat: PyNum,
}

/// 整数と小数を区別して書き出す数（Python 版では、拍の取れない区間が無い曲の nobeat は sum([]) = 0 で整数になる）
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PyNum {
    pub v: f64,
    pub int: bool,
}

impl PyNum {
    pub fn float(v: f64) -> PyNum {
        PyNum { v, int: false }
    }
}

impl Serialize for PyNum {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.int {
            s.serialize_i64(self.v as i64)
        } else {
            s.serialize_f64(self.v)
        }
    }
}

impl<'de> Deserialize<'de> for PyNum {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Number::deserialize(d)?;
        Ok(PyNum { v: v.as_f64().unwrap_or(0.0), int: v.is_i64() || v.is_u64() })
    }
}

/// 1 曲の要約。保存するのは bpm〜vocals。id 以降は読み込むとき・Pool を作るときに付ける（保存しない）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Desc {
    pub bpm: f64,
    pub meter: i64,
    pub duration: f64,
    pub beat_class: Option<String>,
    pub key: String,
    pub key_head: String,
    pub key_tail: String,
    pub extra: Extra,
    #[serde(rename = "use")]
    pub use_: Use,
    #[serde(rename = "in")]
    pub in_: Region,
    pub out: Region,
    /// 使う範囲の小節ごとのボーカルの量（0..1）。音源分離が済んでいない曲は None
    #[serde(default)]
    pub vocals: Option<Vec<f64>>,

    // ---- load_all で付ける（tracks 表から）
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub title: Option<String>,
    #[serde(skip)]
    pub artist: Option<String>,
    #[serde(skip)]
    pub album: Option<String>,
    #[serde(skip)]
    pub genre: Option<String>,
    // ---- Pool で付ける（ライブラリ全体で標準化した値）
    #[serde(skip)]
    pub energy: f64,
    #[serde(skip)]
    pub timbre: [f64; 3],
    #[serde(skip)]
    pub group: &'static str,
    // ---- 好み（apply_prefs）・1 曲あたりの長さ（with_segments / rejoin）で付ける
    #[serde(skip)]
    pub rating: i64,
    #[serde(skip)]
    pub seg: Option<Seg>,
}

// ---------------------------------------------------------------- numpy・Python と同じ丸めと合計
/// Python / numpy と同じ結果になる数の小道具（並びの探索を Python 版と同じ結果にするため）。
pub mod np {
    /// Python の round(x, n)（10 進で正しく丸め、ちょうど半分は偶数へ）。
    pub fn round(x: f64, n: usize) -> f64 {
        if !x.is_finite() {
            return x;
        }
        format!("{:.*}", n, x).parse().unwrap_or(x)
    }

    /// Python の round(x)（整数へ。ちょうど半分は偶数へ）
    pub fn round_int(x: f64) -> i64 {
        x.round_ties_even() as i64
    }

    /// numpy の pairwise summation（add.reduce の内側。float64）
    pub fn pairwise_f64(n: usize, get: &dyn Fn(usize) -> f64) -> f64 {
        fn rec(lo: usize, n: usize, get: &dyn Fn(usize) -> f64) -> f64 {
            if n < 8 {
                let mut r = -0.0;
                for i in 0..n {
                    r += get(lo + i);
                }
                r
            } else if n <= 128 {
                let mut r = [0f64; 8];
                for (k, v) in r.iter_mut().enumerate() {
                    *v = get(lo + k);
                }
                let mut i = 8;
                while i < n - n % 8 {
                    for (k, v) in r.iter_mut().enumerate() {
                        *v += get(lo + i + k);
                    }
                    i += 8;
                }
                let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
                while i < n {
                    res += get(lo + i);
                    i += 1;
                }
                res
            } else {
                let mut n2 = n / 2;
                n2 -= n2 % 8;
                rec(lo, n2, get) + rec(lo + n2, n - n2, get)
            }
        }
        rec(0, n, get)
    }

    /// numpy の pairwise summation（float32）
    pub fn pairwise_f32(n: usize, get: &dyn Fn(usize) -> f32) -> f32 {
        fn rec(lo: usize, n: usize, get: &dyn Fn(usize) -> f32) -> f32 {
            if n < 8 {
                let mut r = -0.0f32;
                for i in 0..n {
                    r += get(lo + i);
                }
                r
            } else if n <= 128 {
                let mut r = [0f32; 8];
                for (k, v) in r.iter_mut().enumerate() {
                    *v = get(lo + k);
                }
                let mut i = 8;
                while i < n - n % 8 {
                    for (k, v) in r.iter_mut().enumerate() {
                        *v += get(lo + i + k);
                    }
                    i += 8;
                }
                let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
                while i < n {
                    res += get(lo + i);
                    i += 1;
                }
                res
            } else {
                let mut n2 = n / 2;
                n2 -= n2 % 8;
                rec(lo, n2, get) + rec(lo + n2, n - n2, get)
            }
        }
        rec(0, n, get)
    }

    pub fn sum(v: &[f64]) -> f64 {
        pairwise_f64(v.len(), &|i| v[i])
    }

    /// numpy の mean（float64）
    pub fn mean(v: &[f64]) -> f64 {
        sum(v) / v.len() as f64
    }

    /// numpy の std（ddof=0）
    pub fn std(v: &[f64]) -> f64 {
        let m = mean(v);
        let d: Vec<f64> = v.iter().map(|x| (x - m) * (x - m)).collect();
        (sum(&d) / v.len() as f64).sqrt()
    }

    /// float32 の 1 列の平均（numpy の a[lo:hi, col].mean()。float32 のまま足して割る）
    pub fn mean_f32_col(rows: &[[f32; 15]], col: usize) -> f32 {
        pairwise_f32(rows.len(), &|i| rows[i][col]) / rows.len() as f32
    }

    /// numpy.quantile（method='linear'。q は 0..1）
    pub fn quantile(v: &[f64], q: f64) -> f64 {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = s.len();
        let vi = (n as f64 - 1.0) * q;
        if vi >= n as f64 - 1.0 {
            return s[n - 1];
        }
        let (prev, next, g) = if vi < 0.0 {
            (0, 0, vi)
        } else {
            let p = vi.floor();
            (p as usize, p as usize + 1, vi - p)
        };
        let (a, b) = (s[prev], s[next]);
        let d = b - a;
        if g >= 0.5 {
            b - d * (1.0 - g)
        } else {
            a + d * g
        }
    }

    /// numpy.median
    pub fn median(v: &[f64]) -> f64 {
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = s.len();
        if n % 2 == 1 {
            s[n / 2]
        } else {
            (-0.0 + s[n / 2 - 1] + s[n / 2]) / 2.0
        }
    }

    /// numpy.linspace(0, 1, n)
    pub fn linspace01(n: usize) -> Vec<f64> {
        if n == 0 {
            return vec![];
        }
        if n == 1 {
            return vec![0.0];
        }
        let step = 1.0 / (n - 1) as f64;
        let mut v: Vec<f64> = (0..n).map(|i| i as f64 * step + 0.0).collect();
        v[n - 1] = 1.0;
        v
    }

    /// numpy.interp（xp は昇順）
    pub fn interp(x: f64, xp: &[f64], fp: &[f64]) -> f64 {
        let n = xp.len();
        if n == 1 {
            return fp[0];
        }
        if x.is_nan() {
            return x;
        }
        if x < xp[0] {
            return fp[0];
        }
        if x > xp[n - 1] {
            return fp[n - 1];
        }
        // xp[j] <= x < xp[j+1]
        let j = xp.partition_point(|&a| a <= x).saturating_sub(1);
        if j >= n - 1 || xp[j] == x {
            return fp[j.min(n - 1)];
        }
        let slope = (fp[j + 1] - fp[j]) / (xp[j + 1] - xp[j]);
        let r = slope * (x - xp[j]) + fp[j];
        if r.is_nan() {
            let r2 = slope * (x - xp[j + 1]) + fp[j + 1];
            if r2.is_nan() && fp[j] == fp[j + 1] {
                return fp[j];
            }
            return r2;
        }
        r
    }

    /// 順番を保ったまま重複を除く（Python の list(dict.fromkeys(v))）
    pub fn dedup(v: Vec<String>) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(v.len());
        for s in v {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }
}

fn fmt(s: f64) -> String {
    format!("{}:{:02}", (s / 60.0).floor() as i64, (s % 60.0).floor() as i64)
}

fn nobeat(no_beat: &[(f64, f64)], t0: f64, t1: f64) -> f64 {
    no_beat.iter().map(|&(s, e)| (t1.min(e) - t0.max(s)).max(0.0)).fold(0.0, |a, b| a + b)
}

fn has(s: &Section, tag: &str) -> bool {
    s.tags.iter().any(|t| t == tag)
}

/// 使う範囲 {t0, t1, bar0, bar1, notes}。試作 work/phase3/planner_proto.py と同じ規則（design_phase3.md §4.1）。
pub fn choose_use(a: &Analysis, secs: &[Section]) -> Use {
    let bars = a.bar_times();
    let bt = a.bar_edges();
    let nb = bars.len() as i64;
    let start = secs.iter().position(|s| !has(s, "拍なし")).unwrap_or(0);
    let mut end = secs.len() - 1;
    let mut notes: Vec<String> = vec![];
    if start != 0 {
        notes.push("拍の取れないイントロを飛ばす".into());
    }
    for (k, s) in secs.iter().enumerate().rev() {
        if !has(s, "拍なし") && s.t1 > secs[start].t0 + 60.0 {
            end = k;
            break;
        }
    }
    if end != secs.len() - 1 {
        notes.push("拍の取れない終わりを切る".into());
    }
    if secs[end].t1 - secs[start].t0 > MAX_USE_SEC {
        let st0 = secs[start].t0;
        let cand: Vec<usize> = (0..secs.len())
            .filter(|&k| {
                let d = secs[k].t1 - st0;
                (MIN_USE_SEC..=MAX_USE_SEC).contains(&d) && !has(&secs[k], "拍なし")
            })
            .collect();
        if let Some(&c) = cand.last() {
            end = c;
            notes.push(format!("長いので {} の区切りで終える", fmt(secs[end].t1)));
        }
    }
    // 小節の番号（Python では範囲外になる所は落ちるので、ここでは端にそろえる）
    let clamp = |k: i64| k.clamp(0, nb);
    let mut b0 = secs[start].bar0 as i64;
    let mut b1 = (secs[end].bar1 as i64).min(nb);
    // 区間より短い拍の取れない部分（最後のフェードアウト、冒頭の数秒）も繋ぐ部分から外す
    for &(s0, e0) in &a.no_beat {
        if bt[b1 as usize] - 30.0 < s0 && s0 < bt[b1 as usize] && s0 > bt[b0 as usize] + 60.0 {
            let last = bars.iter().rposition(|&x| x <= s0).unwrap_or(0) as i64;
            b1 = clamp((b0 + 8).max(last));
            notes.push("最後の拍が取れない部分を切る".into());
        }
        if bt[b0 as usize] <= e0 && e0 < bt[b0 as usize] + 30.0 && bt[b1 as usize] - e0 > 60.0 {
            let first = bars.iter().position(|&x| x >= e0).map(|i| i as i64).unwrap_or(b0);
            b0 = clamp((b1 - 8).min(first));
            notes.push("最初の拍が取れない部分を飛ばす".into());
        }
    }
    b1 = b1.max(b0 + 1).min(nb);
    let (b0, b1) = stable_bounds(&a.no_beat, &bt, b0 as usize, b1 as usize, Some(&mut notes));
    Use { t0: np::round(bt[b0], 3), t1: np::round(bt[b1], 3), bar0: b0 as i64, bar1: b1 as i64, notes: np::dedup(notes) }
}

/// 繋ぐ部分（終わりと始まりの REGION_BARS 小節）に拍の取れない所があれば、拍の取れる所で終わる・始まるよう範囲をずらす（Phase 5 F）。
/// ずらすのは最大 max(90 秒, 使う範囲の 40%)、使う範囲は 2 分以上残す。見つからなければそのまま（エコーアウトで繋ぐ）。
/// つなぎ目エディタの「拍の取れる所で繋ぐ」（transition.js の stableBounds）と同じ規則。
pub fn stable_bounds(
    no_beat: &[(f64, f64)],
    bt: &[f64],
    b0: usize,
    b1: usize,
    mut notes: Option<&mut Vec<String>>,
) -> (usize, usize) {
    let r = REGION_BARS;
    let ok = |s: usize, e: usize| nobeat(no_beat, bt[s], bt[e]) < 1.0;
    let (mut b0, mut b1) = (b0, b1);
    let span = bt[b1] - bt[b0];
    let limit = STABLE_MOVE_SEC.max(0.4 * span);
    if b1 > b0 + r && !ok(b1 - r, b1) {
        let mut e = b1 - 1;
        while e > b0 + r - 1 {
            if bt[b1] - bt[e] > limit || bt[e] - bt[b0] < STABLE_KEEP_SEC {
                break;
            }
            if ok(e - r, e) {
                b1 = e;
                if let Some(n) = notes.as_deref_mut() {
                    n.push("拍の取れる所で終える".into());
                }
                break;
            }
            e -= 1;
        }
    }
    if b1 > b0 + r && !ok(b0, b0 + r) {
        for s in (b0 + 1)..=(b1 - r) {
            if bt[s] - bt[b0] > limit || bt[b1] - bt[s] < STABLE_KEEP_SEC {
                break;
            }
            if ok(s, s + r) {
                b0 = s;
                if let Some(n) = notes.as_deref_mut() {
                    n.push("拍の取れる所から始める".into());
                }
                break;
            }
        }
    }
    (b0, b1)
}

/// 繋ぐ部分（side = "in": 使う範囲の頭の 16 小節 / "out": 終わりの 16 小節）の響き・低音・拍の取れなさ。
/// bt は小節の頭の時刻 + 曲の終わり。
pub fn region(no_beat: &[(f64, f64)], bt: &[f64], feats: &BarFeats, use_: &Use, side: &str) -> Region {
    let r = REGION_BARS as i64;
    let (b0, b1) = if side == "in" {
        (use_.bar0, (use_.bar0 + r).min(use_.bar1))
    } else {
        (use_.bar0.max(use_.bar1 - r), use_.bar1)
    };
    let nf = feats.len() as i64;
    let sl = |lo: i64, hi: i64| {
        let lo = lo.clamp(0, nf) as usize;
        let hi = hi.clamp(0, nf) as usize;
        &feats[lo..hi.max(lo)]
    };
    let seg = if b1 > b0 { sl(b0, b1) } else { sl((b0 - 1).max(0), b0 + 1) };
    // numpy と同じく float32 のまま平均する（クロマは行ごとに順に足す・1 列は pairwise）
    let mut acc = [0f32; 12];
    for row in seg {
        for (k, v) in acc.iter_mut().enumerate() {
            *v += row[COL_CHROMA + k];
        }
    }
    let cnt = seg.len() as f32;
    let chroma: Vec<f64> = acc.iter().map(|&v| np::round((v / cnt) as f64, 4)).collect();
    let nbt = bt.len() as i64;
    let t0 = bt[b0.clamp(0, nbt - 1) as usize];
    let t1 = bt[b1.min(nbt - 1).max(0) as usize];
    Region {
        chroma,
        harm: np::round(np::mean_f32_col(seg, COL_HARM) as f64, 4),
        low: np::round(np::mean_f32_col(seg, COL_LOW) as f64, 4),
        nobeat: PyNum { v: np::round(nobeat(no_beat, t0, t1), 2), int: no_beat.is_empty() },
    }
}

/// 要約を作る。小節が少なすぎる・拍が取れない曲は None（おまかせの候補にしない）。
/// キーが分からない曲（無音など）も None にする（Python 版はそのまま保存して、探索のときに落ちる）。
pub fn build(a: &Analysis, feats: &BarFeats, stems: Option<&StemLevels>) -> Option<Desc> {
    if a.bpm <= 0.0 || a.bars.len() < 16 || feats.len() != a.bars.len() {
        return None;
    }
    let extra = a.extra.clone()?;
    let key = a.key.camelot.clone()?;
    let key_head = a.key_head30.camelot.clone()?;
    let key_tail = a.key_tail30.camelot.clone()?;
    let secs = sections::compute(a, feats);
    let use_ = choose_use(a, &secs);
    let bt = a.bar_edges();
    let vocals = stems.filter(|s| s.len() == a.bars.len()).map(|s| {
        let (lo, hi) = (use_.bar0.max(0) as usize, (use_.bar1.max(0) as usize).min(s.len()));
        s[lo..hi.max(lo)].iter().map(|r| np::round(r[0] as f64, 2)).collect()
    });
    Some(Desc {
        bpm: a.bpm,
        meter: a.meter as i64,
        duration: a.duration,
        beat_class: Some(a.beat_class.clone()),
        key,
        key_head,
        key_tail,
        extra,
        in_: region(&a.no_beat, &bt, feats, &use_, "in"),
        out: region(&a.no_beat, &bt, feats, &use_, "out"),
        use_,
        vocals,
        ..Default::default()
    })
}

/// 要約を作り直して保存する（作れない曲は要約を消す）。stems が None なら解析キャッシュから読む（Python 版と同じ）。
pub fn update(track_id: i64, a: &Analysis, feats: &BarFeats, stems: Option<&StemLevels>) -> Result<Option<Desc>> {
    let read;
    let st = match stems {
        Some(s) => Some(s),
        None => {
            read = stems::read(track_id);
            read.as_ref()
        }
    };
    let d = build(a, feats, st);
    db::tx(|c| {
        match &d {
            None => {
                c.execute("DELETE FROM track_desc WHERE track_id = ?", [track_id])?;
            }
            Some(d) => {
                c.execute(
                    "INSERT INTO track_desc(track_id, version, data) VALUES(?, ?, ?) ON CONFLICT(track_id) DO UPDATE SET \
                     version = excluded.version, data = excluded.data",
                    rusqlite::params![track_id, DESC_VERSION, serde_json::to_string(d)?],
                )?;
            }
        }
        Ok(())
    })?;
    Ok(d)
}

/// 解析済みなのに要約が無い・古い曲の要約を、解析キャッシュから作る。作った数を返す。
pub fn backfill() -> Result<usize> {
    let ids: Vec<i64> = db::with(|c| {
        let mut st = c.prepare(
            "SELECT t.id FROM tracks t LEFT JOIN track_desc d ON d.track_id = t.id \
             WHERE t.analysis_status = 'done' AND (d.track_id IS NULL OR d.version != ?)",
        )?;
        let v = st.query_map([DESC_VERSION], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    })?;
    let mut n = 0;
    for tid in ids {
        let a = cache::read(tid);
        let feats = cache::bars_array(tid);
        if let (Some(a), Some(feats)) = (a, feats) {
            update(tid, &a, &feats, None)?;
            n += 1;
        }
    }
    Ok(n)
}

/// 要約のある曲すべて（読んだ順。id・曲名などのタグつき）。
pub fn load_all() -> Vec<Desc> {
    let (rw, rargs) = crate::library::roots::where_sql("t");
    let r = db::with(|c| {
        let sql = format!(
            "SELECT d.track_id, d.data, t.title, t.artist, t.album, t.genre, t.path FROM track_desc d \
             JOIN tracks t ON t.id = d.track_id WHERE d.version = ? AND t.analysis_status = 'done' AND {rw}"
        );
        let mut params: Vec<rusqlite::types::Value> = vec![DESC_VERSION.into()];
        params.extend(rargs.iter().cloned());
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (id, data, title, artist, album, genre) = row?;
            match serde_json::from_str::<Desc>(&data) {
                Ok(mut d) => {
                    d.id = id;
                    d.title = title;
                    d.artist = artist;
                    d.album = album;
                    d.genre = genre;
                    out.push(d);
                }
                Err(e) => log::warn!("track_desc of {id} is unreadable: {e}"),
            }
        }
        Ok(out)
    });
    r.unwrap_or_else(|e| {
        log::warn!("load_all: {e}");
        vec![]
    })
}

#[cfg(test)]
mod tests {
    use super::np;

    #[test]
    fn python_round() {
        assert_eq!(np::round(0.125, 2), 0.12);
        assert_eq!(np::round(0.375, 2), 0.38);
        assert_eq!(np::round(2.675, 2), 2.67);
        assert_eq!(np::round_int(2.5), 2);
        assert_eq!(np::round_int(3.5), 4);
    }

    #[test]
    fn quantile_lerp() {
        let v = [1.0, 2.0, 3.0, 4.0];
        assert!((np::quantile(&v, 0.5) - 2.5).abs() < 1e-12);
        assert_eq!(np::quantile(&v, 1.0), 4.0);
        assert_eq!(np::median(&[3.0, 1.0, 2.0]), 2.0);
        assert!((np::interp(0.25, &np::linspace01(3), &[0.0, 1.0, 0.0]) - 0.5).abs() < 1e-12);
    }
}
