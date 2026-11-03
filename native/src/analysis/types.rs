//! 解析結果 analysis.json の形（djmix/analysis/pipeline.py の analyze_file の戻り値と同じキー）。
//! 画面（frontend）がこの JSON をそのまま使うので、キーの名前と意味は Python 版と同じにする。
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SR: u32 = 44100;
/// Beat This! の確率の刻み（1 秒あたり）
pub const FPS: f64 = 50.0;
pub const WAVE_RATE: u32 = 200;

pub const KEY_NAMES: [&str; 12] = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];
/// bars.f32 の列（rms, low, chroma_C..chroma_B, harmonic）
pub const NCOLS: usize = 15;
pub const COL_RMS: usize = 0;
pub const COL_LOW: usize = 1;
pub const COL_CHROMA: usize = 2;
pub const COL_HARM: usize = 14;

pub fn bar_columns() -> Vec<String> {
    let mut v = vec!["rms".to_string(), "low".to_string()];
    v.extend(KEY_NAMES.iter().map(|k| format!("chroma_{k}")));
    v.push("harmonic".into());
    v
}

/// 小節ごとの特徴量（行 = 小節、列 = bar_columns）
pub type BarFeats = Vec<[f32; NCOLS]>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Bar {
    pub beat: i64,
    pub time: f64,
    pub conf: f64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub meter: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct KeyInfo {
    pub key: Option<String>,
    pub camelot: Option<String>,
    pub corr: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TempoCurve {
    pub t: Vec<f64>,
    pub bpm: Vec<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metrics {
    pub conf: f64,
    pub unstable: f64,
    pub local16_ms: f64,
    pub downbeat_consistency: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaveInfo {
    pub rate: u32,
    pub bands: Vec<String>,
    pub length: usize,
}

impl Default for WaveInfo {
    fn default() -> Self {
        WaveInfo { rate: WAVE_RATE, bands: vec!["low".into(), "mid".into(), "high".into()], length: 0 }
    }
}

/// 曲全体の特徴量（おまかせ用。features.track_extra）
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Extra {
    pub centroid: f64,
    pub low_ratio: f64,
    pub onset_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Timing {
    pub detect_s: f64,
    pub total_s: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    #[serde(default)]
    pub version: u32,
    pub duration: f64,
    #[serde(default = "default_sr")]
    pub sample_rate: u32,
    pub bpm: f64,
    #[serde(default)]
    pub bpm_p5: Option<f64>,
    #[serde(default)]
    pub bpm_p95: Option<f64>,
    #[serde(default)]
    pub drift_pct: f64,
    pub beats: Vec<f64>,
    #[serde(default)]
    pub beats_raw: Vec<f64>,
    #[serde(default)]
    pub downbeats_raw: Vec<f64>,
    pub meter: u32,
    #[serde(default)]
    pub meter_scores: BTreeMap<String, f64>,
    pub bars: Vec<Bar>,
    #[serde(default)]
    pub no_beat: Vec<(f64, f64)>,
    #[serde(default)]
    pub no_beat_total: f64,
    #[serde(default)]
    pub tempo_curve: TempoCurve,
    #[serde(default)]
    pub octave_fixes: u32,
    #[serde(default = "default_lufs")]
    pub lufs: f64,
    #[serde(default)]
    pub peak_db: f64,
    #[serde(default)]
    pub key: KeyInfo,
    #[serde(default)]
    pub key_head30: KeyInfo,
    #[serde(default)]
    pub key_tail30: KeyInfo,
    #[serde(default)]
    pub beat_class: String,
    #[serde(default)]
    pub beat_class_reasons: Vec<String>,
    #[serde(default)]
    pub metrics: Metrics,
    #[serde(default)]
    pub wave: WaveInfo,
    #[serde(default = "bar_columns")]
    pub bar_columns: Vec<String>,
    #[serde(default)]
    pub extra: Option<Extra>,
    #[serde(default)]
    pub timing: Timing,
}

fn default_sr() -> u32 {
    SR
}
fn default_lufs() -> f64 {
    -14.0
}

impl Analysis {
    pub fn bar_times(&self) -> Vec<f64> {
        self.bars.iter().map(|b| b.time).collect()
    }

    /// 小節の頭の時刻 + 曲の終わり（Python の `[b["time"] for b in a["bars"]] + [a["duration"]]`）
    pub fn bar_edges(&self) -> Vec<f64> {
        let mut v = self.bar_times();
        v.push(self.duration);
        v
    }

    /// [t0, t1) に掛かる拍の取れない区間の長さ（秒）
    pub fn nobeat_overlap(&self, t0: f64, t1: f64) -> f64 {
        self.no_beat.iter().map(|&(s, e)| (t1.min(e) - t0.max(s)).max(0.0)).sum()
    }
}

/// 区間の候補（sections.compute）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Section {
    pub bar0: usize,
    pub bar1: usize,
    pub t0: f64,
    pub t1: f64,
    pub bars: usize,
    pub tags: Vec<String>,
}

impl Section {
    pub fn has_tag(&self, t: &str) -> bool {
        self.tags.iter().any(|x| x == t)
    }
}

/// カットインの候補（cutpoints.compute）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CutBar {
    pub t: f64,
    pub db: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CutPoint {
    pub bar: usize,
    pub t: f64,
    pub score: f64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct CutPoints {
    pub bars: Vec<CutBar>,
    pub outs: Vec<CutPoint>,
    pub ins: Vec<CutPoint>,
}
