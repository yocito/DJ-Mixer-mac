//! DSP の一致テストの共通部分: fixture（tests/fixtures/dsp/*.json。make_fixtures.py が作る）の読み込みと、合成音の作り直し。
#![allow(dead_code)]
use base64::Engine;
use serde_json::Value;
use std::path::PathBuf;

pub fn fixture(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsp").join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

fn bytes(v: &Value, key: &str) -> Vec<u8> {
    let s = v[key].as_str().unwrap_or_else(|| panic!("{key} がない: {v}"));
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

/// {"f64": base64}
pub fn f64s(v: &Value) -> Vec<f64> {
    bytes(v, "f64").chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()
}

/// {"f32": base64}
pub fn f32s(v: &Value) -> Vec<f32> {
    bytes(v, "f32").chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
}

/// {"u8": base64}
pub fn u8s(v: &Value) -> Vec<u8> {
    bytes(v, "u8")
}

/// 確率の列（{"const", "n"} か、k/255 の {"u8p"}）
pub fn probs(v: &Value) -> Vec<f32> {
    if let Some(c) = v.get("const") {
        return vec![c.as_f64().unwrap() as f32; v["n"].as_u64().unwrap() as usize];
    }
    bytes(v, "u8p").into_iter().map(|k| k as f32 / 255.0f32).collect()
}

pub fn bits(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().unwrap())
}

/// make_fixtures.py の synth と同じ演算順で作る合成音（モノラル float64）
pub fn synth(spec: &Value) -> Vec<f64> {
    let n = spec["n"].as_u64().unwrap() as usize;
    let mut buf = vec![0.0f64; n];
    for v in spec["voices"].as_array().unwrap() {
        let a = v.as_array().unwrap();
        let start = a[0].as_u64().unwrap() as usize;
        let len = a[1].as_u64().unwrap() as usize;
        let (amp, cw, sw, dec) = (bits(&a[2]), bits(&a[3]), bits(&a[4]), bits(&a[5]));
        let (mut c, mut s, mut env) = (1.0f64, 0.0f64, amp);
        for j in start..(start + len).min(n) {
            buf[j] += env * s;
            let (c2, s2) = (c * cw - s * sw, s * cw + c * sw);
            c = c2;
            s = s2;
            env *= dec;
        }
    }
    for v in spec["noise"].as_array().unwrap() {
        let a = v.as_array().unwrap();
        let start = a[0].as_u64().unwrap() as usize;
        let len = a[1].as_u64().unwrap() as usize;
        let amp = bits(&a[2]);
        let mut x = a[3].as_u64().unwrap();
        for j in start..(start + len).min(n) {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let r = x.wrapping_mul(0x2545F4914F6CDD1D);
            let u = (r >> 11) as f64 * (1.0 / 9007199254740992.0) * 2.0 - 1.0;
            buf[j] += amp * u;
        }
    }
    buf
}

/// ステレオ（L = 合成音、R = 合成音 × r_gain）をインターリーブで
pub fn stereo(spec: &Value) -> djmix::audio::decode::Stereo {
    let buf = synth(spec);
    let g = bits(&spec["r_gain"]);
    let mut data = Vec::with_capacity(buf.len() * 2);
    for &v in &buf {
        data.push(v as f32);
        data.push((v * g) as f32);
    }
    djmix::audio::decode::Stereo { data }
}

/// 相対誤差の最大（分母は参照の最大絶対値）
pub fn max_rel(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "長さが違う");
    let scale = b.iter().fold(0f64, |m, &v| m.max((v as f64).abs())).max(1e-30);
    a.iter().zip(b).fold(0f64, |m, (&x, &y)| m.max(((x as f64) - (y as f64)).abs())) / scale
}

pub fn max_abs(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len(), "長さが違う");
    a.iter().zip(b).fold(0f64, |m, (&x, &y)| m.max(((x as f64) - (y as f64)).abs()))
}
