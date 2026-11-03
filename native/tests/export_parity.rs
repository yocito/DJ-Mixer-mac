//! Python 版（djmix/export/render.py）との一致: 操作の表（build_plan）とミキサー（Channel・Echo・Compressor・Limiter）。
//! 期待値は tests/fixtures/export/gen_fixtures.py で作った（入力は両方で同じ式から作る）。
mod export_common;
use djmix::analysis::types::CutBar;
use djmix::export::mixer::{hp_freq, per_sample, rbj, xfade_gains, Channel, Compressor, Echo, Limiter, Q_B};
use djmix::export::render::{build_plan, BeatMap, Plan, Track, STEP};
use serde_json::{Map, Value};
use std::path::PathBuf;

fn fixture(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export").join(name);
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

fn f64s(v: &Value) -> Vec<f64> {
    v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect()
}

fn close(a: f64, b: f64, rel: f64) -> bool {
    (a - b).abs() <= rel * (1.0 + a.abs().max(b.abs()))
}

/// gen_fixtures.gen_input と同じ式（インターリーブのステレオ）
fn gen_input(n: usize, seed: u64, amp: f64) -> Vec<f64> {
    let mut x = vec![0.0; n * 2];
    let mut s = seed;
    for i in 0..n {
        s = (s * 1103515245 + 12345) % 2147483648;
        let r = s as f64 / 2147483648.0 - 0.5;
        let fi = i as f64;
        for c in 0..2 {
            let cf = c as f64;
            let v = (0.5 * (fi * 0.0123 * (cf + 1.0)).sin() + 0.3 * (fi * 0.31 + cf).sin() + 0.2 * (fi * 1.7 + 2.0 * cf).sin()) + 0.4 * r;
            x[2 * i + c] = amp * v * (1.0 + 0.8 * (fi * 0.0007).sin());
        }
    }
    x
}

fn check_sample(name: &str, y: &[f64], fx: &Value, rel: f64) {
    let n = fx["n"].as_u64().unwrap() as usize;
    assert_eq!(y.len() / 2, n, "{name}: 長さ");
    let every = fx["every"].as_u64().unwrap() as usize;
    let pick = f64s(&fx["pick"]);
    let mut worst = 0.0f64;
    for (j, i) in (0..n).step_by(every).enumerate() {
        for c in 0..2 {
            let (a, b) = (y[2 * i + c], pick[2 * j + c]);
            worst = worst.max((a - b).abs());
            assert!(close(a, b, rel), "{name}: [{i}][{c}] {a} != {b}");
        }
    }
    let sum: f64 = y.iter().sum();
    let sumsq: f64 = y.iter().map(|v| v * v).sum();
    let absmax = y.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    assert!(close(sum, fx["sum"].as_f64().unwrap(), rel * 10.0), "{name}: sum {sum} != {}", fx["sum"]);
    assert!(close(sumsq, fx["sumsq"].as_f64().unwrap(), rel * 10.0), "{name}: sumsq");
    assert!(close(absmax, fx["absmax"].as_f64().unwrap(), rel), "{name}: absmax");
    println!("{name}: {n} samples, max |diff| of picks {worst:.3e}");
}

#[test]
fn mixer_matches_python() {
    let fx = fixture("mixer_py.json");
    // 係数
    let pairs = [
        ("lp", rbj("lowpass", 250.0, Q_B)),
        ("hp", rbj("highpass", 1234.5, djmix::export::mixer::q_f())),
        ("ap", rbj("allpass", 2500.0, Q_B)),
        ("lp_nyq", rbj("lowpass", 30000.0, 1.0)),
    ];
    for (k, sos) in pairs {
        let want = f64s(&fx["rbj"][k][0]);
        for i in 0..6 {
            assert!(close(sos[i], want[i], 1e-14), "rbj {k}[{i}] {} != {}", sos[i], want[i]);
        }
    }
    let want = f64s(&fx["hp_freq"]);
    for (i, v) in [0.0, 0.01, 0.05, 0.1, 0.3, 0.5, 0.9, 1.0].iter().enumerate() {
        assert!(close(hp_freq(*v), want[i], 1e-13));
    }
    let xs: Vec<f64> = (0..11).map(|i| i as f64 / 10.0).collect();
    for c in ["smooth", "linear", "cut"] {
        let (a, b) = xfade_gains(&xs, c);
        let wa = f64s(&fx["xfade"][c][0]);
        let wb = f64s(&fx["xfade"][c][1]);
        for i in 0..11 {
            assert!(close(a[i], wa[i], 1e-12) && close(b[i], wb[i], 1e-12), "xfade {c} {i}");
        }
    }
    let s = STEP;
    let steps = 40;
    // Channel（3 回に分けて状態を引き継ぐ）
    let mut ch = Channel::new();
    let x = gen_input(3 * steps * s, 7, 0.4);
    let mut y = vec![];
    for c in 0..3 {
        let ks: Vec<f64> = ((c * steps)..((c + 1) * steps)).map(|k| k as f64).collect();
        let low: Vec<f64> = ks.iter().map(|k| (1.0 - k / 80.0).clamp(0.0, 1.0)).collect();
        let hp: Vec<f64> = ks.iter().map(|&k| if k >= 50.0 { (k - 50.0) / 70.0 } else { 0.0 }).collect();
        y.extend(ch.process(&x[c * steps * s * 2..(c + 1) * steps * s * 2], 1.3, &low, &hp));
    }
    check_sample("channel", &y, &fx["channel"], 1e-9);
    // Echo
    let mut e = Echo::new();
    let x = gen_input(4 * steps * s, 11, 0.3);
    let mut y = vec![];
    for c in 0..4 {
        let ks: Vec<f64> = ((c * steps)..((c + 1) * steps)).map(|k| k as f64).collect();
        let delay: Vec<f64> = ks.iter().map(|k| 0.3 + 0.2 * (k / 9.0).sin()).collect();
        let mut src = x[c * steps * s * 2..(c + 1) * steps * s * 2].to_vec();
        for (i, v) in src.iter_mut().enumerate() {
            if ks[i / 2 / s] >= 70.0 {
                *v = 0.0;
            }
        }
        y.extend(e.process(&src, &delay));
    }
    check_sample("echo", &y, &fx["echo"], 1e-9);
    // Compressor・Limiter
    let x = gen_input(3 * steps * s, 3, 1.6);
    let mut comp = Compressor::new();
    let mut y = vec![];
    for c in 0..3 {
        y.extend(comp.process(&x[c * steps * s * 2..(c + 1) * steps * s * 2]));
    }
    check_sample("compressor", &y, &fx["compressor"], 1e-9);
    for (name, ceil, amp) in [("limiter", -0.3, 1.0), ("limiter_mp3", -1.0, 0.5)] {
        let mut lim = Limiter::new(ceil);
        let mut y = vec![];
        for c in 0..3 {
            let seg: Vec<f64> = x[c * steps * s * 2..(c + 1) * steps * s * 2].iter().map(|v| v * amp).collect();
            y.extend(lim.process(&seg));
        }
        y.extend(lim.flush());
        check_sample(name, &y, &fx[name], 1e-9);
        let mx = y.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(mx <= 10f64.powf(ceil / 20.0) + 1e-9);
    }
    // per_sample の形
    let ps = per_sample(&[0.0, 1.0]);
    assert_eq!(ps.len(), 2 * STEP);
    assert_eq!(ps[STEP / 2], 0.5);
    assert_eq!(ps[STEP], 1.0);
    assert_eq!(ps[2 * STEP - 1], 1.0);
}

// ---------------------------------------------------------------- 操作の表
fn tri(x: f64) -> f64 {
    let f = x - x.floor();
    if f < 0.5 {
        4.0 * f - 1.0
    } else {
        3.0 - 4.0 * f
    }
}

fn beats_of(bpm: f64, seconds: f64, first: f64, wobble: f64, per: f64) -> Vec<f64> {
    let mut out = vec![];
    let (mut b, mut j) = (first, 0usize);
    while b < seconds - 0.3 {
        out.push(b);
        b += 60.0 / bpm * (1.0 + wobble * tri(j as f64 / per));
        j += 1;
    }
    out
}

fn tracks_of(specs: &Value) -> Vec<Track> {
    let mut out = vec![];
    for (i, s) in specs.as_array().unwrap().iter().enumerate() {
        let sp = &s["spec"];
        let g = |k: &str| sp[k].as_f64().unwrap();
        let meter = sp["meter"].as_u64().unwrap() as usize;
        let beats = beats_of(g("bpm"), g("seconds"), g("first"), g("wobble"), g("per"));
        let bars: Vec<f64> = beats.iter().step_by(meter).copied().collect();
        let cut_bars: Vec<CutBar> = match sp["cut_db"].as_f64() {
            Some(cd) => bars.iter().enumerate().map(|(k, &t)| CutBar { t, db: -12.0 + 6.0 * tri(k as f64 * 0.13) + cd }).collect(),
            None => vec![],
        };
        let no_beat: Vec<(f64, f64)> = s["no_beat"].as_array().unwrap().iter().map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap())).collect();
        let bm = BeatMap::from_parts(beats, meter as u32, bars, no_beat);
        let mut item: Map<String, Value> = s["item"].as_object().unwrap().clone();
        item.insert("track_id".into(), (i + 1).into());
        item.insert("path".into(), "".into());
        let mut t = Track::new(i, item, PathBuf::new(), bm, s["t0"].as_f64().unwrap(), s["t1"].as_f64().unwrap(), s["lufs"].as_f64().unwrap());
        t.title = format!("t{i}");
        t.artist = format!("a{i}");
        t.album = if i % 2 == 1 { format!("al{i}") } else { String::new() };
        t.cut_bars = cut_bars;
        t.parts_ok = s["parts_ok"].as_bool().unwrap();
        out.push(t);
    }
    out
}

fn col(plan: &Plan, name: &str) -> Vec<Vec<f64>> {
    // 刻みごとの値（pg は 4 つ）
    let (k, d) = name.split_at(name.len() - 1);
    let d: usize = d.parse().unwrap_or(0);
    let one = |v: Vec<f64>| v.into_iter().map(|x| vec![x]).collect::<Vec<_>>();
    match k {
        "track" => one(plan.track[d].iter().map(|&v| v as f64).collect()),
        "pos" => one(plan.pos[d].clone()),
        "low" => one(plan.low[d].clone()),
        "hp" => one(plan.hp[d].clone()),
        "echo" => one(plan.echo[d].clone()),
        "echo_delay" => one(plan.echo_delay[d].clone()),
        "pm" => one(plan.pm[d].iter().map(|&v| v as f64).collect()),
        "jg" => one(plan.jg[d].clone()),
        "pg" => plan.pg[d].iter().map(|g| g.to_vec()).collect(),
        _ => one(plan.xf.clone()),
    }
}

#[test]
fn plan_matches_python() {
    let fx = fixture("plan_py.json");
    for (name, case) in fx.as_object().unwrap() {
        let mut tracks = tracks_of(&case["tracks"]);
        let st = case["settings"].as_object().unwrap().clone();
        let plan = build_plan(&mut tracks, &st);
        assert_eq!(plan.n as u64, case["n"].as_u64().unwrap(), "{name}: 刻みの数");
        let every = case["every"].as_u64().unwrap() as usize;
        let w: Vec<f64> = (0..plan.n).map(|k| (k % 7 + 1) as f64).collect();
        for (cname, want) in case["cols"].as_object().unwrap() {
            let c = col(&plan, cname);
            let pick = f64s(&want["pick"]);
            let flat: Vec<f64> = c.iter().step_by(every).flat_map(|r| r.iter().copied()).collect();
            assert_eq!(flat.len(), pick.len(), "{name}/{cname}: 間引いた数");
            for (i, (a, b)) in flat.iter().zip(&pick).enumerate() {
                assert!(close(*a, *b, 1e-9), "{name}/{cname}[{i}]: {a} != {b}");
            }
            let sum: f64 = c.iter().flat_map(|r| r.iter()).sum();
            let wsum: f64 = c.iter().enumerate().map(|(k, r)| r.iter().sum::<f64>() * w[k]).sum();
            assert!(close(sum, want["sum"].as_f64().unwrap(), 1e-9), "{name}/{cname}: sum {sum} != {}", want["sum"]);
            assert!(close(wsum, want["wsum"].as_f64().unwrap(), 1e-9), "{name}/{cname}: wsum");
        }
        let cuts = case["cuts"].as_array().unwrap();
        assert_eq!(plan.cuts.len(), cuts.len(), "{name}: cuts");
        for (a, b) in plan.cuts.iter().zip(cuts) {
            assert_eq!(a.k as u64, b["k"].as_u64().unwrap());
            assert_eq!(a.out as u64, b["out"].as_u64().unwrap());
            assert!(close(a.f, b["f"].as_f64().unwrap(), 1e-9));
        }
        let marks = case["marks"].as_array().unwrap();
        assert_eq!(plan.marks.len(), marks.len(), "{name}: marks");
        for (a, b) in plan.marks.iter().zip(marks) {
            assert!(close(a.t, b["t"].as_f64().unwrap(), 1e-9), "{name}: mark t");
            assert_eq!(a.title, b["title"].as_str().unwrap());
            assert_eq!(a.artist, b["artist"].as_str().unwrap());
            assert_eq!(a.album, b["album"].as_str().unwrap());
        }
        let shifts = f64s(&case["shift"]);
        for (t, s) in tracks.iter().zip(shifts) {
            assert_eq!(t.shift, s, "{name}: shift");
        }
        println!("{name}: {} steps, {} cuts, {} marks, {} wraps — ok", plan.n, plan.cuts.len(), plan.marks.len(), plan.wraps.len());
    }
}
