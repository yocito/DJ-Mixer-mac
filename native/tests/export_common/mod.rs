//! 書き出しの試験の共通（合成の曲・WAV の書き出し）。tests/test_render.py・tests/test_analysis.py の道具と同じもの。
#![allow(dead_code)]
use djmix::export::render::{BeatMap, Track};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const SR: u32 = 44100;

/// インターリーブのステレオ（f32）を 16bit の WAV に
pub fn write_wav(path: &Path, stereo: &[f32]) {
    let spec = hound::WavSpec { channels: 2, sample_rate: SR, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for v in stereo {
        w.write_sample((v.clamp(-1.0, 1.0) * 32767.0) as i16).unwrap();
    }
    w.finalize().unwrap();
}

/// 1 拍目を強調したクリック＋低音（test_analysis.click_track と同じ。first: 最初のクリックの時刻、pad: 小さな持続音を足す）
pub fn click_samples(bpm: f64, meter: usize, seconds: f64, first: f64, pad: f32) -> Vec<f32> {
    let n = (seconds * SR as f64) as usize;
    let mut y = vec![0.0f32; n];
    let len = (0.05 * SR as f64) as usize;
    let beat = 60.0 / bpm;
    let mut k = 0usize;
    loop {
        let s = ((first + k as f64 * beat) * SR as f64) as usize;
        if s >= n {
            break;
        }
        let down = k.is_multiple_of(meter);
        for j in 0..len.min(n - s) {
            let t = j as f64 / SR as f64;
            let tone = (2.0 * std::f64::consts::PI * if down { 1500.0 } else { 900.0 } * t).sin() * (-t * 60.0).exp() * if down { 0.9 } else { 0.4 };
            let kick = (2.0 * std::f64::consts::PI * 60.0 * t).sin() * (-t * 30.0).exp() * if down { 0.9 } else { 0.3 };
            y[s + j] += (tone + kick) as f32;
        }
        k += 1;
    }
    if pad != 0.0 {
        for (i, v) in y.iter_mut().enumerate() {
            *v += pad * (2.0 * std::f64::consts::PI * 220.0 * i as f64 / SR as f64).sin() as f32;
        }
    }
    let mut st = Vec::with_capacity(n * 2);
    for v in y {
        st.push(v);
        st.push(v);
    }
    st
}

pub fn click_track(path: &Path, bpm: f64, seconds: f64) {
    write_wav(path, &click_samples(bpm, 4, seconds, 0.0, 0.0));
}

/// 一定テンポの合成の曲（test_render.synth_track と同じ。拍は 0.25 秒から）
pub fn synth_track(idx: usize, bpm: f64, seconds: f64, path: Option<&Path>, t0: f64, t1: Option<f64>, item: Value) -> Track {
    synth_track_first(idx, bpm, seconds, path, t0, t1, item, 0.25)
}

#[allow(clippy::too_many_arguments)]
pub fn synth_track_first(idx: usize, bpm: f64, seconds: f64, path: Option<&Path>, t0: f64, t1: Option<f64>, item: Value, first: f64) -> Track {
    let beat = 60.0 / bpm;
    let nb = ((seconds - 0.5) / beat) as usize;
    let beats: Vec<f64> = (0..nb).map(|i| first + i as f64 * beat).collect();
    let bars: Vec<f64> = beats.iter().enumerate().filter(|(i, _)| i % 4 == 0).map(|(_, b)| *b).collect();
    let bm = BeatMap::from_parts(beats, 4, bars, vec![]);
    let mut it: Map<String, Value> = Map::new();
    it.insert("track_id".into(), json!(idx + 1));
    it.insert("path".into(), json!(path.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()));
    if let Value::Object(m) = item {
        for (k, v) in m {
            it.insert(k, v);
        }
    }
    let mut t = Track::new(idx, it, path.map(PathBuf::from).unwrap_or_default(), bm, t0, t1.unwrap_or(seconds - 1.0), -14.0);
    t.title = format!("t{idx}");
    t
}

pub fn settings(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

pub fn tmpdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// 16bit の PCM → (L, R)
pub fn pcm_to_lr(b: &[u8]) -> (Vec<f64>, Vec<f64>) {
    let mut l = vec![];
    let mut r = vec![];
    for c in b.chunks_exact(4) {
        l.push(i16::from_le_bytes([c[0], c[1]]) as f64 / 32767.0);
        r.push(i16::from_le_bytes([c[2], c[3]]) as f64 / 32767.0);
    }
    (l, r)
}

fn env_win(x: &[f64], a: i64, b: i64) -> Vec<f64> {
    // 低音を除いた音（差分）の絶対値の 0.5ms の移動平均
    let w = 22usize;
    let get = |i: i64| if i >= 1 && (i as usize) < x.len() { (x[i as usize] - x[i as usize - 1]).abs() } else { 0.0 };
    let mut out = Vec::with_capacity((b - a) as usize);
    let mut acc = 0.0;
    for i in (a - w as i64)..b {
        acc += get(i);
        acc -= get(i - w as i64);
        if i >= a {
            out.push(acc / w as f64);
        }
    }
    out
}

/// クリックの位置のずれ（秒）: y の時刻 t の前後と、x（元の音）の時刻 c の前後の包絡の相互相関が最大になるずれ（±max_lag 秒、補間つき）。
/// ストレッチで頭が少しにじんでも、音のかたまりの位置で比べられる
pub fn xcorr_lag(y: &[f64], t: f64, x: &[f64], c: f64, max_lag: f64) -> Option<f64> {
    let sr = SR as f64;
    let (pre, post) = ((0.01 * sr) as i64, (0.04 * sr) as i64);
    let ml = (max_lag * sr) as i64;
    let ex = env_win(x, (c * sr) as i64 - pre, (c * sr) as i64 + post);
    let ty = (t * sr) as i64;
    let ey = env_win(y, ty - pre - ml, ty + post + ml);
    if ex.iter().sum::<f64>() < 1e-6 || ey.iter().sum::<f64>() < 1e-6 {
        return None;
    }
    let score = |lag: i64| -> f64 {
        let off = (lag + ml) as usize;
        ex.iter().enumerate().map(|(i, v)| v * ey[i + off]).sum()
    };
    let lags: Vec<i64> = (-ml..=ml).collect();
    let sc: Vec<f64> = lags.iter().map(|&l| score(l)).collect();
    let bi = (0..sc.len()).max_by(|&a, &b| sc[a].partial_cmp(&sc[b]).unwrap())?;
    let mut lag = lags[bi] as f64;
    if bi > 0 && bi + 1 < sc.len() {
        let (a, b, c2) = (sc[bi - 1], sc[bi], sc[bi + 1]);
        let den = a - 2.0 * b + c2;
        if den.abs() > 1e-18 {
            lag += 0.5 * (a - c2) / den;
        }
    }
    // t の位置での小数点以下のずれも足す
    Some((lag + (ty as f64 - t * sr) - ((c * sr) as i64 as f64 - c * sr)) / sr)
}

/// 音の立ち上がり（クリックの頭）: 予想の時刻 t の前後 (−win, +win) 秒で、包絡（1ms）が最大の 30% を最初に超えたところ（秒、補間つき）
pub fn onset_near(x: &[f64], t: f64, win: f64) -> Option<f64> {
    let sr = SR as f64;
    let a = ((t - win) * sr).max(0.0) as usize;
    let b = (((t + win) * sr) as usize).min(x.len());
    if b <= a + 100 {
        return None;
    }
    // 低音を除いた音（差分）の絶対値の 1ms の移動平均
    let w = 44usize;
    let hp: Vec<f64> = (a..b).map(|i| if i > 0 { (x[i] - x[i - 1]).abs() } else { 0.0 }).collect();
    let mut env = vec![0.0; hp.len()];
    let mut acc = 0.0;
    for i in 0..hp.len() {
        acc += hp[i];
        if i >= w {
            acc -= hp[i - w];
        }
        env[i] = acc / w as f64;
    }
    let mx = env.iter().cloned().fold(0.0, f64::max);
    if mx <= 1e-4 {
        return None;
    }
    let th = 0.3 * mx;
    let i = env.iter().position(|&v| v >= th)?;
    let frac = if i > 0 { (th - env[i - 1]) / (env[i] - env[i - 1]).max(1e-12) } else { 0.0 };
    // 移動平均の窓の真ん中に合わせる
    Some((a as f64 + i as f64 - 1.0 + frac - w as f64 / 2.0) / sr)
}
