//! 端から端まで: クリックの曲を 3 曲つないで書き出し（split: 左＝デッキ0・右＝デッキ1）、重なりの間の 2 つのデッキの拍のずれを測る。
#![allow(clippy::needless_range_loop)]
mod export_common;
use djmix::export::mixer::Mixdown;
use djmix::export::render::{build_plan, Plan, Track, DT};
use export_common::*;
use serde_json::json;

/// デッキ d が曲の位置 c を鳴らす時刻（秒。刻みの間は直線）。重なりの刻み [k0, k1) の中だけ
fn times_of(plan: &Plan, d: usize, c: f64, k0: usize, k1: usize) -> Vec<f64> {
    let mut out = vec![];
    for k in k0.max(1)..k1 {
        let (a, b) = (plan.pos[d][k - 1], plan.pos[d][k]);
        if b > a && a <= c && c < b {
            out.push(((k - 1) as f64 + (c - a) / (b - a)) * DT);
        }
    }
    out
}

struct Click {
    t: f64,
    actual: f64,
}

#[allow(clippy::too_many_arguments)]
fn measure(plan: &Plan, d: usize, tk: &Track, src: &[f64], out: &[f64], k0: usize, k1: usize, clicks: &[f64]) -> Vec<Click> {
    let mut res = vec![];
    for &c in clicks {
        for t in times_of(plan, d, c, k0, k1) {
            if t < k0 as f64 * DT + 0.05 || t > k1 as f64 * DT - 0.06 {
                continue;
            }
            if let Some(l) = xcorr_lag(out, t, src, c, 0.004) {
                res.push(Click { t, actual: t + l });
            }
        }
    }
    let _ = tk;
    res
}

#[test]
fn decks_stay_beat_aligned_during_overlap() {
    let dir = tmpdir();
    let specs = [(120.0, 0.25, 60.0, 2.0, 50.0), (126.0, 0.4, 60.0, 1.0, 45.0), (123.0, 0.33, 60.0, 1.0, 50.0)];
    let mut tracks = vec![];
    let mut srcs = vec![];
    let mut clicks = vec![];
    for (i, &(bpm, first, secs, t0, t1)) in specs.iter().enumerate() {
        let p = dir.path().join(format!("t{i}.wav"));
        let st = click_samples(bpm, 4, secs, first, 0.02);
        write_wav(&p, &st);
        srcs.push(st.chunks_exact(2).map(|c| c[0] as f64).collect::<Vec<_>>());
        clicks.push((0..).map(|k| first + k as f64 * 60.0 / bpm).take_while(|&c| c < secs - 0.1).collect::<Vec<_>>());
        let item = if i < 2 { json!({"join_tpl": "long", "join_bars": 8}) } else { json!({}) };
        tracks.push(synth_track_first(i, bpm, secs, Some(&p), t0, Some(t1), item, first));
    }
    let st = settings(json!({"tempo_range": 0.08, "lufs_target": -14, "xfade_curve": "smooth", "tempo_return": true}));
    let plan = build_plan(&mut tracks, &st);
    let mut m = Mixdown::new(&tracks, &plan, &st, true, -0.3);
    let mut pcm = vec![];
    m.run(&mut |b: &[u8]| pcm.extend_from_slice(b), &mut |_| true).unwrap();
    let (l, r) = pcm_to_lr(&pcm);
    let outs = [l, r];
    // 重なり（両方のデッキが鳴っている刻み）ごとに
    let both: Vec<usize> = (0..plan.n).filter(|&s| plan.track[0][s] >= 0 && plan.track[1][s] >= 0).collect();
    let mut groups: Vec<(usize, usize)> = vec![];
    for &s in &both {
        match groups.last_mut() {
            Some(g) if g.1 == s => g.1 = s + 1,
            _ => groups.push((s, s + 1)),
        }
    }
    assert_eq!(groups.len(), 2, "重なりが 2 つ");
    let mut all = vec![];
    let mut per_deck = vec![];
    for &(k0, k1) in &groups {
        let mut ms = vec![];
        for d in 0..2 {
            let ti = plan.track[d][k0] as usize;
            let c = measure(&plan, d, &tracks[ti], &srcs[ti], &outs[d], k0, k1, &clicks[ti]);
            let e: Vec<f64> = c.iter().map(|x| (x.actual - x.t) * 1000.0).collect();
            let rate = (plan.pos[d][k1 - 1] - plan.pos[d][k0]) / ((k1 - 1 - k0) as f64 * DT);
            println!(
                "overlap {k0}..{k1} deck {d} (曲 {ti}, 平均の速度 {rate:.4}): {} clicks, ずれ 平均 {:.3} ms 最大 {:.3} ms",
                e.len(),
                e.iter().sum::<f64>() / e.len() as f64,
                e.iter().fold(0.0f64, |a, v| a.max(v.abs()))
            );
            per_deck.extend(e);
            ms.push(c);
        }
        // 同じ拍（予定の時刻が 3ms 以内）どうしで、鳴った時刻の差
        let mut diffs = vec![];
        for a in &ms[0] {
            if let Some(b) = ms[1].iter().find(|b| (b.t - a.t).abs() < 0.003) {
                diffs.push((b.actual - a.actual) * 1000.0);
                assert!((b.t - a.t).abs() < 1e-4, "表の上で拍がそろっていない: {} {}", a.t, b.t);
            }
        }
        println!("  2 つのデッキの差: {} 拍、平均 {:.3} ms、最大 {:.3} ms", diffs.len(), diffs.iter().sum::<f64>() / diffs.len() as f64,
                 diffs.iter().fold(0.0f64, |m, v| m.max(v.abs())));
        assert!(diffs.len() >= 12);
        all.extend(diffs);
    }
    let mean = all.iter().sum::<f64>() / all.len() as f64;
    let mx = all.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let mut sorted: Vec<f64> = all.iter().map(|v| v.abs()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p95 = sorted[(sorted.len() as f64 * 0.95) as usize - 1];
    println!("inter-deck: {} beats, mean {mean:.3} ms, p95 {p95:.3} ms, max {mx:.3} ms", all.len());
    assert!(mean.abs() < 0.5 && p95 < 1.0 && mx < 1.5, "デッキの拍がずれる: mean {mean} p95 {p95} max {mx}");
    let pm = per_deck.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert!(pm < 1.5, "表の位置からずれる: {pm}");
}
