//! Beat This!（native/src/analysis/beat_this.rs）を PyTorch の基準（tools/ml_reference.py）と比べる。
//!
//! 乱数の重みのモデルと基準の出力を作ってから（tools/ml_reference.py の説明のとおり）:
//!   DJMIX_ML_TEST_DIR=<DIR> ORT_DYLIB_PATH=<libonnxruntime> cargo test --release --features ort-dynamic --test ml_beat_this -- --nocapture
//! DJMIX_ML_TEST_DIR が無いとき（ふだんの cargo test）は何もせずに通る。
use djmix::analysis::beat_this as bt;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn test_dir() -> Option<PathBuf> {
    let d = std::env::var_os("DJMIX_ML_TEST_DIR").map(PathBuf::from)?;
    if !d.join("ml_reference.json").exists() {
        eprintln!("skip: {}/ml_reference.json がありません", d.display());
        return None;
    }
    // モデルはこのフォルダから読む（config::paths() を最初に使う前に）
    std::env::set_var("DJMIX_MODELS", &d);
    std::env::set_var("DJMIX_DATA", std::env::temp_dir().join("djmix-ml-test"));
    Some(d)
}

fn read_f32(p: &Path) -> Vec<f32> {
    std::fs::read(p).unwrap().chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn read_f64(p: &Path) -> Vec<f64> {
    std::fs::read(p).unwrap().chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn max_abs(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

fn rms(a: &[f32]) -> f64 {
    (a.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / a.len().max(1) as f64).sqrt()
}

/// 拍の一致: (一致した数, Rust の数, Python の数, 一致した拍の最大の時刻差)
fn match_times(a: &[f64], b: &[f64], tol: f64) -> (usize, usize, usize, f64) {
    let mut hit = 0;
    let mut worst = 0f64;
    for x in a {
        if let Some(d) = b.iter().map(|y| (x - y).abs()).min_by(|p, q| p.partial_cmp(q).unwrap()) {
            if d <= tol {
                hit += 1;
                worst = worst.max(d);
            }
        }
    }
    (hit, a.len(), b.len(), worst)
}

#[test]
fn beat_this_matches_pytorch() {
    let Some(d) = test_dir() else {
        eprintln!("skip: DJMIX_ML_TEST_DIR が無いので Beat This! の比較はしない");
        return;
    };
    if !bt::available() {
        eprintln!("skip: Beat This! を使えない（--features ort-dynamic と ORT_DYLIB_PATH、{} を確かめる）", bt::MODEL_FILE);
        return;
    }
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("ml_reference.json")).unwrap()).unwrap();
    for name in ["short", "long"] {
        let stereo = read_f32(&d.join(format!("signal_{name}.f32")));
        let mono: Vec<f32> = stereo.chunks_exact(2).map(|c| (c[0] + c[1]) * 0.5).collect();
        let ref22 = read_f32(&d.join(format!("bt_{name}_mono22.f32")));
        let ref_spect = read_f32(&d.join(format!("bt_{name}_spect.f32")));
        let ref_b = read_f32(&d.join(format!("bt_{name}_beat.f32")));
        let ref_d = read_f32(&d.join(format!("bt_{name}_down.f32")));
        let ref_beats = read_f64(&d.join(format!("bt_{name}_beats.f64")));
        let ref_downs = read_f64(&d.join(format!("bt_{name}_downs.f64")));
        let frames = ref_spect.len() / bt::N_MELS;
        println!("== {name}: {:.1}s, {frames} frames, {} chunks", mono.len() as f64 / 44100.0, bt::chunk_starts(frames).len());

        // 1. 22050Hz への変換（soxr との差）
        let t0 = Instant::now();
        let y = bt::resample_to_22050(&mono, 44100);
        let t_rs = t0.elapsed().as_secs_f64();
        assert_eq!(y.len(), ref22.len());
        let diff: Vec<f32> = y.iter().zip(&ref22).map(|(a, b)| a - b).collect();
        println!(
            "  resample vs soxr: max abs {:.3e}, rms err {:.3e} (signal rms {:.3e}, {:.1} dB) [{:.3}s]",
            max_abs(&y, &ref22),
            rms(&diff),
            rms(&ref22),
            20.0 * (rms(&diff) / rms(&ref22)).log10(),
            t_rs
        );
        assert!(rms(&diff) < rms(&ref22) * 1e-3);

        // 2. LogMelSpect（同じ 22050Hz の音から）
        let t0 = Instant::now();
        let spect = bt::log_mel_spect(&ref22);
        let t_sp = t0.elapsed().as_secs_f64();
        assert_eq!(spect.len(), ref_spect.len());
        let e_sp = max_abs(&spect, &ref_spect);
        let sp_max = ref_spect.iter().copied().fold(0f32, f32::max);
        println!("  spect (from soxr audio): max abs err {e_sp:.3e} (max value {sp_max:.2}) [{t_sp:.3}s]");
        assert!(e_sp < 1e-3, "spect err {e_sp}");
        let spect_own = bt::log_mel_spect(&y);
        println!("  spect (from own resampler): max abs err {:.3e}", max_abs(&spect_own, &ref_spect));

        // 3. 網 + 塊の組み立て（同じスペクトログラムから）
        let t0 = Instant::now();
        let (b, dn) = bt::predict(&ref_spect).unwrap();
        let t_nn = t0.elapsed().as_secs_f64();
        let (eb, ed) = (max_abs(&b, &ref_b), max_abs(&dn, &ref_d));
        let lr = ref_b.iter().map(|v| v.abs()).fold(0f32, f32::max);
        println!("  logits (from ref spect): max abs err beat {eb:.3e}, down {ed:.3e} (|logit| max {lr:.2}) [{t_nn:.2}s onnx]");
        assert!(eb < 1e-3 && ed < 1e-3);

        // 4. 全体（44.1kHz のモノラルから）
        let t0 = Instant::now();
        let det = bt::detect(&mono, 44100).unwrap();
        let t_all = t0.elapsed().as_secs_f64();
        let pb: Vec<f32> = ref_b.iter().map(|x| 1.0 / (1.0 + (-x).exp())).collect();
        let pd: Vec<f32> = ref_d.iter().map(|x| 1.0 / (1.0 + (-x).exp())).collect();
        let (epb, epd) = (max_abs(&det.beat_prob, &pb), max_abs(&det.down_prob, &pd));
        let mb = match_times(&det.beats, &ref_beats, 0.0101);
        let md = match_times(&det.downbeats, &ref_downs, 0.0101);
        let exact = det.beats == ref_beats && det.downbeats == ref_downs;
        let py = &meta[format!("bt_{name}")];
        let py_t = py["time_spect_s"].as_f64().unwrap_or(0.0)
            + py["time_model_s"].as_f64().unwrap_or(0.0)
            + py["time_post_s"].as_f64().unwrap_or(0.0);
        println!("  full: prob max abs err beat {epb:.3e}, down {epd:.3e}; beats {}/{} (py {}) worst {:.3}s; downbeats {}/{} (py {}); identical: {exact}",
            mb.0, mb.1, mb.2, mb.3, md.0, md.1, md.2);
        println!(
            "  time: rust {t_all:.2}s vs pytorch {py_t:.2}s (spect {:.3}s + model {:.2}s + post {:.3}s)",
            py["time_spect_s"].as_f64().unwrap_or(0.0),
            py["time_model_s"].as_f64().unwrap_or(0.0),
            py["time_post_s"].as_f64().unwrap_or(0.0)
        );
        assert!(epb < 5e-3 && epd < 5e-3);
        assert!(mb.0 as f64 >= 0.97 * ref_beats.len() as f64 && (mb.1 as i64 - mb.2 as i64).abs() <= 2);
        // 後処理だけ（基準の logits から）は完全に一致するはず
        let (pb2, pd2) = bt::postprocess(&ref_b, &ref_d);
        assert_eq!(pb2, ref_beats, "postprocess beats");
        assert_eq!(pd2, ref_downs, "postprocess downbeats");
        println!("  postprocess (from ref logits): identical");
    }
}
