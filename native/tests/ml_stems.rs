//! 音源分離（native/src/analysis/stems.rs）。
//!
//! - 分離した音の保存（8ch Ogg Vorbis）と、サンプル単位のシーク付きの読み込み: いつも確かめる
//! - htdemucs の分離を PyTorch の基準（tools/ml_reference.py）と比べる: DJMIX_ML_TEST_DIR があるときだけ
//!   DJMIX_ML_TEST_DIR=<DIR> ORT_DYLIB_PATH=<libonnxruntime> cargo test --release --features ort-dynamic --test ml_stems -- --nocapture
use djmix::analysis::stems;
use djmix::audio::decode::Stereo;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Instant;

static INIT: Once = Once::new();

/// config::paths() を最初に使う前に、データとモデルの場所を決める（このテストの中で 1 回だけ）
fn setup() -> Option<PathBuf> {
    let d = std::env::var_os("DJMIX_ML_TEST_DIR").map(PathBuf::from).filter(|d| d.join("ml_reference.json").exists());
    INIT.call_once(|| {
        let data = std::env::temp_dir().join(format!("djmix-ml-stems-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        std::env::set_var("DJMIX_DATA", &data);
        if let Some(d) = &d {
            std::env::set_var("DJMIX_MODELS", d);
        }
    });
    d
}

fn read_f32(p: &Path) -> Vec<f32> {
    std::fs::read(p).unwrap().chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// 8ch の試しの音（パートごとに違う音）
fn test_sep(n: usize) -> Vec<Vec<f32>> {
    (0..8)
        .map(|c| {
            (0..n)
                .map(|i| {
                    let t = i as f64 / 44100.0;
                    let f = 110.0 * (c + 1) as f64;
                    let env = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * 0.5 * t).sin();
                    (0.3 * env * (2.0 * std::f64::consts::PI * f * t).sin()
                        + 0.05 * (2.0 * std::f64::consts::PI * 3000.0 * t * (1.0 + c as f64 * 0.1)).sin())
                        as f32
                })
                .collect()
        })
        .collect()
}

#[test]
fn stems_audio_roundtrip_and_seek() {
    setup();
    let tid = 900_001;
    let n = 44100 * 20 + 123;
    let sep = test_sep(n);
    assert!(!stems::audio_ready(tid));
    let t0 = Instant::now();
    stems::save_audio(tid, &sep, 44100).unwrap();
    let t_enc = t0.elapsed().as_secs_f64();
    assert!(stems::audio_ready(tid));
    let size = std::fs::metadata(stems::audio_path(tid)).unwrap().len();
    // 全体を読む
    let t0 = Instant::now();
    let (all, total) = stems::read_audio(tid, 0, n + 1000).unwrap();
    let t_dec = t0.elapsed().as_secs_f64();
    assert_eq!(total, n, "総フレーム数");
    assert_eq!(all.len(), 8);
    assert!(all.iter().all(|c| c.len() == n));
    // 元の音との差（非可逆なので SNR で）
    let mut worst_snr = f64::INFINITY;
    for (a, b) in all.iter().zip(&sep) {
        let (mut s, mut e) = (0f64, 0f64);
        for (x, y) in a.iter().zip(b) {
            s += (*y as f64).powi(2);
            e += (*x as f64 - *y as f64).powi(2);
        }
        worst_snr = worst_snr.min(10.0 * (s / e).log10());
    }
    println!(
        "ogg 8ch {:.1}s: {:.2} MB ({:.1} MB / 5 min), encode {:.2}s, decode {:.2}s, worst SNR {:.1} dB",
        n as f64 / 44100.0,
        size as f64 / 1e6,
        size as f64 / 1e6 * 300.0 / (n as f64 / 44100.0),
        t_enc,
        t_dec,
        worst_snr
    );
    assert!(worst_snr > 20.0, "SNR {worst_snr}");
    // 途中から読んでも、全体を読んだものと同じサンプル（サンプル単位で正確なシーク）
    for &(start, len) in
        &[(0usize, 1000usize), (1, 4096), (44100 * 3 + 17, 44100), (441000, 2), (n - 500, 2000), (123_457, 300_000)]
    {
        let (part, total) = stems::read_audio(tid, start, len).unwrap();
        assert_eq!(total, n);
        let want = len.min(n - start);
        for c in 0..8 {
            assert_eq!(part[c].len(), want);
            let d = part[c].iter().zip(&all[c][start..start + want]).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
            assert!(d < 1e-6, "start {start} ch {c}: max diff {d}");
        }
    }
    let (part, _) = stems::read_audio(tid, n + 10, 100).unwrap();
    assert!(part.iter().all(|c| c.is_empty()));
}

#[test]
fn request_queue_done_and_pending() {
    setup();
    // パートの量が保存済みなら "done"
    let tid = 900_002;
    stems::save(tid, &vec![[0.5, 0.1, 0.2, 0.3]; 4]).unwrap();
    assert_eq!(stems::request(tid, "/nonexistent.mp3".into(), None, false), "done");
    // 解析されていない曲は "pending" → 裏で None になって終わる（on_done は呼ばれない）
    let tid2 = 900_003;
    let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c2 = called.clone();
    let cb: stems::OnDone = Box::new(move |_, _| c2.store(true, std::sync::atomic::Ordering::SeqCst));
    assert_eq!(stems::request(tid2, "/nonexistent.mp3".into(), Some(cb), true), "pending");
    for _ in 0..200 {
        if !stems::is_pending(tid2, true) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!stems::is_pending(tid2, true));
    assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn separate_matches_pytorch() {
    let Some(d) = setup() else {
        eprintln!("skip: DJMIX_ML_TEST_DIR が無いので htdemucs の比較はしない");
        return;
    };
    if !stems::available() {
        eprintln!("skip: 音源分離を使えない（--features ort-dynamic と ORT_DYLIB_PATH、{} を確かめる）", stems::MODEL_FILE);
        return;
    }
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("ml_reference.json")).unwrap()).unwrap();
    let stereo = read_f32(&d.join("signal_long.f32"));
    let n = stereo.len() / 2;
    let ref_sep = read_f32(&d.join("dm_long_sep.f32"));
    assert_eq!(ref_sep.len(), 8 * n);
    let audio = Stereo { data: stereo };
    let t0 = Instant::now();
    let sep = stems::separate(&audio).unwrap();
    let dt = t0.elapsed().as_secs_f64();
    assert_eq!(sep.len(), 8);
    let mut worst_abs = 0f32;
    let mut worst_rel = 0f64;
    for (c, ch) in sep.iter().enumerate() {
        assert_eq!(ch.len(), n);
        let r = &ref_sep[c * n..(c + 1) * n];
        let (mut e2, mut s2, mut ea) = (0f64, 0f64, 0f32);
        for (a, b) in ch.iter().zip(r) {
            ea = ea.max((a - b).abs());
            e2 += ((a - b) as f64).powi(2);
            s2 += (*b as f64).powi(2);
        }
        let rel = (e2 / s2).sqrt();
        println!(
            "  {} {}: max abs err {ea:.3e}, rel rms err {rel:.3e} (ref max {:.3})",
            stems::SOURCES[c / 2],
            ["L", "R"][c % 2],
            r.iter().map(|v| v.abs()).fold(0f32, f32::max)
        );
        worst_abs = worst_abs.max(ea);
        worst_rel = worst_rel.max(rel);
    }
    let py = meta["dm_long"]["time_s"].as_f64().unwrap_or(0.0);
    let segs = (n as f64 / (343980.0f64 * 0.75).floor()).ceil();
    println!("separate {:.1}s ({segs} segments): worst max abs err {worst_abs:.3e}, worst rel rms err {worst_rel:.3e}; rust {dt:.1}s vs pytorch {py:.1}s",
        n as f64 / 44100.0);
    assert!(worst_rel < 1e-3, "rel err {worst_rel}");
    // 小節ごとの量
    let bars: Vec<f64> = (0..18).map(|i| i as f64 * 2.0).collect();
    let ref_planar: Vec<Vec<f32>> = (0..8).map(|c| ref_sep[c * n..(c + 1) * n].to_vec()).collect();
    let a = stems::levels(&sep, 44100, &bars, 36.0);
    let b = stems::levels(&ref_planar, 44100, &bars, 36.0);
    let e = a.iter().zip(&b).flat_map(|(x, y)| x.iter().zip(y).map(|(p, q)| (p - q).abs())).fold(0f32, f32::max);
    println!("levels: max abs err {e:.3e}");
    assert!(e < 1e-3);
}
