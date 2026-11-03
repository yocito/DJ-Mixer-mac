//! パス・設定値（djmix/config.py）。値は環境変数で上書きできる（テストや別環境用）。
//!
//! Mac での置き場所:
//! - データ（ライブラリの DB・解析キャッシュ・ログ・割り当て）: `~/Library/Application Support/DJ Mixer/`（`DJMIX_DATA` で変更）
//! - 画面・既定の割り当て・モデル（読むだけ）: アプリの `Contents/Resources/`、開発中はリポジトリの直下（`DJMIX_RESOURCES` で変更）
use once_cell::sync::OnceCell;
use std::path::{Path, PathBuf};

pub const APP_TITLE: &str = "DJ Mixer";
pub const HOST: &str = "127.0.0.1";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub db_path: PathBuf,
    pub mappings_dir: PathBuf,
    pub resources_dir: PathBuf,
    pub frontend_dir: PathBuf,
    /// ONNX モデルを探す場所（先にあるものを使う）
    pub model_dirs: Vec<PathBuf>,
    /// 既定の音楽フォルダ（`DJMIX_MUSIC_ROOT`）。無ければ未設定
    pub music_root: Option<PathBuf>,
}

static PATHS: OnceCell<Paths> = OnceCell::new();

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DJMIX_DATA") {
        return PathBuf::from(d);
    }
    if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support").join(APP_TITLE)
    } else {
        home().join(".local").join("share").join("djmix")
    }
}

/// 画面ファイル（frontend/index.html）のある場所を探す。
/// アプリ: 実行ファイル（Contents/MacOS/djmix）の ../Resources。開発: 実行ファイルから親をたどってリポジトリの直下。
pub fn default_resources_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DJMIX_RESOURCES") {
        return PathBuf::from(d);
    }
    let mut cands: Vec<PathBuf> = vec![];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("..").join("Resources"));
            let mut p: Option<&Path> = Some(dir);
            while let Some(d) = p {
                cands.push(d.to_path_buf());
                p = d.parent();
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.clone());
        if let Some(p) = cwd.parent() {
            cands.push(p.to_path_buf());
        }
    }
    // コンパイル時のリポジトリ（cargo run / cargo test）
    cands.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));
    for c in cands {
        if c.join("frontend").join("index.html").exists() {
            return c.canonicalize().unwrap_or(c);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

impl Paths {
    pub fn new(data_dir: PathBuf, resources_dir: PathBuf) -> Paths {
        let mut model_dirs = vec![data_dir.join("models"), resources_dir.join("models")];
        if let Some(d) = std::env::var_os("DJMIX_MODELS") {
            model_dirs.insert(0, PathBuf::from(d));
        }
        Paths {
            cache_dir: data_dir.join("cache"),
            db_path: data_dir.join("library.sqlite"),
            mappings_dir: data_dir.join("mappings"),
            frontend_dir: resources_dir.join("frontend"),
            model_dirs,
            music_root: std::env::var_os("DJMIX_MUSIC_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from),
            data_dir,
            resources_dir,
        }
    }
}

/// 起動時に一度だけ決める（呼ばなければ既定の場所）。
pub fn init(p: Paths) -> &'static Paths {
    let _ = PATHS.set(p);
    paths()
}

pub fn paths() -> &'static Paths {
    PATHS.get_or_init(|| Paths::new(default_data_dir(), default_resources_dir()))
}

/// モデルのファイル（例 "beat_this_final0.onnx"）を探す。
pub fn model_file(name: &str) -> Option<PathBuf> {
    paths().model_dirs.iter().map(|d| d.join(name)).find(|p| p.exists())
}

/// 既定の割り当て（リソースの config/mappings）を、データの mappings に無ければ写す。
pub fn seed_mappings() {
    let p = paths();
    let _ = std::fs::create_dir_all(&p.mappings_dir);
    let src = p.resources_dir.join("config").join("mappings");
    if let Ok(rd) = std::fs::read_dir(&src) {
        for e in rd.flatten() {
            let dst = p.mappings_dir.join(e.file_name());
            if !dst.exists() {
                let _ = std::fs::copy(e.path(), dst);
            }
        }
    }
}
