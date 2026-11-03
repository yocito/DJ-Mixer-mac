//! 1 曲の解析（djmix/analysis/pipeline.py）。
//!
//! Python 版と同じ順に計算する。ただし拍の検出（Beat This!）と、拍に依らない特徴量（ラウドネス・波形・フレームごとの特徴・
//! 曲全体の特徴）は互いに関係しないので並列に計算する（Mac の CPU のコアを使い切る）。
use super::bars::{bar_phases, downbeat_strength, estimate_meter};
use super::beats::{classify, fix_octave, local_tempo, no_beat_regions, raw_metrics, smooth};
use super::dsp::npy::{median_f64, py_round};
use super::types::{Analysis, BarFeats, TempoCurve, Timing, WaveInfo, SR, WAVE_RATE};
use super::{beat_this, features};
use crate::audio::decode::decode;
use crate::util::percentile;
use anyhow::Result;
use std::time::Instant;

fn r(v: &[f64], nd: usize) -> Vec<f64> {
    v.iter().map(|&x| py_round(x, nd)).collect()
}

/// 戻り値: (analysis.json の内容, 波形 (3,n) uint8 を帯域ごとに並べたもの, 小節特徴量, ダウンビート確率 50fps)。
pub fn analyze_file(path: &str) -> Result<(Analysis, Vec<u8>, BarFeats, Vec<f32>)> {
    let t0 = Instant::now();
    let audio = decode(path)?;
    let duration = audio.frames() as f64 / SR as f64;
    let mono = audio.mono();

    // 拍の検出 ‖ 拍に依らない特徴量
    let (det, (((lufs, peak), wave), (ff, extra))) = rayon::join(
        || beat_this::detect(&mono, SR).map(|d| (d, t0.elapsed().as_secs_f64())),
        || {
            rayon::join(
                || rayon::join(|| features::loudness(&audio, SR), || features::waveform(&mono, SR)),
                || {
                    rayon::join(
                        || features::frame_features(&super::dsp::resample::resample(&mono, SR, 22050)),
                        || features::track_extra(&mono, SR),
                    )
                },
            )
        },
    );
    let (det, t_detect) = det?;
    let (raw_b, raw_d, bprob, dprob) = (det.beats, det.downbeats, det.beat_prob, det.down_prob);

    let (b, octave_fixes) = fix_octave(&raw_b, &bprob);
    let b = smooth(&b);
    let nobeat = no_beat_regions(&b, &bprob, duration);
    let lt = local_tempo(&b);
    let valid: Vec<f64> = lt.iter().copied().filter(|x| x.is_finite()).collect();
    let n = valid.len();
    let bpm = if n > 0 { median_f64(&valid) } else { 0.0 };
    let k = (n / 5).max(1);
    let head = if n > 0 { median_f64(&valid[..k.min(n)]) } else { 0.0 };
    let tail = if n > 0 { median_f64(&valid[n - k.min(n)..]) } else { 0.0 };
    let drift = if head != 0.0 { (tail - head) / head * 100.0 } else { 0.0 };

    let strength = downbeat_strength(&b, &dprob);
    let (meter, meter_scores) = estimate_meter(&strength);
    let bar_list = bar_phases(&b, &strength, meter);
    let bar_times: Vec<f64> = bar_list.iter().map(|x| x.time).collect();
    let (feats, keys) = features::bar_features_from(&ff, &bar_times, duration);

    let m = raw_metrics(&raw_b, &raw_d, &bprob);
    let no_beat_total: f64 = nobeat.iter().map(|(s, e)| e - s).sum::<f64>() + 0.0; // 空の和は -0.0 になるので 0 に
    let (cls, reasons) = classify(&m, no_beat_total, drift);
    let metrics = super::types::Metrics {
        conf: py_round(m.conf, 4),
        unstable: py_round(m.unstable, 4),
        local16_ms: py_round(m.local16_ms, 4),
        downbeat_consistency: py_round(m.downbeat_consistency, 4),
    };
    let wave_len = wave.len() / 3;
    let a = Analysis {
        version: 0,
        duration: py_round(duration, 3),
        sample_rate: SR,
        bpm: py_round(bpm, 2),
        bpm_p5: (n > 0).then(|| py_round(percentile(&valid, 5.0), 2)),
        bpm_p95: (n > 0).then(|| py_round(percentile(&valid, 95.0), 2)),
        drift_pct: py_round(drift, 2),
        beats: r(&b, 4),
        beats_raw: r(&raw_b, 3),
        downbeats_raw: r(&raw_d, 3),
        meter,
        meter_scores,
        bars: bar_list,
        no_beat: nobeat,
        no_beat_total: py_round(no_beat_total, 1),
        tempo_curve: TempoCurve { t: r(&b, 3), bpm: lt.iter().map(|&x| py_round(if x.is_nan() { 0.0 } else { x }, 2)).collect() },
        octave_fixes,
        lufs: py_round(lufs, 2),
        peak_db: py_round(peak, 2),
        key: keys.all,
        key_head30: keys.head30,
        key_tail30: keys.tail30,
        beat_class: cls,
        beat_class_reasons: reasons,
        metrics,
        wave: WaveInfo { rate: WAVE_RATE, bands: vec!["low".into(), "mid".into(), "high".into()], length: wave_len },
        bar_columns: super::types::bar_columns(),
        extra: Some(extra),
        timing: Timing { detect_s: py_round(t_detect, 2), total_s: py_round(t0.elapsed().as_secs_f64(), 2) },
    };
    Ok((a, wave, feats, dprob))
}
