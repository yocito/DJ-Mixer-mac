//! デッキの音（stretch.rs）: 操作の表の位置どおりに鳴るか（ストレッチの遅れの補正）、区切りで音量が落ちないか、ループの折り返し。
mod export_common;
use djmix::export::render::{Plan, Wrap, DT, STEP};
use djmix::export::stretch::{default_stem_reader, DeckAudio};
use export_common::*;
use serde_json::json;

/// デッキ 0 だけが曲 0 を鳴らす表（位置は pos(k)）
fn plan_with(n: usize, pos: impl Fn(usize) -> f64) -> Plan {
    let mut p = Plan { n, ..Default::default() };
    for d in 0..2 {
        p.track[d] = vec![if d == 0 { 0 } else { -1 }; n];
        p.pos[d] = (0..n).map(|k| if d == 0 { pos(k) } else { 0.0 }).collect();
        p.low[d] = vec![1.0; n];
        p.hp[d] = vec![0.0; n];
        p.echo[d] = vec![0.0; n];
        p.echo_delay[d] = vec![0.3; n];
        p.pm[d] = vec![0; n];
        p.pg[d] = vec![[1.0; 4]; n];
        p.jg[d] = vec![1.0; n];
    }
    p.xf = vec![0.0; n];
    p
}

fn left(x: &[f32]) -> Vec<f64> {
    x.chunks_exact(2).map(|c| c[0] as f64).collect()
}

/// クリックの頭のずれ（ms）: 鳴った時刻 − 位置から求めた時刻（元の音で測った検出の偏りを引く）
fn click_errors(out: &[f64], src: &[f64], clicks: &[f64], expect: impl Fn(f64) -> Option<f64>) -> Vec<f64> {
    let mut errs = vec![];
    for &c in clicks {
        let Some(t) = expect(c) else { continue };
        let Some(l) = xcorr_lag(out, t, src, c, 0.004) else { continue };
        errs.push(l * 1000.0);
    }
    errs
}

fn stats(e: &[f64]) -> (f64, f64) {
    let m = e.iter().sum::<f64>() / e.len() as f64;
    let mx = e.iter().fold(0.0f64, |a, v| a.max(v.abs()));
    (m, mx)
}

#[test]
fn stretch_follows_plan_positions() {
    let dir = tmpdir();
    let path = dir.path().join("a.wav");
    let bpm = 120.0;
    let src_st = click_samples(bpm, 4, 30.0, 0.25, 0.02);
    write_wav(&path, &src_st);
    let src: Vec<f64> = src_st.chunks_exact(2).map(|c| c[0] as f64).collect();
    let clicks: Vec<f64> = (0..58).map(|i| 0.25 + i as f64 * 0.5).collect();
    let tk = synth_track(0, bpm, 30.0, Some(&path), 0.0, None, json!({}));
    let tracks = vec![tk];
    for &(rate, p0) in &[(1.0, 2.0), (0.94, 3.1), (1.06, 1.37), (0.75, 2.5)] {
        let n = (20.0 / DT) as usize;
        let plan = plan_with(n, |k| p0 + k as f64 * rate * DT);
        let mut deck = DeckAudio::new(0, &tracks, &plan, default_stem_reader());
        let mut y = vec![];
        let mut c0 = 0;
        while c0 < n {
            let c1 = n.min(c0 + 176);
            y.extend(deck.render(c0, c1).unwrap());
            c0 = c1;
        }
        let out = left(&y);
        let errs = click_errors(&out, &src, &clicks, |c| {
            let t = (c - p0) / rate;
            (0.05..19.9).contains(&t).then_some(t)
        });
        let (m, mx) = stats(&errs);
        println!("rate {rate}: {} clicks, mean {m:.3} ms, max |err| {mx:.3} ms", errs.len());
        assert!(errs.len() > 20);
        assert!(m.abs() < 0.5 && mx < 1.0, "rate {rate}: mean {m} max {mx}");
    }
}

#[test]
fn varying_rate_keeps_level_and_position() {
    // 速度が変わり続けても（拍を合わせている間・原速へ戻す間）、区切りで音量が落ちず、位置どおりに鳴る
    let dir = tmpdir();
    let path = dir.path().join("s.wav");
    let n_src = 40 * SR as usize;
    let mut st = Vec::with_capacity(n_src * 2);
    for i in 0..n_src {
        let v = 0.3 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / SR as f64).sin() as f32;
        st.push(v);
        st.push(v);
    }
    write_wav(&path, &st);
    let tracks = vec![synth_track(0, 120.0, 40.0, Some(&path), 0.0, None, json!({}))];
    let n = (25.0 / DT) as usize;
    // 0.92 → 1.08 → 0.92 と揺らす
    let mut pos = vec![1.0];
    for k in 1..n {
        let r = 1.0 + 0.08 * (k as f64 / 400.0).sin();
        pos.push(pos[k - 1] + r * DT);
    }
    let plan = plan_with(n, |k| pos[k]);
    let mut deck = DeckAudio::new(0, &tracks, &plan, default_stem_reader());
    let y = deck.render(0, n).unwrap();
    let out = left(&y);
    let rms: Vec<f64> = out.chunks_exact(STEP).map(|c| (c.iter().map(|v| v * v).sum::<f64>() / STEP as f64).sqrt()).collect();
    let body = &rms[20..rms.len() - 20];
    let lo = body.iter().cloned().fold(f64::MAX, f64::min);
    let hi = body.iter().cloned().fold(0.0, f64::max);
    let db = 20.0 * (hi / lo).log10();
    println!("level range {db:.3} dB (rms {lo:.4}..{hi:.4})");
    assert!(db < 0.5, "音量が揺れる: {db} dB");
    assert!((lo / (0.3 / 2f64.sqrt()) - 1.0).abs() < 0.06);
}

#[test]
fn loop_wrap_is_seamless_and_aligned() {
    // ループの折り返し（最後の 1 小節を繰り返す）: 折り返しの後もクリックが位置どおりに鳴る
    let dir = tmpdir();
    let path = dir.path().join("a.wav");
    let src_st = click_samples(120.0, 4, 30.0, 0.25, 0.02);
    write_wav(&path, &src_st);
    let src: Vec<f64> = src_st.chunks_exact(2).map(|c| c[0] as f64).collect();
    let clicks: Vec<f64> = (0..58).map(|i| 0.25 + i as f64 * 0.5).collect();
    let tracks = vec![synth_track(0, 120.0, 30.0, Some(&path), 0.0, None, json!({}))];
    let (ls, le) = (8.25, 10.25); // 1 小節（2 秒）
    let rate = 0.97;
    let n = (14.0 / DT) as usize;
    let mut pos = vec![5.0];
    let mut wraps = vec![];
    for k in 1..n {
        let mut p = pos[k - 1] + rate * DT;
        if ls <= pos[k - 1] && p >= le && wraps.len() < 2 {
            p -= le - ls;
            wraps.push(Wrap { deck: 0, k, start: ls, end: le });
        }
        pos.push(p);
    }
    let mut plan = plan_with(n, |k| pos[k]);
    plan.wraps = wraps.clone();
    let mut deck = DeckAudio::new(0, &tracks, &plan, default_stem_reader());
    let out = left(&deck.render(0, n).unwrap());
    // 鳴った時刻 t → 入力位置（刻みの間は直線）。クリック c が鳴る t を全部探す
    let mut errs = vec![];
    for k in 1..n {
        let (a, b) = (pos[k - 1], pos[k]);
        if b < a {
            continue;
        }
        for &c in &clicks {
            if a <= c && c < b {
                let t = ((k - 1) as f64 + (c - a) / (b - a)) * DT;
                if t > 0.05 {
                    let Some(l) = xcorr_lag(&out, t, &src, c, 0.004) else { continue };
                    errs.push(l * 1000.0);
                }
            }
        }
    }
    let (m, mx) = stats(&errs);
    println!("loop: {} clicks, mean {m:.3} ms, max {mx:.3} ms, wraps {}", errs.len(), wraps.len());
    assert_eq!(wraps.len(), 2);
    assert!(errs.len() > 15 && m.abs() < 0.5 && mx < 1.0);
}
