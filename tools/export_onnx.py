"""Beat This!（拍の検出）と Demucs（htdemucs。音源分離）を ONNX に書き出す（Mac 版 native/ の ML 推論用）。

Mac で一度だけ、リポジトリの直下で実行する（本物の重みをダウンロードするのでネットにつながっていること。数分）:

    uv run --no-project --python 3.11 --with torch==2.11.0 --with torchaudio==2.11.0 --with beat-this==1.1.0 \
        --with demucs==4.1.0 --with onnx==1.23.2 --with onnxscript --with onnxruntime --with soxr \
        python tools/export_onnx.py

（--no-project: リポジトリの pyproject.toml は Windows 版用で、torch を CUDA 版の置き場所から取るので使わない。
バージョンは Windows 版の uv.lock と同じ。Intel Mac は torch 2.11 が無いので --with torch==2.2.2 --with torchaudio==2.2.2
--with demucs==4.0.1 --with "numpy<2" にする（未確認）。）
重みのキャッシュ: Beat This! は ~/.cache/torch/hub/checkpoints/、Demucs は ~/.cache/huggingface/（4.1.0）。

書き出す先（--out で変更）: ~/Library/Application Support/DJ Mixer/models/
- beat_this_final0.onnx / .json   … Beat This!（final0）。入力 spect (1, T, 128) → beat, downbeat (1, T) の logits（50fps）
- htdemucs.onnx / .json           … HTDemucs の内側の網だけ。STFT/iSTFT と複素数の部分は Rust 側（native/src/analysis/stems.rs）
  入力 mag (1, 4, 2048, 336)（左右 × 実部/虚部）, mix (1, 2, 343980) → freq (1, 4, 4, 2048, 336), time (1, 4, 2, 343980)

重みは Windows 版と同じものを、同じ読み込み方で使う:
- Beat This!: beat_this.inference.Audio2Frames(checkpoint_path="final0") の model
- Demucs: demucs.pretrained.get_model("htdemucs")（重みの入った BagOfModels。中のモデルは 1 つ）

書き出した後、短い乱数の信号で ONNX Runtime と PyTorch の出力を比べ、最大の差を表示する。

--random-weights: 重みをダウンロードせず、同じ構成（final0 / htdemucs の既定）の乱数の重みで書き出す（テスト用。--out 必須）。
このとき PyTorch の重みも <out>/*.random.pt に保存する（tools/ml_reference.py が同じ重みで基準の出力を作る）。
"""
from __future__ import annotations

import argparse
import json
import math
import sys
import time
from fractions import Fraction
from pathlib import Path

import numpy as np
import torch

DEFAULT_OUT = Path.home() / "Library" / "Application Support" / "DJ Mixer" / "models"
OPSET = 18
FORMAT = 1      # .json の形の版（Rust 側と合わせる）

# htdemucs（955717e8）の構成。--random-weights のときだけ使う（本物は重みのファイルに入っている構成を使う）
HTDEMUCS_SOURCES = ["drums", "bass", "other", "vocals"]
HTDEMUCS_KWARGS = dict(
    audio_channels=2, samplerate=44100, segment=Fraction(39, 5), channels=48, channels_time=None, growth=2,
    nfft=4096, wiener_iters=0, end_iters=0, wiener_residual=False, cac=True, depth=4, rewrite=True,
    multi_freqs=[], multi_freqs_depth=3, freq_emb=0.2, emb_scale=10, emb_smooth=True, kernel_size=8,
    stride=4, time_stride=2, context=1, context_enc=0, norm_starts=4, norm_groups=4, dconv_mode=3,
    dconv_depth=2, dconv_comp=8, dconv_init=1e-3, bottom_channels=512, t_layers=5, t_hidden_scale=4.0,
    t_heads=8, t_dropout=0.02, t_layer_scale=True, t_gelu=True, t_emb="sin", t_max_positions=10000,
    t_max_period=10000.0, t_weight_pos_embed=1.0, t_cape_mean_normalize=True, t_cape_augment=True,
    t_cape_glob_loc_scale=[5000.0, 1.0, 1.4], t_sin_random_shift=0, t_norm_in=True, t_norm_in_group=False,
    t_group_norm=False, t_norm_first=True, t_norm_out=True, t_weight_decay=0.0, t_lr=None,
    t_sparse_self_attn=False, t_sparse_cross_attn=False, t_mask_type="diag", t_mask_random_seed=42,
    t_sparse_attn_window=400, t_global_window=100, t_sparsity=0.95, t_auto_sparsity=False,
    t_cross_first=False, rescale=0.1,
)


def _versions() -> dict:
    out = {"torch": torch.__version__}
    for mod in ["beat_this", "demucs", "onnx", "onnxruntime"]:
        try:
            from importlib.metadata import version
            out[mod] = version(mod.replace("_", "-"))
        except Exception:
            pass
    return out


def _randomize_norm_stats(model: torch.nn.Module, gen: torch.Generator) -> None:
    """乱数の重みのとき、BatchNorm の平均・分散やスケールも初期値（0/1）から動かす（比べる意味のある値にする）。"""
    with torch.no_grad():
        for m in model.modules():
            if isinstance(m, (torch.nn.BatchNorm1d, torch.nn.BatchNorm2d)):
                m.running_mean.copy_(torch.randn(m.running_mean.shape, generator=gen) * 0.1)
                m.running_var.copy_(torch.rand(m.running_var.shape, generator=gen) * 0.5 + 0.75)
                if m.affine:
                    m.weight.copy_(1 + 0.1 * torch.randn(m.weight.shape, generator=gen))
                    m.bias.copy_(0.1 * torch.randn(m.bias.shape, generator=gen))


def _export(module, args, path: Path, input_names, output_names, dynamic_axes=None) -> None:
    """ONNX に書き出す（TorchScript 版の書き出しを先に試し、だめなら dynamo 版）。1 ファイルにまとめる。

    nn.MultiheadAttention の高速版（_native_multi_head_attention）は TorchScript 版では書き出せないので切っておく
    （htdemucs の transformer。dynamo 版でも書き出せるが、ONNX Runtime で最初の数回の推論が遅いグラフになった）。
    dynamic_axes: {入力・出力の名前: {軸: 名前}}（dynamo 版では torch.export.Dim にする）"""
    import inspect

    import onnx
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp.onnx")
    err = None
    mha = getattr(torch.backends, "mha", None)
    if mha is not None and hasattr(mha, "set_fastpath_enabled"):
        mha.set_fastpath_enabled(False)
    has_dynamo = "dynamo" in inspect.signature(torch.onnx.export).parameters
    for dynamo in ((False, True) if has_dynamo else (False,)):
        try:
            kw = dict(input_names=input_names, output_names=output_names, opset_version=OPSET)
            if has_dynamo:
                kw["dynamo"] = dynamo
            if dynamo:
                if dynamic_axes:
                    dims = {}
                    kw["dynamic_shapes"] = tuple(
                        {ax: dims.setdefault(nm, torch.export.Dim(nm, min=8, max=1 << 20))
                         for ax, nm in dynamic_axes.get(name, {}).items()} or None
                        for name in input_names)
            else:
                kw["do_constant_folding"] = True
                if dynamic_axes:
                    kw["dynamic_axes"] = dynamic_axes
            torch.onnx.export(module, args, str(tmp), **kw)
            err = None
            break
        except Exception as e:  # noqa: BLE001
            err = e
            print(f"  export (dynamo={dynamo}) failed: {e!r}", file=sys.stderr)
    if err is not None:
        raise err
    m = onnx.load(str(tmp), load_external_data=True)
    onnx.save(m, str(path), save_as_external_data=False)
    for p in tmp.parent.glob(tmp.name + "*"):
        p.unlink()
    data = tmp.with_suffix(".onnx.data")
    if data.exists():
        data.unlink()
    onnx.checker.check_model(str(path))


def _ort_session(path: Path):
    import onnxruntime as ort
    so = ort.SessionOptions()
    return ort.InferenceSession(str(path), so, providers=["CPUExecutionProvider"])


# ---------------- Beat This! ----------------
class BeatThisWrapper(torch.nn.Module):
    def __init__(self, model):
        super().__init__()
        self.model = model

    def forward(self, spect):
        out = self.model(spect)
        return out["beat"], out["downbeat"]


def load_beat_this(random_weights: bool, seed: int = 0):
    from rotary_embedding_torch import RotaryEmbedding
    if random_weights:
        from beat_this.model.beat_tracker import BeatThis
        torch.manual_seed(seed)
        model = BeatThis()          # final0 は既定の構成（spect_dim 128, transformer_dim 512, 6 層, head_dim 32, stem_dim 32）
        _randomize_norm_stats(model, torch.Generator().manual_seed(seed + 1))
        model.eval()
        hparams = {}
    else:
        from beat_this.inference import Audio2Frames, load_checkpoint
        model = Audio2Frames(checkpoint_path="final0", device="cpu").model
        hparams = {k: v for k, v in load_checkpoint("final0", "cpu")["hyper_parameters"].items()
                   if isinstance(v, (int, float, str, bool, list, dict))}
    # 回転位置埋め込みのキャッシュは書き出しの時の長さで固定されてしまうので切る（値は同じ式で毎回計算される）
    for m in model.modules():
        if isinstance(m, RotaryEmbedding):
            m.cache_if_possible = False
    return model.eval(), hparams


def export_beat_this(out: Path, random_weights: bool) -> None:
    print("== Beat This! (final0)" + (" [random weights]" if random_weights else ""))
    model, hparams = load_beat_this(random_weights)
    if random_weights:
        torch.save(model.state_dict(), out / "beat_this_final0.random.pt")
    wrapper = BeatThisWrapper(model).eval()
    path = out / "beat_this_final0.onnx"
    x = torch.randn(1, 1500, 128)
    t0 = time.time()
    with torch.no_grad():
        _export(wrapper, (x,), path, ["spect"], ["beat", "downbeat"],
                dynamic_axes={"spect": {1: "time"}, "beat": {1: "time"}, "downbeat": {1: "time"}})
    print(f"  wrote {path} ({path.stat().st_size / 1e6:.1f} MB, {time.time() - t0:.1f}s)")
    meta = {
        "format": FORMAT, "model": "beat_this", "checkpoint": "random" if random_weights else "final0",
        "random_weights": random_weights, "inputs": {"spect": ["batch=1", "time", 128]},
        "outputs": {"beat": ["batch=1", "time"], "downbeat": ["batch=1", "time"]},
        # beat_this.preprocessing.LogMelSpect と inference.split_predict_aggregate の値（Rust 側はこれと同じ値で作ってある）
        "spect": {"sample_rate": 22050, "n_fft": 1024, "hop_length": 441, "f_min": 30, "f_max": 11000, "n_mels": 128,
                  "mel_scale": "slaney", "normalized": "frame_length", "power": 1, "log_multiplier": 1000},
        "fps": 50, "chunk_size": 1500, "border_size": 6, "overlap_mode": "keep_first",
        "hparams": hparams, "versions": _versions(), "opset": OPSET,
    }
    (out / "beat_this_final0.json").write_text(json.dumps(meta, indent=1, default=str), encoding="utf-8")
    # 自己確認: 長さの違う乱数のスペクトログラムで比べる（時間方向は可変）
    sess = _ort_session(path)
    for t in (1500, 437):
        x = torch.randn(1, t, 128) * 2 + 3
        with torch.no_grad():
            b, d = wrapper(x)
        ob, od = sess.run(None, {"spect": x.numpy()})
        err = max(np.abs(ob - b.numpy()).max(), np.abs(od - d.numpy()).max())
        print(f"  self-check T={t}: max abs err {err:.3g} (logit range {float(b.abs().max()):.3g})")
    # 音から（soxr → LogMelSpect → 分割して推論）でも比べる
    from beat_this.inference import split_predict_aggregate
    from beat_this.preprocessing import LogMelSpect
    import soxr
    sig = (np.random.default_rng(0).standard_normal(44100 * 4) * 0.1).astype(np.float32)
    spect = LogMelSpect()(torch.tensor(soxr.resample(sig, 44100, 22050), dtype=torch.float32))
    with torch.no_grad():
        ref = split_predict_aggregate(spect, 1500, 6, "keep_first", model)
    # 1500 フレーム未満は 1 つの塊（前後 6 フレームを 0 で埋めて推論）
    sp = torch.nn.functional.pad(spect, (0, 0, 6, 6))
    ob, od = sess.run(None, {"spect": sp[None].numpy()})
    err = max(np.abs(ob[0, 6:-6] - ref["beat"].numpy()).max(), np.abs(od[0, 6:-6] - ref["downbeat"].numpy()).max())
    print(f"  self-check (4 s noise, full pipeline): max abs err {err:.3g}")


# ---------------- Demucs (htdemucs) ----------------
class HTDemucsCore(torch.nn.Module):
    """HTDemucs.forward の、振幅（cac の実部/虚部）を作った後から、マスク（複素数）を作る前までだけ。

    forward 本体はそのまま使い（版の違いに強くするため）、複素数の部分（_spec, _magnitude, _mask, _ispec）だけを差し替える:
    - _spec(mix) → 入力の mag をそのまま返す（Rust で reflect pad + STFT + cac にしたもの）
    - _mask(z, m) → m（周波数側の出力。x * std + mean の後）を取っておく
    - _ispec(...) → 0（時間側の出力 xt * stdt + meant だけが forward の戻り値になる）
    """

    def __init__(self, model):
        super().__init__()
        self.m = model

    def forward(self, mag, mix):
        m = self.m
        store = {}
        S = len(m.sources)

        def spec(_x):
            return mag

        def magnitude(z):
            return z

        def mask(_z, x):
            store["freq"] = x
            return x

        def ispec(_z, length=None, scale=0):
            return torch.zeros(mix.shape[0], S, mix.shape[1], length, dtype=mix.dtype)

        m._spec, m._magnitude, m._mask, m._ispec = spec, magnitude, mask, ispec
        try:
            time_out = type(m).forward(m, mix)
        finally:
            del m._spec, m._magnitude, m._mask, m._ispec
        return store["freq"], time_out


def load_htdemucs(random_weights: bool, seed: int = 0):
    from demucs.apply import BagOfModels
    from demucs.htdemucs import HTDemucs
    if random_weights:
        torch.manual_seed(seed)
        model = HTDemucs(HTDEMUCS_SOURCES, **HTDEMUCS_KWARGS)
        # 層ごとのスケール（LayerScale。初期値 1e-3/1e-4）が小さすぎると枝の差が見えないので大きめにする
        with torch.no_grad():
            g = torch.Generator().manual_seed(seed + 1)
            for name, p in model.named_parameters():
                if name.endswith(".scale") and p.ndim == 1:
                    p.copy_(0.1 + 0.05 * torch.rand(p.shape, generator=g))
        model.eval()
        return model
    from demucs.pretrained import get_model
    bag = get_model("htdemucs")
    bag.eval()
    if isinstance(bag, BagOfModels):
        assert len(bag.models) == 1, "htdemucs は 1 モデルの想定"
        w = bag.weights[0]
        assert all(abs(x - 1.0) < 1e-9 for x in w), w
        model = bag.models[0]
    else:
        model = bag
    assert isinstance(model, HTDemucs), type(model)
    return model.eval()


def htdemucs_meta(model, random_weights: bool) -> dict:
    seg_len = int(model.segment * model.samplerate)
    hl = model.hop_length
    frames = int(math.ceil(seg_len / hl))
    return {
        "format": FORMAT, "model": "htdemucs", "random_weights": random_weights,
        "sources": list(model.sources), "samplerate": model.samplerate, "audio_channels": model.audio_channels,
        "segment": [model.segment.numerator, model.segment.denominator] if isinstance(model.segment, Fraction)
        else float(model.segment),
        "segment_length": seg_len, "nfft": model.nfft, "hop_length": hl, "cac": bool(model.cac),
        "freq_bins": model.nfft // 2, "frames": frames,
        "inputs": {"mag": [1, 2 * model.audio_channels, model.nfft // 2, frames], "mix": [1, model.audio_channels, seg_len]},
        "outputs": {"freq": [1, len(model.sources), 2 * model.audio_channels, model.nfft // 2, frames],
                    "time": [1, len(model.sources), model.audio_channels, seg_len]},
        "apply": {"shifts": 0, "split": True, "overlap": 0.25, "transition_power": 1.0},
        "versions": _versions(), "opset": OPSET,
    }


def export_htdemucs(out: Path, random_weights: bool) -> None:
    print("== Demucs (htdemucs)" + (" [random weights]" if random_weights else ""))
    model = load_htdemucs(random_weights)
    assert model.cac and model.wiener_iters == 0 and model.use_train_segment
    if random_weights:
        torch.save(model.state_dict(), out / "htdemucs.random.pt")
    meta = htdemucs_meta(model, random_weights)
    L, F, T = meta["segment_length"], meta["freq_bins"], meta["frames"]
    core = HTDemucsCore(model).eval()
    mix = torch.randn(1, 2, L) * 0.3
    with torch.no_grad():
        mag = model._magnitude(model._spec(mix))
    assert tuple(mag.shape) == (1, 4, F, T), mag.shape
    path = out / "htdemucs.onnx"
    t0 = time.time()
    with torch.no_grad():
        _export(core, (mag, mix), path, ["mag", "mix"], ["freq", "time"])
    print(f"  wrote {path} ({path.stat().st_size / 1e6:.1f} MB, {time.time() - t0:.1f}s)")
    (out / "htdemucs.json").write_text(json.dumps(meta, indent=1, default=str), encoding="utf-8")
    # 自己確認 1: 内側の網（ONNX と PyTorch）
    sess = _ort_session(path)
    mix = torch.randn(1, 2, L) * 0.2
    with torch.no_grad():
        mag = model._magnitude(model._spec(mix))
        f_ref, t_ref = core(mag, mix)
        full = model(mix)
    t1 = time.time()
    f_onnx, t_onnx = sess.run(None, {"mag": mag.numpy(), "mix": mix.numpy()})
    dt = time.time() - t1
    print(f"  self-check core: max abs err freq {np.abs(f_onnx - f_ref.numpy()).max():.3g} "
          f"(range {float(f_ref.abs().max()):.3g}), time {np.abs(t_onnx - t_ref.numpy()).max():.3g} "
          f"(range {float(t_ref.abs().max()):.3g}); onnx {dt:.2f}s / segment")
    # 自己確認 2: 差し替えた部分を元に戻して組み立てると model(mix) と同じになるか
    with torch.no_grad():
        S = len(model.sources)
        zout = model._mask(None, torch.from_numpy(f_onnx))
        x = model._ispec(zout, L)
        rebuilt = torch.from_numpy(t_onnx) + x
    print(f"  self-check full forward (onnx core + torch istft vs model(mix)): max abs err "
          f"{(rebuilt - full).abs().max().item():.3g} (range {float(full.abs().max()):.3g}, sources {S})")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, default=None, help=f"書き出す先（既定: {DEFAULT_OUT}）")
    ap.add_argument("--random-weights", action="store_true", help="乱数の重みで書き出す（テスト用。--out 必須）")
    ap.add_argument("--only", choices=["beat_this", "htdemucs"], default=None)
    a = ap.parse_args()
    if a.random_weights and a.out is None:
        ap.error("--random-weights では --out を指定すること（本物のモデルを上書きしないため）")
    out = a.out or DEFAULT_OUT
    out.mkdir(parents=True, exist_ok=True)
    torch.set_grad_enabled(False)
    if a.only in (None, "beat_this"):
        export_beat_this(out, a.random_weights)
    if a.only in (None, "htdemucs"):
        export_htdemucs(out, a.random_weights)
    print("done:", out)


if __name__ == "__main__":
    main()
