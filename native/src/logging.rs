//! ログ: <data>/djmix.log と標準エラー（画面側のエラーもここに記録される）。
use parking_lot::Mutex;
use std::io::Write;

struct Logger {
    file: Mutex<Option<std::fs::File>>,
    stderr: bool,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info || m.target().starts_with("djmix")
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        // 依存クレートの細かいログは出さない
        let t = r.target();
        if r.level() > log::Level::Warn && !(t.starts_with("djmix") || t.starts_with("djmix_window")) {
            return;
        }
        let line = format!("{} {} {}: {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), r.level(), t, r.args());
        if self.stderr {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        if let Some(f) = self.file.lock().as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().as_mut() {
            let _ = f.flush();
        }
    }
}

pub fn init(stderr: bool) {
    let p = crate::config::paths().data_dir.join("djmix.log");
    // 大きくなりすぎたら前の分を .1 に回す
    if std::fs::metadata(&p).map(|m| m.len() > 20 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&p, p.with_extension("log.1"));
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&p).ok();
    let _ = log::set_boxed_logger(Box::new(Logger { file: Mutex::new(file), stderr }));
    log::set_max_level(log::LevelFilter::Info);
}
