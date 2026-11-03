"""Rust 版の ML 推論（native/tests/ml_*.rs）を確かめるための基準の出力を PyTorch で作る。

使い方（本物の重みは使わない。乱数の重みのモデルを書き出してから）:

    python tools/export_onnx.py --random-weights --out <DIR>
    python tools/ml_reference.py --models <DIR>
    cd native && DJMIX_ML_TEST_DIR=<DIR> ORT_DYLIB_PATH=<libonnxruntime.so> \
        cargo test --release --features ort-dynamic --test ml_beat_this --test ml_stems -- --nocapture

<DIR> に書くもの（すべて little endian の生の配列。形は ml_reference.json）:
- signal_<name>.f32        合成した音（ステレオ・インターリーブ・44.1kHz）
- bt_<name>_mono22.f32     soxr で 22050Hz にしたモノラル（(L+R)/2）
- bt_<name>_spect.f32      LogMelSpect（T × 128）
- bt_<name>_beat.f32 / bt_<name>_down.f32   split_predict_aggregate の logits（T）
- bt_<name>_beats.f64 / bt_<name>_downs.f64 Postprocessor("minimal") の拍・小節頭（秒）
- dm_<name>_sep.f32        djmix.analysis.stems.separate と同じ処理の出力（8 × n、SOURCES の順に L, R）
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np
import torch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import export_onnx  # noqa: E402

SR = 44100
SOURCES = ["vocals", "drums", "bass", "other"]


def synth(seconds: float, seed: int, bpm: float = 124.0) -> np.ndarray:
    """拍のはっきりした合成音 (n, 2) float32: キック・ハイハット・ベース・和音・ボーカル風の音。"""
    rng = np.random.default_rng(seed)
    n = int(seconds * SR)
    t = np.arange(n) / SR
    out = np.zeros((n, 2))
    beat = 60.0 / bpm
    nb = int(seconds / beat) + 1
    for i in range(nb):
        s = int(i * beat * SR)
        if s >= n:
            break
        k = np.arange(min(int(0.25 * SR), n - s))
        tt = k / SR
        amp = 1.0 if i % 4 == 0 else 0.7
        kick = amp * np.sin(2 * np.pi * (50 + 120 * np.exp(-tt * 30)) * tt) * np.exp(-tt * 12)
        out[s:s + len(k)] += kick[:, None] * 0.8
        # ハイハット（8 分）
        for off in (0.0, 0.5):
            h0 = s + int(off * beat * SR)
            hl = min(int(0.05 * SR), n - h0)
            if hl > 0:
                hh = rng.standard_normal(hl) * np.exp(-np.arange(hl) / SR * 80) * 0.15
                out[h0:h0 + hl, 0] += hh
                out[h0:h0 + hl, 1] += hh * 0.7
    # ベース（小節ごとに音が変わる）
    roots = [55.0, 55.0 * 2 ** (3 / 12), 55.0 * 2 ** (5 / 12), 55.0 * 2 ** (-2 / 12)]
    bar = 4 * beat
    f_bass = np.array([roots[int(x / bar) % 4] for x in t])
    out += (0.25 * np.sin(2 * np.pi * np.cumsum(f_bass) / SR))[:, None]
    # 和音
    for semi in (0, 4, 7):
        f = 220.0 * 2 ** (semi / 12)
        out[:, 0] += 0.05 * np.sin(2 * np.pi * f * t + semi)
        out[:, 1] += 0.05 * np.sin(2 * np.pi * f * 1.003 * t)
    # ボーカル風（後半だけ。ビブラート付きの倍音）
    vib = 330 * (1 + 0.01 * np.sin(2 * np.pi * 5 * t))
    ph = 2 * np.pi * np.cumsum(vib) / SR
    voc = sum(np.sin(h * ph) / h for h in range(1, 8)) * 0.12 * (t > seconds / 2)
    out += voc[:, None]
    out += rng.standard_normal((n, 2)) * 0.003
    return (out / np.abs(out).max() * 0.8).astype(np.float32)


def beat_reference(model, stereo: np.ndarray, name: str, out: Path, meta: dict) -> None:
    import soxr
    from beat_this.inference import split_predict_aggregate
    from beat_this.model.postprocessor import Postprocessor
    from beat_this.preprocessing import LogMelSpect
    mono = ((stereo[:, 0] + stereo[:, 1]) * np.float32(0.5)).astype(np.float32)   # Stereo::mono と同じ (L+R)/2
    t0 = time.time()
    # beat_this.inference.Audio2Frames と同じ
    y = soxr.resample(mono, in_rate=SR, out_rate=22050)
    spect = LogMelSpect()(torch.tensor(y, dtype=torch.float32))
    t1 = time.time()
    with torch.inference_mode():
        pred = split_predict_aggregate(spect=spect, chunk_size=1500, overlap_mode="keep_first", border_size=6, model=model)
    bl, dl = pred["beat"].float(), pred["downbeat"].float()
    t2 = time.time()
    beats, downs = Postprocessor(type="minimal")(bl, dl)
    t3 = time.time()
    y.astype("<f4").tofile(out / f"bt_{name}_mono22.f32")
    spect.numpy().astype("<f4").tofile(out / f"bt_{name}_spect.f32")
    bl.numpy().astype("<f4").tofile(out / f"bt_{name}_beat.f32")
    dl.numpy().astype("<f4").tofile(out / f"bt_{name}_down.f32")
    np.asarray(beats, dtype="<f8").tofile(out / f"bt_{name}_beats.f64")
    np.asarray(downs, dtype="<f8").tofile(out / f"bt_{name}_downs.f64")
    meta[f"bt_{name}"] = {"frames": int(spect.shape[0]), "samples22": int(len(y)), "beats": len(beats), "downbeats": len(downs),
                          "time_spect_s": t1 - t0, "time_model_s": t2 - t1, "time_post_s": t3 - t2,
                          "logit_range": [float(bl.min()), float(bl.max())]}
    print(f"  beat_this {name}: {spect.shape[0]} frames, {len(beats)} beats, {len(downs)} downbeats, "
          f"spect {t1 - t0:.2f}s model {t2 - t1:.2f}s post {t3 - t2:.3f}s")


def demucs_reference(model, stereo: np.ndarray, name: str, out: Path, meta: dict) -> None:
    # djmix/analysis/stems.py の separate と同じ（モデルだけ乱数の重み）
    from demucs.apply import apply_model
    wav = torch.from_numpy(np.ascontiguousarray(stereo.T, dtype=np.float32))
    ref = wav.mean(0)
    m, s = ref.mean(), ref.std() + 1e-8
    t0 = time.time()
    with torch.no_grad():
        res = apply_model(model, ((wav - m) / s)[None], device="cpu", shifts=0, split=True, overlap=0.25, progress=False)[0]
        res = (res * s + m).numpy()
    dt = time.time() - t0
    order = [list(model.sources).index(k) for k in SOURCES]
    sep = res[order].astype(np.float32).reshape(8, -1)
    sep.astype("<f4").tofile(out / f"dm_{name}_sep.f32")
    meta[f"dm_{name}"] = {"frames": int(sep.shape[1]), "time_s": dt, "range": float(np.abs(sep).max())}
    print(f"  demucs {name}: {sep.shape[1]} frames, {dt:.1f}s (torch CPU, {torch.get_num_threads()} threads)")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", type=Path, required=True, help="export_onnx.py --random-weights の --out")
    ap.add_argument("--long", type=float, default=36.0, help="長い信号（Beat This! が 2 つの塊、Demucs が 7 区間）")
    ap.add_argument("--short", type=float, default=12.0, help="短い信号（Beat This! が 1 つの塊）")
    ap.add_argument("--skip-demucs", action="store_true")
    a = ap.parse_args()
    out = a.models
    torch.set_grad_enabled(False)
    meta: dict = {"sr": SR, "torch_threads": torch.get_num_threads(), "signals": {}}
    sigs = {"long": synth(a.long, 1), "short": synth(a.short, 2, bpm=97.0)}
    for k, v in sigs.items():
        v.astype("<f4").tofile(out / f"signal_{k}.f32")
        meta["signals"][k] = {"frames": int(v.shape[0])}

    from beat_this.model.beat_tracker import BeatThis
    bt = BeatThis()
    bt.load_state_dict(torch.load(out / "beat_this_final0.random.pt", weights_only=True))
    bt.eval()
    with torch.inference_mode():
        bt(torch.zeros(1, 1500, 128))       # 1 回目は遅いので、時間を測る前に一度動かす
    for k, v in sigs.items():
        beat_reference(bt, v, k, out, meta)

    if not a.skip_demucs:
        from demucs.htdemucs import HTDemucs
        dm = HTDemucs(export_onnx.HTDEMUCS_SOURCES, **export_onnx.HTDEMUCS_KWARGS)
        dm.load_state_dict(torch.load(out / "htdemucs.random.pt", weights_only=True))
        dm.eval()
        demucs_reference(dm, sigs["long"], "long", out, meta)

    (out / "ml_reference.json").write_text(json.dumps(meta, indent=1), encoding="utf-8")
    print("wrote", out / "ml_reference.json")


if __name__ == "__main__":
    main()
