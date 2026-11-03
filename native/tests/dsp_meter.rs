//! 拍子（bars.rs: estimate_meter, local_baseline, track_meter, bar_phases_varying, meter_summary）が Python 版と一致するか。
//! fixture は tests/fixtures/dsp/make_fixtures.py が作る（meter.json。小節頭らしさは float32、tests/test_meter.py の合成データ）。
mod dsp_common;

use djmix::analysis::bars;
use djmix::analysis::types::Bar;
use dsp_common::*;
use serde_json::Value;

fn bars_of(v: &Value) -> Vec<Bar> {
    let beat = v["beat"].as_array().unwrap();
    let time = f64s(&v["time"]);
    let conf = f64s(&v["conf"]);
    let meter = v.get("meter").and_then(|m| m.as_array());
    (0..beat.len())
        .map(|i| Bar {
            beat: beat[i].as_i64().unwrap(),
            time: time[i],
            conf: conf[i],
            meter: meter.map(|m| m[i].as_u64().unwrap() as u32),
        })
        .collect()
}

fn case_inputs(c: &Value) -> (Vec<f64>, Vec<f64>) {
    let s: Vec<f64> = f32s(&c["strength"]).into_iter().map(|v| v as f64).collect();
    let beats =
        if c["beats"].is_null() { (0..s.len()).map(|i| i as f64 * 0.5 + 1.0).collect() } else { f64s(&c["beats"]) };
    (s, beats)
}

#[test]
fn meter_matches_python() {
    let fx = fixture("meter.json");
    for c in fx["cases"].as_array().unwrap() {
        let name = c["name"].as_str().unwrap();
        let (s, beats) = case_inputs(c);
        let (m, scores) = bars::estimate_meter(&s);
        assert_eq!(m as u64, c["meter"].as_u64().unwrap(), "{name} meter");
        for (k, v) in c["scores"].as_object().unwrap() {
            assert_eq!(scores[k], v.as_f64().unwrap(), "{name} score {k}");
        }
        let path = bars::track_meter(&s);
        let want: Vec<(u32, u32)> = c["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                let x = v.as_u64().unwrap() as u32;
                (x / 10, x % 10)
            })
            .collect();
        assert_eq!(path, want, "{name} path");
        let bl = bars::bar_phases_varying(&beats, &s);
        assert_eq!(bl, bars_of(&c["varying"]), "{name} varying");
        let sm = bars::meter_summary(&bl);
        let ws = &c["summary"];
        assert_eq!(sm.meter as u64, ws["meter"].as_u64().unwrap(), "{name} summary");
        assert_eq!(sm.switches as u64, ws["switches"].as_u64().unwrap(), "{name} summary");
        let counts: Vec<(u32, usize)> = sm.counts.iter().map(|(&a, &b)| (a, b)).collect();
        let mut wc: Vec<(u32, usize)> = ws["counts"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.parse().unwrap(), v.as_u64().unwrap() as usize))
            .collect();
        wc.sort();
        assert_eq!(counts, wc, "{name} counts");
        if let Some(b) = c.get("baseline") {
            let p: Vec<f64> = s.iter().map(|v| v.clamp(1e-6, 1.0 - 1e-6)).collect();
            let got = bars::local_baseline(&p, bars::NEUTRAL_WIN);
            let want = f64s(b);
            assert_eq!(got.len(), want.len());
            for (x, y) in got.iter().zip(&want) {
                assert!((x - y).abs() < 1e-14, "{name} baseline {x} vs {y}");
            }
        }
        // 土台（bars.rs の bar_phases。この作業の前からあるもの）も同じ入力で確かめておく
        let bp = bars::bar_phases(&beats, &s, m);
        assert_eq!(bp, bars_of(&c["bar_phases"]), "{name} bar_phases");
    }
}

/// tests/test_meter.py の test_bar_phases_varying_and_summary（同じ合成データの fixture で）
#[test]
fn bar_phases_varying_and_summary() {
    let fx = fixture("meter.json");
    let c = fx["cases"].as_array().unwrap().iter().find(|c| c["name"] == "seven_four").unwrap();
    let (s, beats) = case_inputs(c);
    let bl = bars::bar_phases_varying(&beats, &s);
    let ms: Vec<u32> = bl.iter().map(|b| b.meter.unwrap()).collect();
    let mut want = vec![7; 6];
    want.extend(vec![4; 10]);
    assert_eq!(ms, want);
    assert_eq!(bl[6].beat, 42);
    assert!((bl[6].time - 22.0).abs() < 1e-9);
    assert!(bl.iter().all(|b| (0.0..=1.0).contains(&b.conf)) && bl.iter().map(|b| b.conf).fold(1.0, f64::min) > 0.5);
    let sm = bars::meter_summary(&bl);
    assert_eq!((sm.meter, sm.switches), (4, 1));
    assert_eq!(sm.counts.into_iter().collect::<Vec<_>>(), vec![(4, 10), (7, 6)]);
}

/// tests/test_meter.py の test_gap_splits_segments
#[test]
fn gap_splits_segments() {
    let fx = fixture("meter.json");
    let c = fx["cases"].as_array().unwrap().iter().find(|c| c["name"] == "gap").unwrap();
    let (s, beats) = case_inputs(c);
    let bl = bars::bar_phases_varying(&beats, &s);
    assert_eq!(bl.iter().map(|b| b.beat).collect::<Vec<_>>(), (0..40).step_by(4).collect::<Vec<i64>>());
}
