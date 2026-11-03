//! 速さ: 10 分ほどの 3 曲のミックスを書き出す時間（実時間の何倍速か）。
//! release で: `cargo test --release --test export_perf -- --ignored --nocapture`
mod export_common;
use djmix::export::mixer::Mixdown;
use djmix::export::render::build_plan;
use djmix::export::writer::WavSink;
use export_common::*;
use serde_json::json;
use std::time::Instant;

#[test]
#[ignore]
fn render_10min_mix_faster_than_realtime() {
    let dir = tmpdir();
    let specs = [(122.0, 230.0), (126.0, 230.0), (124.0, 230.0)];
    let mut tracks = vec![];
    for (i, &(bpm, secs)) in specs.iter().enumerate() {
        let p = dir.path().join(format!("t{i}.wav"));
        // クリック＋持続音＋小さな雑音（無音の所が無い、ふつうの曲に近い重さ）
        let mut st = click_samples(bpm, 4, secs, 0.3, 0.05);
        let mut s: u64 = 12345 + i as u64;
        for v in st.iter_mut() {
            s = (s * 1103515245 + 12345) % 2147483648;
            *v += 0.05 * (s as f32 / 2147483648.0 - 0.5);
        }
        write_wav(&p, &st);
        let item = if i < 2 { json!({"join_tpl": "long", "join_bars": 16}) } else { json!({}) };
        tracks.push(synth_track_first(i, bpm, secs, Some(&p), 2.0, Some(secs - 3.0), item, 0.3));
    }
    let st = settings(json!({"tempo_range": 0.08, "lufs_target": -14, "xfade_curve": "smooth", "tempo_return": true}));
    let t0 = Instant::now();
    let plan = build_plan(&mut tracks, &st);
    let t_plan = t0.elapsed().as_secs_f64();
    let out = dir.path().join("mix.wav");
    let mut sink = WavSink::new(&out, "wav", SR).unwrap();
    let mut m = Mixdown::new(&tracks, &plan, &st, false, -0.3);
    m.run(&mut |b: &[u8]| sink.write(b).unwrap(), &mut |_| true).unwrap();
    let res = sink.close(None).unwrap();
    let el = t0.elapsed().as_secs_f64();
    let secs = res["seconds"].as_f64().unwrap();
    let speed = secs / el;
    println!("mix {secs:.1} s rendered in {el:.2} s (plan {t_plan:.3} s): {speed:.1}x real time [{}]",
             if cfg!(debug_assertions) { "debug" } else { "release" });
    assert!(secs > 540.0);
    if !cfg!(debug_assertions) {
        assert!(speed > 10.0, "遅い: {speed}x");
    }
}

/// 部品ごとの速さ（1 スレッド。実時間の何倍か）
#[test]
#[ignore]
fn component_timing() {
    use djmix::export::mixer::{Channel, Limiter};
    use djmix::export::stretch::new_stretch;
    let n = 60 * SR as usize;
    let x: Vec<f32> = (0..n * 2).map(|i| ((i as f32) * 0.001).sin() * 0.3).collect();
    let t = Instant::now();
    let mut s = new_stretch(2, 0.0);
    let mut out = vec![0.0f32; 1024];
    let mut j = 0;
    while j + 1100 < n * 2 {
        s.process(&x[j..j + 974], &mut out);
        j += 974;
    }
    println!("stretch 2ch (rate 0.95): {:.1}x", 60.0 / t.elapsed().as_secs_f64());
    let xd: Vec<f64> = x.iter().map(|&v| v as f64).collect();
    let steps = n / 512;
    let (low, hp) = (vec![1.0; 176], vec![0.0; 176]);
    let t = Instant::now();
    let mut ch = Channel::new();
    for c in 0..steps / 176 {
        let _ = ch.process(&xd[c * 176 * 1024..(c + 1) * 176 * 1024], 1.0, &low, &hp);
    }
    println!("channel: {:.1}x", 60.0 / t.elapsed().as_secs_f64());
    let t = Instant::now();
    let mut l = Limiter::new(-0.3);
    for c in 0..steps / 176 {
        let _ = l.process(&xd[c * 176 * 1024..(c + 1) * 176 * 1024]);
    }
    println!("limiter: {:.1}x", 60.0 / t.elapsed().as_secs_f64());
}
