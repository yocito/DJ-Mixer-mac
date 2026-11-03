//! 5 分の曲で特徴量の計算時間を測る（`cargo test --release --test dsp_perf -- --nocapture`）。
//! リリースビルドでは合計が 1 秒を大きく下回ることを確かめる（遅い CI でも落ちないよう、判定は 2 秒）。
use djmix::analysis::features;
use djmix::audio::decode::Stereo;
use std::time::Instant;

fn five_minutes() -> Stereo {
    let sr = 44100.0;
    let n = (300.0 * sr) as usize;
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut data = Vec::with_capacity(n * 2);
    for i in 0..n {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let u = (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / 9007199254740992.0 * 2.0 - 1.0;
        let t = i as f64 / sr;
        let beat = (t * 2.0).fract();
        let kick = (2.0 * std::f64::consts::PI * 55.0 * t).sin() * (-beat * 20.0).exp();
        let chord =
            [261.63, 329.63, 392.0].iter().map(|f| (2.0 * std::f64::consts::PI * f * t).sin()).sum::<f64>() * 0.1;
        let v = 0.5 * kick + chord + 0.05 * u;
        data.push(v as f32);
        data.push((v * 0.8) as f32);
    }
    Stereo { data }
}

#[test]
fn features_of_five_minutes() {
    let audio = five_minutes();
    let mono = audio.mono();
    let dur = mono.len() as f64 / 44100.0;
    let bar_times: Vec<f64> = (0..150).map(|i| i as f64 * 2.0).collect();
    // 1 回目はスレッドプールの立ち上げなどを含むので捨てる
    let _ = features::track_extra(&mono[..44100], 44100);
    let t0 = Instant::now();
    let (lufs, peak) = features::loudness(&audio, 44100);
    let t1 = Instant::now();
    let wave = features::waveform(&mono, 44100);
    let t2 = Instant::now();
    let (feats, keys) = features::bar_features(&mono, 44100, &bar_times, dur);
    let t3 = Instant::now();
    let extra = features::track_extra(&mono, 44100);
    let t4 = Instant::now();
    let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
    println!(
        "5 分の曲: loudness {:.0}ms / waveform {:.0}ms / bar_features {:.0}ms / track_extra {:.0}ms / 合計 {:.0}ms（{} スレッド）",
        ms(t0, t1),
        ms(t1, t2),
        ms(t2, t3),
        ms(t3, t4),
        ms(t0, t4),
        rayon::current_num_threads()
    );
    println!(
        "  lufs {lufs:.2} peak {peak:.2} wave {} 点 bars {} key {:?} extra {extra:?}",
        wave.len() / 3,
        feats.len(),
        keys.all.key
    );
    assert_eq!(wave.len(), 3 * 60000);
    assert_eq!(feats.len(), 150);
    if !cfg!(debug_assertions) {
        assert!(ms(t0, t4) < 2000.0, "遅すぎる: {:.0}ms", ms(t0, t4));
    }
}
