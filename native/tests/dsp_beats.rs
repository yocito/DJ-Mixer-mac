//! 拍の後処理（beats.rs: local_tempo, fix_octave, smooth, no_beat_regions, raw_metrics, classify）が Python 版と一致するか。
//! fixture は tests/fixtures/dsp/make_fixtures.py が作る（beats.json）。tests/test_analysis.py の合成データのテストも移植。
mod dsp_common;

use djmix::analysis::beats as B;
use djmix::analysis::types::Metrics;
use dsp_common::*;
use serde_json::Value;

fn close(a: &[f64], b: &[f64], tol: f64, what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: 長さ");
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x.is_nan() || y.is_nan() {
            assert!(x.is_nan() && y.is_nan(), "{what}[{i}]: {x} vs {y}");
            continue;
        }
        assert!((x - y).abs() <= tol * y.abs().max(1.0), "{what}[{i}]: {x} vs {y}");
    }
}

fn regions(v: &Value) -> Vec<(f64, f64)> {
    v.as_array().unwrap().iter().map(|r| (r[0].as_f64().unwrap(), r[1].as_f64().unwrap())).collect()
}

#[test]
fn beat_postprocessing_matches_python() {
    let fx = fixture("beats.json");
    let mut worst = 0f64;
    for c in fx["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let beats = f64s(&c["beats"]);
        let prob = probs(&c["prob"]);
        let dur = c["duration"].as_f64().unwrap();
        let lt = B::local_tempo(&beats);
        close(&lt, &f64s(&c["local_tempo"]), 1e-9, &format!("{name} local_tempo"));
        let (fx_b, n) = B::fix_octave(&beats, &prob);
        assert_eq!(n as u64, c["nfix"].as_u64().unwrap(), "{name} nfix");
        close(&fx_b, &f64s(&c["fix"]), 1e-12, &format!("{name} fix"));
        let sm = B::smooth(&fx_b);
        let sm_ref = f64s(&c["smooth"]);
        close(&sm, &sm_ref, 1e-12, &format!("{name} smooth"));
        for (a, b) in sm.iter().zip(&sm_ref) {
            worst = worst.max((a - b).abs());
        }
        close(&B::smooth(&beats), &f64s(&c["smooth_raw"]), 1e-12, &format!("{name} smooth_raw"));
        // no_beat は Python の結果（を丸めたもの）を入力にして比べる（smooth の 1e-15 の差で丸めが変わらないように）
        assert_eq!(B::no_beat_regions(&sm_ref, &prob, dur), regions(&c["no_beat"]), "{name} no_beat");
        assert_eq!(B::no_beat_regions(&beats, &prob, dur), regions(&c["no_beat_raw"]), "{name} no_beat_raw");
        if let Some(m) = c.get("metrics") {
            let got = B::raw_metrics(&beats, &f64s(&c["downs"]), &prob);
            let want = Metrics {
                conf: bits(&m["conf"]),
                unstable: bits(&m["unstable"]),
                local16_ms: bits(&m["local16_ms"]),
                downbeat_consistency: bits(&m["downbeat_consistency"]),
            };
            assert_eq!(got.conf, want.conf, "{name} conf");
            assert_eq!(got.unstable, want.unstable, "{name} unstable");
            assert_eq!(got.downbeat_consistency, want.downbeat_consistency, "{name} downbeat_consistency");
            assert!(
                (got.local16_ms - want.local16_ms).abs() <= 1e-9 * want.local16_ms.abs().max(1.0),
                "{name} local16_ms {} vs {}",
                got.local16_ms,
                want.local16_ms
            );
        }
    }
    println!("smooth の拍時刻の最大差 {worst:.2e} 秒");
}

#[test]
fn classify_matches_python() {
    let fx = fixture("beats.json");
    for c in fx["classify"].as_array().unwrap() {
        let m = &c["m"];
        let met = Metrics {
            conf: m["conf"].as_f64().unwrap(),
            unstable: m["unstable"].as_f64().unwrap(),
            local16_ms: m["local16_ms"].as_f64().unwrap(),
            downbeat_consistency: m["downbeat_consistency"].as_f64().unwrap(),
        };
        let (cls, reasons) = B::classify(&met, c["no_beat_total"].as_f64().unwrap(), c["drift"].as_f64().unwrap());
        assert_eq!(cls, c["class"].as_str().unwrap());
        let want: Vec<String> =
            c["reasons"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(reasons, want);
    }
}

fn rmse(a: &[f64], b: &[f64]) -> f64 {
    (a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / a.len() as f64).sqrt()
}

fn nanmedian(v: &[f64]) -> f64 {
    djmix::analysis::dsp::npy::nanmedian_f64(v)
}

/// tests/test_analysis.py の test_smooth_removes_quantization_but_keeps_tempo_change（量子化した拍は fixture から）
#[test]
fn smooth_removes_quantization_but_keeps_tempo_change() {
    let fx = fixture("beats.json");
    let c = &fx["cases"][0];
    assert_eq!(c["name"], "quantized_tempo_change");
    let q = f64s(&c["beats"]);
    let mut truth = vec![];
    let mut acc = 0.0;
    for i in 0..80 {
        acc += if i < 40 { 60.0 / 123.0 } else { 60.0 / 136.0 };
        truth.push(0.013 + acc);
    }
    let sm = B::smooth(&q);
    assert!(rmse(&sm, &truth) < rmse(&q, &truth));
    let lt = B::local_tempo(&sm);
    assert!((nanmedian(&lt[..30]) - 123.0).abs() < 1.5 && (nanmedian(&lt[lt.len() - 30..]) - 136.0).abs() < 1.5);
}

/// tests/test_analysis.py の test_fix_octave_halves_doubled_section
#[test]
fn fix_octave_halves_doubled_section() {
    let mut beats: Vec<f64> = (0..40).map(|i| i as f64 * 0.5).collect();
    beats.extend((0..40).map(|i| 20.0 + i as f64 * 0.25));
    beats.extend((0..40).map(|i| 30.0 + i as f64 * 0.5));
    let (fixed, n) = B::fix_octave(&beats, &vec![1.0f32; 3000]);
    assert_eq!(n, 1);
    let lt = B::local_tempo(&fixed);
    let worst = lt.iter().filter(|v| !v.is_nan()).map(|v| (v / 120.0 - 1.0).abs()).fold(0.0, f64::max);
    assert!(worst < 0.3);
}
