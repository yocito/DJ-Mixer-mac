//! P3-8: 高速書き出し（tests/test_render.py の移植）。一定テンポの合成の曲で、拍の写像・原速へ戻す・リミッター・パートごとの音を確かめる。
#![allow(clippy::needless_range_loop)]
mod export_common;
use djmix::export::mixer::Mixdown;
use djmix::export::render::{build_plan, segments_of, RenderJob, DT, STEP};
use djmix::export::stretch::StemReader;
use djmix::export::transition as T;
use djmix::export::writer::tracklist_text;
use export_common::*;
use serde_json::{json, Value};
use std::sync::Arc;

fn both_steps(plan: &djmix::export::render::Plan) -> Vec<usize> {
    (0..plan.n).filter(|&s| plan.track[0][s] >= 0 && plan.track[1][s] >= 0).collect()
}

fn ptp(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
}

#[test]
fn plan_locks_beats_and_returns_to_original_speed() {
    let a = synth_track(0, 120.0, 120.0, None, 10.0, Some(100.0), json!({"join_tpl": "long", "join_bars": 16}));
    let b = synth_track(1, 126.0, 150.0, None, 5.0, Some(140.0), json!({}));
    let mut tracks = vec![a, b];
    let plan = build_plan(&mut tracks, &settings(json!({"tempo_range": 0.08, "tempo_return": true})));
    let (a, b) = (&tracks[0], &tracks[1]);
    let both = both_steps(&plan);
    assert!(!both.is_empty(), "重なりが無い");
    // 重なりの間、次の曲の拍位置 − 前の曲の拍位置 が一定の整数（位相がそろっている）
    let d: Vec<f64> = both.iter().map(|&s| b.bm.pos(plan.pos[1][s]) - a.bm.pos(plan.pos[0][s])).collect();
    assert!(ptp(&d) < 1e-6 && (d[0] - d[0].round()).abs() < 1e-6);
    // 重なりの長さは 16 小節（前の曲の拍で 64 拍）
    let (f, l) = (both[0], *both.last().unwrap());
    assert!((a.bm.pos(plan.pos[0][l]) - a.bm.pos(plan.pos[0][f]) - 64.0).abs() < 0.2);
    // 繋いだ後、次の曲の速度は 120/126 から 8 小節かけて 1 に戻り、範囲の終わりで止まる
    let solo: Vec<usize> = (l + 1..plan.n).filter(|&s| plan.track[1][s] >= 0).collect();
    let rate: Vec<f64> = solo.windows(2).map(|w| (plan.pos[1][w[1]] - plan.pos[1][w[0]]) / DT).collect();
    assert!((rate[0] - 120.0 / 126.0).abs() < 0.01 && (rate.last().unwrap() - 1.0).abs() < 1e-6);
    assert!((plan.pos[1][plan.n - 1] - 140.0).abs() < 0.05);
    let titles: Vec<&str> = plan.marks.iter().map(|m| m.title.as_str()).collect();
    assert_eq!(titles, ["t0", "t1"]);
}

#[test]
fn echo_join_enters_at_end() {
    let mut tracks = vec![
        synth_track(0, 120.0, 120.0, None, 10.0, Some(100.0), json!({"join_tpl": "echo"})),
        synth_track(1, 120.0, 150.0, None, 5.0, Some(60.0), json!({})),
    ];
    let plan = build_plan(&mut tracks, &settings(json!({"tempo_range": 0.08})));
    let first_b = (0..plan.n).find(|&s| plan.track[1][s] >= 0).unwrap();
    let last_a = (0..plan.n).filter(|&s| plan.track[0][s] >= 0).max().unwrap();
    assert!((first_b as i64 - last_a as i64).abs() <= 2); // 前の曲の終わりで入る
    assert!(plan.echo[0].iter().cloned().fold(0.0, f64::max) > 0.9); // 最後の小節でエコーを上げている
}

fn run_mix(m: &mut Mixdown) -> Vec<u8> {
    let mut out = vec![];
    m.run(&mut |b: &[u8]| out.extend_from_slice(b), &mut |_| true).unwrap();
    out
}

#[test]
fn mixdown_length_and_ceiling() {
    let dir = tmpdir();
    let (pa, pb) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    click_track(&pa, 120.0, 40.0);
    click_track(&pb, 124.0, 40.0);
    let mut a = synth_track(0, 120.0, 40.0, Some(&pa), 2.0, Some(30.0), json!({"join_tpl": "short", "join_bars": 8}));
    let mut b = synth_track(1, 124.0, 40.0, Some(&pb), 1.0, Some(30.0), json!({}));
    a.lufs = -30.0; // 大きく持ち上げさせて、リミッターを働かせる
    b.lufs = -30.0;
    let mut tracks = vec![a, b];
    let st = settings(json!({"tempo_range": 0.08, "lufs_target": -14, "xfade_curve": "smooth"}));
    let plan = build_plan(&mut tracks, &st);
    let mut m = Mixdown::new(&tracks, &plan, &st, false, -0.3);
    let (l, r) = pcm_to_lr(&run_mix(&mut m));
    assert!((l.len() as i64 - (plan.n * STEP) as i64).abs() <= STEP as i64);
    let mx = l.iter().chain(&r).fold(0.0f64, |m, v| m.max(v.abs()));
    assert!(mx <= 10f64.powf(-0.3 / 20.0) + 1e-3, "0dBFS を超えない（−0.3dBFS で止める）: {mx}");
    assert!(mx > 0.5);
}

#[test]
fn levels_match_fixture() {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../frontend/tests/levels_fixture.json");
    let fx: Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    let cases = fx["cases"].as_array().unwrap();
    for c in cases {
        let j = T::join_of(c["item"].as_object());
        let lv = T::levels(&j, c["u"].as_f64().unwrap(), c["beats"].as_f64().unwrap());
        let got = serde_json::to_value(lv).unwrap();
        for k in ["xf", "lowOut", "lowIn", "hpOut", "echoOut"] {
            // serde_json の既定の数の読み込みは最後の 1 桁（1 ulp）ずれることがあるので、その分だけ許す（Python では完全に一致）
            let (a, b) = (got[k].as_f64().unwrap(), c["out"][k].as_f64().unwrap());
            assert!((a - b).abs() <= 4.0 * f64::EPSILON * a.abs().max(b.abs()), "{k}: {a} != {b}: {c}");
        }
    }
    println!("{} cases", cases.len());
}

#[test]
fn plan_ramp_loop_shift() {
    let mut tracks = vec![
        synth_track(0, 120.0, 120.0, None, 10.0, Some(100.0), json!({"join_tpl": "long", "join_bars": 16,
            "join_params": {"tempo": "ramp", "loop": 4, "shift": 1}})),
        synth_track(1, 126.0, 150.0, None, 5.0, Some(140.0), json!({})),
    ];
    let plan = build_plan(&mut tracks, &settings(json!({"tempo_range": 0.08, "tempo_return": true})));
    let b = &tracks[1];
    let both = both_steps(&plan);
    let pa: Vec<f64> = both.iter().map(|&s| plan.pos[0][s]).collect();
    assert_eq!(pa.windows(2).filter(|w| w[1] - w[0] < -1.0).count(), 1); // ループで 1 回巻き戻る（最後の 4 小節を繰り返す）
    let rb: Vec<f64> = both.windows(2).map(|w| (plan.pos[1][w[1]] - plan.pos[1][w[0]]) / DT).collect();
    assert!((rb.last().unwrap() - 1.0).abs() < 0.005 && rb[0] < 0.97); // テンポランプ: 次の曲は重なりの終わりで原速
    // 次の曲の拍の進みは、前の曲の 16 小節（64 拍）ぶん
    let (f, l) = (both[0], *both.last().unwrap());
    assert!((b.bm.pos(plan.pos[1][l]) - b.bm.pos(plan.pos[1][f]) - 64.0).abs() < 0.5);
    assert_eq!(b.shift, 1.0); // キー合わせ
    let solo: Vec<usize> = (l + 1..plan.n).filter(|&s| plan.track[1][s] >= 0).take(50).collect();
    let mean = (plan.pos[1][solo[solo.len() - 1]] - plan.pos[1][solo[0]]) / (solo.len() - 1) as f64 / DT;
    assert!((mean - 1.0).abs() < 0.002); // 繋いだ後に原速へ戻す動きは無い
    // ループの折り返しは記録してある（ストレッチがつなぐのに使う）
    assert!(plan.wraps.iter().any(|w| w.deck == 0 && both.contains(&w.k)));
}

// ---- パートごとに鳴らす（Phase 5 B2） ----
#[test]
fn part_gains() {
    let j = T::join_of(json!({"join_tpl": "long", "join_params": {"parts": "swap"}}).as_object());
    let (g0, g1) = (T::part_gains(&j, 0.0, 64.0).unwrap(), T::part_gains(&j, 1.0, 64.0).unwrap());
    assert!(g0.out == [1.0; 4] && g0.inn == [0.0, 1.0, 0.0, 0.0]); // 前の曲はそのまま、次の曲はドラムだけ
    assert!(g1.inn == [1.0; 4] && g1.out[0] == 0.0 && g1.out[2] == 0.0);
    assert!((T::part_gains(&j, 0.125, 64.0).unwrap().out[0] - 0.5).abs() < 1e-9); // 歌は最初の 1/4 で消す
    assert!(T::levels(&j, 0.9, 64.0).low_in == 1.0 && T::levels(&j, 0.1, 64.0).low_out == 1.0); // EQ の LOW は動かさない
    assert!(T::part_gains(&T::join_of(json!({"join_tpl": "echo", "join_params": {"parts": "swap"}}).as_object()), 0.5, 64.0).is_none());
    assert!(T::part_gains(&T::join_of(json!({"join_tpl": "long"}).as_object()), 0.5, 64.0).is_none());
}

/// 分離した音の代わりに「元の音の 1/4 × 4 パート」を返す
fn quarter_stems(paths: Vec<std::path::PathBuf>) -> StemReader {
    let audio: Vec<Vec<f32>> = paths.iter().map(|p| djmix::audio::decode::decode(p).unwrap().data).collect();
    Arc::new(move |tid: i64, start: usize, frames: usize| {
        let a = &audio[(tid - 1) as usize];
        let total = a.len() / 2;
        let mut out = vec![vec![0.0f32; frames]; 8];
        for i in 0..frames {
            let f = start + i;
            if f < total {
                for s in 0..4 {
                    out[2 * s][i] = a[2 * f] * 0.25;
                    out[2 * s + 1][i] = a[2 * f + 1] * 0.25;
                }
            }
        }
        Ok((out, total))
    })
}

fn rms(v: &[f64]) -> f64 {
    (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt()
}

fn corr(a: &[f64], b: &[f64]) -> f64 {
    let (ma, mb) = (a.iter().sum::<f64>() / a.len() as f64, b.iter().sum::<f64>() / b.len() as f64);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma) * (x - ma);
        sbb += (y - mb) * (y - mb);
    }
    sab / (saa * sbb).sqrt()
}

#[test]
fn mixdown_with_parts() {
    // パートの合計 = 元の音。パートを分けない繋ぎと比べて、重なりの外は同じ、重なりの前半は次の曲が小さい（ドラムだけ）
    let dir = tmpdir();
    let (pa, pb) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    click_track(&pa, 120.0, 40.0);
    click_track(&pb, 120.0, 40.0);
    let reader = quarter_stems(vec![pa.clone(), pb.clone()]);
    let st = settings(json!({"tempo_range": 0.08, "lufs_target": -14, "xfade_curve": "smooth"}));
    let mix = |parts: &str| {
        let mut a = synth_track(0, 120.0, 40.0, Some(&pa), 2.0, Some(30.0), json!({"join_tpl": "long", "join_bars": 8, "join_params": {"parts": parts}}));
        let mut b = synth_track(1, 120.0, 40.0, Some(&pb), 1.0, Some(30.0), json!({}));
        a.parts_ok = true;
        b.parts_ok = true;
        let mut tracks = vec![a, b];
        let plan = build_plan(&mut tracks, &st);
        let mut m = Mixdown::with_stems(&tracks, &plan, &st, true, -0.3, reader.clone());
        let y = pcm_to_lr(&run_mix(&mut m));
        (plan, y)
    };
    let (p0, y0) = mix("none");
    let (p1, y1) = mix("drums");
    let both = both_steps(&p1);
    let (f, l) = (both[0], *both.last().unwrap());
    assert!(p1.pm[1][f] == 1 && p1.pm[1][l + 2] == 0 && p0.pm[1].iter().all(|&v| v == 0));
    let s = STEP;
    // 重なりの前は同じ
    let before = 0..(f * s - s * 200);
    let dmax = before.clone().map(|i| (y0.0[i] - y1.0[i]).abs().max((y0.1[i] - y1.1[i]).abs())).fold(0.0, f64::max);
    assert!(dmax < 1e-3, "重なりの前が違う: {dmax}");
    let early = (f * s + s * 20)..(f * s + (l - f) * s / 5);
    let after = ((l + 300) * s)..(y0.0.len().min(y1.0.len()) - s);
    // 比べるのは同じ曲の重なりの後（全パート・EQ もそのまま）
    let r = rms(&y1.1[early]) / rms(&y1.1[after.clone()]);
    println!("drums only / full = {r:.3}");
    assert!(0.2 < r && r < 0.3, "次の曲はドラムだけ（1/4）: {r}");
    let c = corr(&y0.1[after.clone()], &y1.1[after]);
    assert!(c > 0.98, "重なりの後は元の音に戻る: {c}");
}

#[test]
fn tracklist_text_has_album() {
    let t = tracklist_text(&[
        json!({"t": 0, "title": "CONTROL", "artist": "T-SQUARE", "album": "Wave"}),
        json!({"t": 245.4, "title": "X", "artist": ""}),
        json!({"t": 3725, "title": "Y", "artist": "Z", "album": null}),
    ]);
    assert_eq!(t.lines().collect::<Vec<_>>(), ["00:00  CONTROL - T-SQUARE 『Wave』", "04:05  X", "1:02:05  Y - Z"]);
}

#[test]
fn segments_are_contiguous_per_deck() {
    let dir = tmpdir();
    let (pa, pb) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    click_track(&pa, 120.0, 40.0);
    click_track(&pb, 124.0, 40.0);
    let mut tracks = vec![
        synth_track(0, 120.0, 40.0, Some(&pa), 2.0, Some(30.0), json!({"join_tpl": "short", "join_bars": 8})),
        synth_track(1, 124.0, 40.0, Some(&pb), 1.0, Some(30.0), json!({})),
    ];
    let st = settings(json!({"tempo_range": 0.08, "lufs_target": -14, "xfade_curve": "smooth"}));
    let plan = build_plan(&mut tracks, &st);
    let segs = segments_of(&plan, &tracks);
    let kinds: Vec<(usize, &str)> = segs.iter().map(|g| (g.deck, g.kind)).collect();
    assert_eq!(kinds, [(0, "main"), (1, "main")]);
    for g in &segs {
        // 区間の中はずっと同じ曲
        assert!(plan.track[g.deck][g.s0..g.s1].iter().all(|&v| v == g.idx as i32));
        assert!(g.s0 == 0 || plan.track[g.deck][g.s0 - 1] != g.idx as i32);
    }
    // 止まっている刻みの音は 0
    let mut m = Mixdown::new(&tracks, &plan, &st, true, -0.3);
    let x = m.deck_audio(1, 0, plan.n).unwrap();
    for k in 0..plan.n {
        if plan.track[1][k] < 0 {
            assert!(x[k * STEP * 2..(k + 1) * STEP * 2].iter().all(|&v| v == 0.0));
        }
    }
}

#[test]
fn job_flow_writes_file() {
    // 書き出しの流れ: prepare → mix → done、ファイルと曲目リストができ、状態のキーは Python 版と同じ
    let dir = tmpdir();
    let (pa, pb) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    click_track(&pa, 120.0, 30.0);
    click_track(&pb, 120.0, 30.0);
    let tracks = vec![
        synth_track(0, 120.0, 30.0, Some(&pa), 1.0, Some(20.0), json!({"join_tpl": "short", "join_bars": 4})),
        synth_track(1, 120.0, 30.0, Some(&pb), 1.0, Some(20.0), json!({})),
    ];
    let out = dir.path().join("out.wav");
    let job = RenderJob::new(vec![], settings(json!({"tempo_range": 0.08, "lufs_target": -14})), out.clone(), "wav", true);
    let s0 = job.status();
    assert_eq!(s0["stage"], "prepare");
    job.run(Some(tracks));
    let s = job.status();
    assert!(s["error"].is_null(), "{s}");
    assert_eq!(s["stage"], "done");
    assert_eq!(s["done"], true);
    assert_eq!(s["progress"], 1.0);
    for k in ["progress", "message", "error", "done", "result", "eta_sec", "stage", "segments", "segments_done"] {
        assert!(s.get(k).is_some(), "{k}");
    }
    assert!(job.segment_info().is_empty());
    let res = &s["result"];
    assert_eq!(res["path"], out.to_string_lossy().as_ref());
    assert!(out.exists());
    let txt = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert!(txt.starts_with("00:00  t0\n"));
    let secs = res["seconds"].as_f64().unwrap();
    let r = hound::WavReader::open(&out).unwrap();
    assert_eq!(r.spec().channels, 2);
    assert_eq!(r.spec().sample_rate, 44100);
    assert!((r.duration() as f64 / 44100.0 - secs).abs() < 0.06);
    assert!(secs > 30.0 && secs < 40.0, "{secs}");
}

#[test]
fn job_cancel_removes_partial_file() {
    let dir = tmpdir();
    let pa = dir.path().join("a.wav");
    click_track(&pa, 120.0, 30.0);
    let tracks = vec![synth_track(0, 120.0, 30.0, Some(&pa), 1.0, Some(28.0), json!({}))];
    let out = dir.path().join("x.wav");
    let job = RenderJob::new(vec![], settings(json!({})), out.clone(), "wav", true);
    job.cancel();
    job.run(Some(tracks));
    assert_eq!(job.status()["error"], "中止しました");
    assert_eq!(job.stage(), "error");
    assert!(!out.exists());
}
