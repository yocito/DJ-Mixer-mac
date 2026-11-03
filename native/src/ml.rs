//! ONNX Runtime まわりの共通の処理（拍の検出 beat_this.rs と音源分離 stems.rs が使う）。
//!
//! - ONNX Runtime の初期化（`ort-dynamic` のときは共有ライブラリを探して読む。読めなければ ML は使えないだけでアプリは動く）
//! - セッションの作成: `coreml` 付きでビルドしたときは CoreML の実行プロバイダを先に試し、だめなら CPU。
//!   実行中に CoreML で失敗したら、そのモデルは CPU で作り直してやり直す
//! - 重い推論は 1 本ずつ（djmix/analysis/gpu.py の `use()` と同じ役割）。ロックは 1 回の推論（塊・区間ごと）の間だけ持つので、
//!   長い音源分離の途中でも拍の検出が割り込める
//!
//! 環境変数:
//! - `DJMIX_ML_EP=cpu` で CoreML を使わない（`coreml` 付きのビルドでも）
//! - `DJMIX_ML_THREADS` で推論のスレッド数（既定は論理コア数。最大 16）
//! - `ORT_DYLIB_PATH`（`ort-dynamic` のとき）ONNX Runtime の共有ライブラリ
use anyhow::Result;
use parking_lot::{Mutex, MutexGuard};
use std::path::Path;

static HEAVY: Mutex<()> = parking_lot::const_mutex(());

/// 重い推論の区間（この間は他の推論が待つ）。
pub fn heavy() -> MutexGuard<'static, ()> {
    HEAVY.lock()
}

/// ML（ONNX Runtime）を入れてビルドしたか。
pub const fn compiled() -> bool {
    cfg!(feature = "ml")
}

/// 1 つの出力（形と値。行優先）
#[derive(Debug, Clone)]
pub struct Output {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

/// 1 つの入力（名前・形・値）
pub struct Input<'a> {
    pub name: &'a str,
    pub shape: Vec<usize>,
    pub data: &'a [f32],
}

#[cfg(feature = "ml")]
mod imp {
    use super::{Input, Output};
    use anyhow::{anyhow, bail, Context, Result};
    use once_cell::sync::OnceCell;
    use ort::session::{builder::GraphOptimizationLevel, Session, SessionInputValue};
    use ort::value::TensorRef;
    use parking_lot::Mutex;
    use std::borrow::Cow;
    use std::path::{Path, PathBuf};

    static INIT: OnceCell<std::result::Result<(), String>> = OnceCell::new();

    /// `ort-dynamic` のとき、ONNX Runtime の共有ライブラリを探す（ORT_DYLIB_PATH が無いとき）。
    #[cfg(feature = "ort-dynamic")]
    fn find_dylib() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("ORT_DYLIB_PATH").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(p));
        }
        let names: &[&str] = if cfg!(target_os = "macos") {
            &["libonnxruntime.dylib"]
        } else if cfg!(windows) {
            &["onnxruntime.dll"]
        } else {
            &["libonnxruntime.so"]
        };
        let mut dirs: Vec<PathBuf> = crate::config::paths().model_dirs.clone();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(d) = exe.parent() {
                dirs.push(d.to_path_buf());
                dirs.push(d.join("..").join("Frameworks"));
            }
        }
        dirs.push(PathBuf::from("/opt/homebrew/lib"));
        dirs.push(PathBuf::from("/usr/local/lib"));
        dirs.iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| p.exists())
    }

    fn init_inner() -> std::result::Result<(), String> {
        let r = std::panic::catch_unwind(|| -> std::result::Result<(), String> {
            #[cfg(feature = "ort-dynamic")]
            let builder = match find_dylib() {
                Some(p) => ort::init_from(p.to_string_lossy().to_string()),
                None => return Err("ONNX Runtime の共有ライブラリが見つかりません（ORT_DYLIB_PATH）".into()),
            };
            #[cfg(not(feature = "ort-dynamic"))]
            let builder = ort::init();
            builder.with_name("djmix").commit().map_err(|e| e.to_string())?;
            // ここで API を一度引いて、読めないライブラリなら失敗させる（ort は読めないと panic する）
            let _ = ort::api();
            Ok(())
        });
        match r {
            Ok(v) => v,
            Err(e) => Err(e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "ONNX Runtime を読み込めません".into())),
        }
    }

    pub fn init() -> Result<()> {
        INIT.get_or_init(|| {
            let r = init_inner();
            match &r {
                Ok(()) => log::info!("ONNX Runtime ready"),
                Err(e) => log::warn!("ONNX Runtime unavailable: {e}"),
            }
            r
        })
        .clone()
        .map_err(|e| anyhow!(e))
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Ep {
        Cpu,
        CoreMl,
    }

    fn want_coreml() -> bool {
        cfg!(feature = "coreml")
            && cfg!(target_vendor = "apple")
            && !std::env::var("DJMIX_ML_EP").map(|v| v.eq_ignore_ascii_case("cpu")).unwrap_or(false)
    }

    fn build(path: &Path, ep: Ep) -> Result<Session> {
        // スレッド数は明示する（指定しないと ONNX Runtime の既定が遅かった。4 コアの Linux で htdemucs の 1 区間 6 秒 → 2.5 秒）
        let threads = std::env::var("DJMIX_ML_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16));
        let b = Session::builder()?.with_optimization_level(GraphOptimizationLevel::Level3)?.with_intra_threads(threads)?;
        #[allow(unused_mut)]
        let mut b = b;
        if ep == Ep::CoreMl {
            #[cfg(feature = "coreml")]
            {
                use ort::execution_providers::coreml::{CoreMLComputeUnits, CoreMLExecutionProvider, CoreMLModelFormat};
                let cache = crate::config::paths().cache_dir.join("coreml");
                let _ = std::fs::create_dir_all(&cache);
                let ep = CoreMLExecutionProvider::default()
                    .with_model_format(CoreMLModelFormat::MLProgram)
                    .with_compute_units(CoreMLComputeUnits::All)
                    .with_model_cache_dir(cache.to_string_lossy())
                    .build()
                    .error_on_failure();
                b = b.with_execution_providers([ep])?;
            }
        }
        Ok(b.commit_from_file(path)?)
    }

    /// 読み込んだモデル（ONNX Runtime のセッション）。複数のスレッドから使える（実行は 1 本ずつ）。
    pub struct Model {
        path: PathBuf,
        inner: Mutex<(Session, Ep)>,
    }

    impl Model {
        pub fn load(path: &Path) -> Result<Model> {
            init()?;
            let t0 = std::time::Instant::now();
            let (sess, ep) = if want_coreml() {
                match build(path, Ep::CoreMl) {
                    Ok(s) => (s, Ep::CoreMl),
                    Err(e) => {
                        log::warn!("CoreML でモデルを読めません（CPU で動かします）: {}: {e}", path.display());
                        (build(path, Ep::Cpu)?, Ep::Cpu)
                    }
                }
            } else {
                (build(path, Ep::Cpu)?, Ep::Cpu)
            };
            log::info!("loaded {} ({:?}) in {:.1}s", path.display(), ep, t0.elapsed().as_secs_f64());
            Ok(Model { path: path.to_path_buf(), inner: Mutex::new((sess, ep)) })
        }

        pub fn ep(&self) -> Ep {
            self.inner.lock().1
        }

        fn run_once(sess: &mut Session, inputs: &[Input]) -> Result<Vec<Output>> {
            let mut vals: Vec<(Cow<str>, SessionInputValue)> = Vec::with_capacity(inputs.len());
            for i in inputs {
                let t = TensorRef::from_array_view((i.shape.clone(), i.data))
                    .with_context(|| format!("入力 {} {:?}", i.name, i.shape))?;
                vals.push((Cow::Borrowed(i.name), t.into()));
            }
            let out = sess.run(vals)?;
            let mut res = Vec::with_capacity(out.len());
            for (_, v) in out.iter() {
                let (shape, data) = v.try_extract_tensor::<f32>()?;
                res.push(Output { shape: shape.iter().map(|d| *d as usize).collect(), data: data.to_vec() });
            }
            Ok(res)
        }

        /// 推論する（出力はモデルの出力の順）。重い推論のロックを持って実行する。
        pub fn run(&self, inputs: &[Input]) -> Result<Vec<Output>> {
            let _g = super::heavy();
            let mut inner = self.inner.lock();
            match Self::run_once(&mut inner.0, inputs) {
                Ok(v) => Ok(v),
                Err(e) if inner.1 == Ep::CoreMl => {
                    log::warn!("CoreML での推論に失敗（CPU でやり直します）: {}: {e:#}", self.path.display());
                    *inner = (build(&self.path, Ep::Cpu)?, Ep::Cpu);
                    Self::run_once(&mut inner.0, inputs)
                }
                Err(e) => bail!("推論に失敗: {}: {e:#}", self.path.display()),
            }
        }
    }
}

#[cfg(not(feature = "ml"))]
mod imp {
    use super::{Input, Output};
    use anyhow::{bail, Result};
    use std::path::Path;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Ep {
        Cpu,
        CoreMl,
    }

    pub fn init() -> Result<()> {
        bail!("ML（ONNX Runtime）なしでビルドされています（--features ort-download / ort-dynamic）")
    }

    pub struct Model;

    impl Model {
        pub fn load(_path: &Path) -> Result<Model> {
            init()?;
            unreachable!()
        }
        pub fn ep(&self) -> Ep {
            Ep::Cpu
        }
        pub fn run(&self, _inputs: &[Input]) -> Result<Vec<Output>> {
            init()?;
            unreachable!()
        }
    }
}

pub use imp::{Ep, Model};

/// ONNX Runtime を使えるようにする（2 回目以降は最初の結果を返すだけ）。
pub fn init() -> Result<()> {
    imp::init()
}

/// ONNX Runtime が使えるか（ビルドに入っていて、読み込めた）。
pub fn ready() -> bool {
    compiled() && init().is_ok()
}

/// モデルのメタデータ（export_onnx.py が ONNX と並べて書く .json）。
pub fn read_meta(onnx: &Path) -> Result<serde_json::Value> {
    let p = onnx.with_extension("json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?)?;
    if v.get("random_weights").and_then(|x| x.as_bool()) == Some(true) {
        log::warn!("{} は乱数の重みのモデル（テスト用）です", onnx.display());
    }
    Ok(v)
}
