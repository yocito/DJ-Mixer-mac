//! 音の特徴量（features.rs と analysis::dsp）が Python 版（pyloudnorm / scipy / librosa / soxr）と一致するか。
//! fixture は tests/fixtures/dsp/make_fixtures.py が作る。`cargo test --test dsp_audio -- --nocapture` で誤差の実測値を表示する。
mod dsp_common;

use djmix::analysis::dsp::butter::{butter_sos, BType};
use djmix::analysis::dsp::onset::{onset_detect_env, onset_strength};
use djmix::analysis::dsp::resample::resample;
use djmix::analysis::dsp::stft::stft_mag;
use djmix::analysis::features::{self, frame_features};
use djmix::analysis::types::KeyInfo;
use dsp_common::*;
use serde_json::Value;

fn signals() -> Value {
    fixture("audio.json")
}

fn each_signal(sr: &str, mut f: impl FnMut(&str, &Value)) {
    let fx = signals();
    for (name, ent) in fx["signals"].as_object().unwrap() {
        f(name, &ent[sr]);
    }
}

fn key_of(v: &Value) -> KeyInfo {
    KeyInfo {
        key: v["key"].as_str().map(String::from),
        camelot: v["camelot"].as_str().map(String::from),
        corr: v["corr"].as_f64().unwrap_or(f64::NAN),
    }
}

#[test]
fn sos_coefficients_match_scipy() {
    let fx = signals();
    let mut worst = 0f64;
    for (fs, list) in fx["sos"].as_object().unwrap() {
        let fs: f64 = fs.parse().unwrap();
        let mine = [
            butter_sos(4, &[200.0], BType::Lowpass, fs),
            butter_sos(4, &[200.0, 2500.0], BType::Bandpass, fs),
            butter_sos(4, &[2500.0], BType::Highpass, fs),
        ];
        for (m, r) in mine.iter().zip(list.as_array().unwrap()) {
            let r = f64s(r);
            let flat: Vec<f64> = m.iter().flatten().copied().collect();
            assert_eq!(flat.len(), r.len(), "fs={fs}");
            for (a, b) in flat.iter().zip(&r) {
                let e = (a - b).abs() / b.abs().max(1e-300);
                worst = worst.max(if *b == 0.0 { a.abs() } else { e });
            }
        }
    }
    println!("sos: 最大の相対誤差 {worst:.3e}");
    assert!(worst < 1e-12, "{worst}");
}

#[test]
fn loudness_matches_pyloudnorm() {
    each_signal("44100", |name, e| {
        let st = stereo(&e["spec"]);
        let (lufs, peak) = features::loudness(&st, 44100);
        let peak_ref = e["peak"].as_f64().unwrap();
        // numpy の float32 の log10 は 1 ulp ずれることがある（float32 で 3.6 dB 付近の 1 ulp = 2.4e-7）
        assert!((peak - peak_ref).abs() < 1e-5, "{name}: peak {peak} vs {peak_ref}");
        if let Some(l) = e.get("loudness") {
            let lr = l[0].as_f64().unwrap();
            println!("loudness {name}: rust {lufs:.6} python {lr:.6} 差 {:.2e}", (lufs - lr).abs());
            assert!((lufs - lr).abs() < 0.05, "{name}: {lufs} vs {lr}");
        } else {
            // Python（pyloudnorm）は 400ms 未満で例外。Rust は −70
            assert_eq!(lufs, -70.0, "{name}");
        }
    });
}

#[test]
fn waveform_matches_scipy() {
    each_signal("44100", |name, e| {
        let st = stereo(&e["spec"]);
        let w = features::waveform(&st.mono(), 44100);
        let r = u8s(&e["wave"]);
        assert_eq!(w.len(), r.len(), "{name}");
        let diff: Vec<i32> = w.iter().zip(&r).map(|(&a, &b)| a as i32 - b as i32).collect();
        let n_off = diff.iter().filter(|&&d| d != 0).count();
        let max_off = diff.iter().map(|d| d.abs()).max().unwrap_or(0);
        println!("waveform {name}: {} 点のうち {n_off} 点が違う（最大 {max_off} LSB）", r.len());
        assert!(max_off <= 1, "{name}");
        assert!(n_off * 1000 <= r.len().max(1), "{name}: {n_off}");
    });
}

#[test]
fn resample_matches_soxr() {
    each_signal("44100", |name, e| {
        if e.get("resample_22050").is_none() {
            return;
        }
        let mono = stereo(&e["spec"]).mono();
        for tsr in [22050u32, 11025] {
            let r = &e[format!("resample_{tsr}")];
            let z = resample(&mono, 44100, tsr);
            assert_eq!(z.len() as u64, r["len"].as_u64().unwrap(), "{name} {tsr}");
            let k = f32s(&r["head"]).len();
            let mid = r["mid_at"].as_u64().unwrap() as usize;
            let parts = [
                (&z[..k], f32s(&r["head"])),
                (&z[mid..mid + k], f32s(&r["mid"])),
                (&z[z.len() - k..], f32s(&r["tail"])),
            ];
            let peak = mono.iter().fold(0f32, |m, &v| m.max(v.abs())) as f64;
            let mut worst = 0f64;
            for (a, b) in parts.iter() {
                worst = worst.max(max_abs(a, b));
            }
            let energy: f64 = z.iter().map(|&v| (v as f64) * (v as f64)).sum();
            let er = (energy / r["energy"].as_f64().unwrap() - 1.0).abs();
            println!("resample {name} 44100→{tsr}: 最大誤差 {worst:.3e}（信号のピーク {peak:.3}）、エネルギーの相対差 {er:.2e}");
            assert!(worst < 2e-6 * peak.max(1.0), "{name} {tsr}: {worst}");
            assert!(er < 1e-5, "{name} {tsr}: {er}");
        }
    });
}

#[test]
fn frame_features_match_librosa() {
    each_signal("22050", |name, e| {
        let st = stereo(&e["spec"]);
        let mono = st.mono();
        let ff = frame_features(&mono);
        let fr = &e["frames"];
        let tuning = fr["tuning"].as_f64().unwrap();
        let chroma_ref = f32s(&fr["chroma"]);
        let chroma: Vec<f32> = ff.chroma.iter().flatten().copied().collect();
        let low_ref = f32s(&fr["low"]);
        let rms_ref = f32s(&fr["rms"]);
        println!(
            "frames {name}: tuning rust {:.2} python {tuning:.2}; chroma 最大誤差 {:.2e}、low 相対 {:.2e}、rms 相対 {:.2e}",
            ff.tuning,
            max_abs(&chroma, &chroma_ref),
            max_rel(&ff.low, &low_ref),
            max_rel(&ff.rms, &rms_ref)
        );
        assert!((ff.tuning - tuning).abs() < 1e-9, "{name}: tuning");
        assert!(max_abs(&chroma, &chroma_ref) < 1e-4, "{name}: chroma");
        assert!(max_rel(&ff.low, &low_ref) < 1e-5, "{name}: low");
        assert!(max_rel(&ff.rms, &rms_ref) < 1e-5, "{name}: rms");
    });
}

fn check_bar_features(sr: u32, tol: f64) {
    each_signal(&sr.to_string(), |name, e| {
        let st = stereo(&e["spec"]);
        let mono = st.mono();
        let bt = f64s(&e["bar_times"]);
        let dur = e["duration"].as_f64().unwrap();
        let (feats, keys) = features::bar_features(&mono, sr, &bt, dur);
        let r = f32s(&e["bar_feats"]);
        let flat: Vec<f32> = feats.iter().flatten().copied().collect();
        assert_eq!(flat.len(), r.len(), "{name}");
        // rms・low（列 0, 1）はすべての小節で一致。クロマ・harmonic は、デジタル無音から音が始まる所の小節だけ違ってよい:
        // soxr は float32 の FFT の雑音（1e-8）を音の手前 1 ブロック（〜0.04 秒）に出すので、librosa はそのフレームの雑音を
        // 正規化したクロマにする。Rust 版はフィルタの範囲外をちょうど 0 にするので、そのフレームのクロマは 0
        let mut worst = 0f64;
        let mut worst_level = 0f64;
        let mut bad_bars = vec![];
        for b in 0..feats.len() {
            let row = &flat[b * 15..(b + 1) * 15];
            let want = &r[b * 15..(b + 1) * 15];
            worst_level = worst_level.max(max_abs(&row[..2], &want[..2]));
            let e = max_abs(row, want);
            if e < tol {
                worst = worst.max(e);
            } else {
                bad_bars.push((b, e));
            }
        }
        println!("bar_features {name} @{sr}: {} 小節、最大誤差 {worst:.2e}（rms/low {worst_level:.2e}）、外れた小節 {bad_bars:?}", feats.len());
        assert!(worst_level < tol, "{name}: rms/low {worst_level}");
        let allowed = if sr == 44100 && name == "noise_silence" { 1 } else { 0 };
        assert!(bad_bars.len() <= allowed, "{name}: {bad_bars:?}");
        let k = &e["keys"];
        for (mine, which) in [(&keys.all, "all"), (&keys.head30, "head30"), (&keys.tail30, "tail30")] {
            let want = key_of(&k[which]);
            assert_eq!(mine.key, want.key, "{name} {which}");
            assert_eq!(mine.camelot, want.camelot, "{name} {which}");
            // 上のデジタル無音のフレームは曲全体のクロマの平均にも入る
            let ctol = if allowed > 0 { 0.01 } else { 1e-3 + 1e-12 };
            assert!((mine.corr - want.corr).abs() <= ctol, "{name} {which}: {} vs {}", mine.corr, want.corr);
        }
    });
}

#[test]
fn bar_features_match_at_22050() {
    // 22050Hz の入力は再標本化しない: クロマ・RMS などの計算そのものの一致
    check_bar_features(22050, 1e-4);
}

#[test]
fn bar_features_match_at_44100() {
    // 44100Hz: soxr の再現の誤差を含む
    check_bar_features(44100, 1e-4);
}

#[test]
fn onsets_match_librosa() {
    each_signal("11025", |name, e| {
        let mono = stereo(&e["spec"]).mono();
        let on = &e["onset"];
        let env_ref = f32s(&on["env"]);
        let power = stft_mag(&mono, 2048, 512).power();
        let env = onset_strength(&power, 11025.0, 512);
        let frames = onset_detect_env(&env, 11025.0, 512);
        let fr: Vec<usize> = on["frames"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
        println!(
            "onset {name}: 強度の最大誤差 {:.2e}、オンセット rust {} 個 / python {} 個",
            max_abs(&env, &env_ref),
            frames.len(),
            fr.len()
        );
        assert!(max_abs(&env, &env_ref) < 1e-3, "{name}");
        assert_eq!(frames, fr, "{name}");
    });
}

fn check_extra(sr: &str) {
    each_signal(sr, |name, e| {
        let mono = stereo(&e["spec"]).mono();
        let x = features::track_extra(&mono, sr.parse().unwrap());
        let r = &e["extra"];
        let (c, l, o) =
            (r["centroid"].as_f64().unwrap(), r["low_ratio"].as_f64().unwrap(), r["onset_rate"].as_f64().unwrap());
        println!("extra {name} @{sr}: rust {x:?} / python centroid {c} low_ratio {l} onset_rate {o}");
        assert!((x.centroid - c).abs() <= c.abs() * 0.005 + 0.1, "{name}: centroid");
        assert!((x.low_ratio - l).abs() <= 2e-4, "{name}: low_ratio");
        let dur = mono.len() as f64 / sr.parse::<f64>().unwrap();
        assert!((x.onset_rate - o).abs() * dur.max(1e-3) <= 1.0 + 1e-6, "{name}: onset");
    });
}

#[test]
fn extra_matches_at_11025() {
    check_extra("11025");
}

#[test]
fn extra_matches_at_44100() {
    check_extra("44100");
}

#[test]
fn estimate_key_matches() {
    let fx = signals();
    for c in fx["keys"].as_array().unwrap() {
        let ch = f32s(&c["chroma"]);
        let mut v = [0f64; 12];
        for i in 0..12 {
            v[i] = ch[i] as f64;
        }
        let k = features::estimate_key(&v);
        let want = key_of(&c["key"]);
        assert_eq!(k.key, want.key);
        assert_eq!(k.camelot, want.camelot);
        assert!((k.corr - want.corr).abs() < 1e-12, "{} vs {}", k.corr, want.corr);
    }
}
