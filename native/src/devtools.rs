//! 開発用の検証（djmix/devtools.py。同期の E2E 試験）。
//!
//! 画面側で 2 つのチャンネルを左右に分けて録音し（左=A、右=B）、ここで左右それぞれの拍を Beat This! で検出して、
//! B の各拍と最寄りの A の拍のずれを集計する。両チャンネルに同じ検出器を使うので、検出器の癖（一定の遅れ）は打ち消し合う。
use crate::analysis::beat_this;
use crate::audio::decode::{ffmpeg_bin, SR};
use crate::util::{median, percentile, round};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub fn sync_check(data: &[u8], k: f64, skip_sec: f64, windows: Option<Vec<(f64, f64)>>) -> Result<Value> {
    let dir = tempfile_dir()?;
    let src = dir.join("rec.webm");
    std::fs::write(&src, data)?;
    let out = std::process::Command::new(ffmpeg_bin())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&src)
        .args(["-f", "f32le", "-ac", "2", "-ar", &SR.to_string(), "-"])
        .output()?;
    let _ = std::fs::remove_dir_all(&dir);
    if !out.status.success() {
        bail!("録音を読めません: {}", String::from_utf8_lossy(&out.stderr));
    }
    let st: Vec<f32> = out.stdout.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let n = st.len() / 2;
    let mut res = serde_json::Map::new();
    res.insert("seconds".into(), json!(round(n as f64 / SR as f64, 1)));
    let mut beats: Vec<Vec<f64>> = vec![];
    for (i, ch) in ["A", "B"].iter().enumerate() {
        let mono: Vec<f32> = (0..n).map(|j| st[j * 2 + i]).collect();
        let det = beat_this::detect(&mono, SR)?;
        let b: Vec<f64> = det.beats.into_iter().filter(|&t| t > skip_sec).collect();
        res.insert(format!("beats_{ch}"), json!(b.len()));
        beats.push(b);
    }
    let mut a = beats[0].clone();
    let mut b = beats[1].clone();
    if let Some(w) = &windows {
        b.retain(|&t| w.iter().any(|&(s0, s1)| t >= s0 && t < s1));
        res.insert("beats_B_in_windows".into(), json!(b.len()));
    }
    if a.len() < 8 || b.len() < 8 {
        res.insert("error".into(), json!("拍が少なすぎます"));
        return Ok(Value::Object(res));
    }
    // k=2 のとき B は A の倍の拍を刻むので、A の拍の間に中点を補ってから比べる
    let mids = |x: &[f64]| -> Vec<f64> {
        let mut v: Vec<f64> = x.to_vec();
        v.extend(x.windows(2).map(|w| (w[0] + w[1]) / 2.0));
        v.sort_by(|p, q| p.partial_cmp(q).unwrap());
        v
    };
    if k == 2.0 {
        a = mids(&a);
    } else if k == 0.5 {
        b = mids(&b);
    }
    let near = |t: f64| -> f64 {
        let i = crate::util::searchsorted_left(&a, t).clamp(1, a.len() - 1);
        if (t - a[i - 1]).abs() < (t - a[i]).abs() {
            a[i - 1]
        } else {
            a[i]
        }
    };
    let off: Vec<f64> = b.iter().map(|&t| (t - near(t)) * 1000.0).collect();
    // 検出器の倍テンポ誤り: 周りの拍間隔が曲の中央値の 0.7 倍未満の拍は、同期のずれではないので分けて数える
    let dense = |x: &[f64]| -> Vec<bool> {
        let ibi: Vec<f64> = x.windows(2).map(|w| w[1] - w[0]).collect();
        let med = median(&ibi);
        (0..x.len())
            .map(|i| {
                let l = if i == 0 { ibi[0] } else { ibi[i - 1] };
                let r = if i < ibi.len() { ibi[i] } else { ibi[ibi.len() - 1] };
                l.min(r) < 0.7 * med
            })
            .collect()
    };
    let db = dense(&b);
    let da = dense(&a);
    let octave: Vec<bool> = b
        .iter()
        .enumerate()
        .map(|(i, &t)| db[i] || da[crate::util::searchsorted_left(&a, t).min(a.len() - 1)])
        .collect();
    let valid: Vec<f64> = off.iter().zip(&octave).filter(|(_, o)| !**o).map(|(v, _)| *v).collect();
    let med = median(&valid);
    let dev: Vec<f64> = valid.iter().map(|v| (v - med).abs()).collect();
    let absv: Vec<f64> = valid.iter().map(|v| v.abs()).collect();
    let bad: Vec<f64> =
        b.iter().zip(&off).zip(&octave).filter(|((_, o), oc)| !**oc && o.abs() > 50.0).map(|((t, _), _)| round(*t, 1)).take(50).collect();
    res.insert("detector_octave_beats".into(), json!(octave.iter().filter(|x| **x).count()));
    res.insert("offset_median_ms".into(), json!(round(med, 1)));
    res.insert("jitter_p95_ms".into(), json!(round(percentile(&dev, 95.0), 1)));
    res.insert("abs_p95_ms".into(), json!(round(percentile(&absv, 95.0), 1)));
    res.insert("offbeat_ratio_50ms".into(), json!(round(absv.iter().filter(|v| **v > 50.0).count() as f64 / absv.len() as f64, 3)));
    res.insert("offbeat_times".into(), json!(bad));
    res.insert("raw_offbeat_ratio_50ms".into(), json!(round(off.iter().filter(|v| v.abs() > 50.0).count() as f64 / off.len() as f64, 3)));
    res.insert("offsets_ms".into(), json!(off.iter().map(|v| round(*v, 1)).collect::<Vec<_>>()));
    res.insert("b_times".into(), json!(b.iter().map(|v| round(*v, 2)).collect::<Vec<_>>()));
    Ok(Value::Object(res))
}

fn tempfile_dir() -> Result<std::path::PathBuf> {
    let d = std::env::temp_dir().join(format!("djmix-sync-{}-{}", std::process::id(), crate::util::now() as u64));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}
