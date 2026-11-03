//! MIDI の入出力（djmix/midi/bridge.py）。全入力ポートを監視し、WebSocket で画面に送る（Mac は CoreMIDI。midir を使う）。
//!
//! - 入力ポートの抜き差しは 2 秒ごとに確認して自動で開き直す
//! - 画面からの MIDI 出力（コントローラーの LED 点灯）も受け付ける
//! - 実機が無くても試せるよう、inject() で擬似的なメッセージを同じ経路に流せる
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast;

pub struct MidiBridge {
    pub tx: broadcast::Sender<Value>,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    available: bool,
    error: Option<String>,
    inputs: Vec<String>,
    outputs: Vec<String>,
    #[cfg(target_os = "macos")]
    in_conns: Vec<(String, midir::MidiInputConnection<()>)>,
    #[cfg(target_os = "macos")]
    out_conns: Vec<(String, midir::MidiOutputConnection)>,
}

impl Default for MidiBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiBridge {
    pub fn new() -> MidiBridge {
        let (tx, _) = broadcast::channel(1024);
        MidiBridge { tx, state: Arc::new(Mutex::new(State { available: true, ..Default::default() })) }
    }

    pub fn start(&self) {
        let st = self.state.clone();
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("midi".into())
            .spawn(move || {
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = &tx;
                    let mut s = st.lock();
                    s.available = false;
                    s.error = Some("この OS では MIDI を使えません".into());
                    log::warn!("MIDI unavailable on this OS");
                }
                #[cfg(target_os = "macos")]
                loop {
                    if let Err(e) = refresh(&st, &tx) {
                        log::error!("MIDI refresh failed: {e:#}");
                        st.lock().error = Some(e.to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            })
            .ok();
    }

    pub fn ports(&self) -> Value {
        let s = self.state.lock();
        let mut i = s.inputs.clone();
        i.sort();
        let mut o = s.outputs.clone();
        o.sort();
        json!({"available": s.available, "error": s.error, "inputs": i, "outputs": o})
    }

    /// MIDI メッセージを画面へ送る（実機の入力と、試験用の擬似入力の共通経路）。
    pub fn inject(&self, port: &str, data: &[i64]) {
        inject(&self.tx, port, data);
    }

    /// 画面 → 出力（LED）
    pub fn send(&self, port: &str, data: &[i64]) -> bool {
        #[cfg(target_os = "macos")]
        {
            let mut s = self.state.lock();
            let bytes: Vec<u8> = data.iter().map(|x| (*x & 0xff) as u8).collect();
            let first = |n: &str| n.split(' ').next().unwrap_or("").to_string();
            let idx = s
                .out_conns
                .iter()
                .position(|(n, _)| n == port)
                // 入力と出力でポート名が少し違う機種があるので、先頭の語でも探す
                .or_else(|| s.out_conns.iter().position(|(n, _)| first(n) == first(port)));
            if let Some(i) = idx {
                return s.out_conns[i].1.send(&bytes).is_ok();
            }
            false
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (port, data);
            false
        }
    }
}

fn inject(tx: &broadcast::Sender<Value>, port: &str, data: &[i64]) {
    let d: Vec<i64> = data.iter().take(3).map(|x| x & 0xff).collect();
    let _ = tx.send(json!({"type": "midi", "port": port, "data": d, "t": crate::util::now()}));
}

#[cfg(target_os = "macos")]
fn refresh(st: &Arc<Mutex<State>>, tx: &broadcast::Sender<Value>) -> anyhow::Result<()> {
    use midir::{Ignore, MidiInput, MidiOutput};
    let probe = MidiInput::new("djmix-probe")?;
    let ports = probe.ports();
    let names: Vec<(String, midir::MidiInputPort)> =
        ports.into_iter().filter_map(|p| probe.port_name(&p).ok().map(|n| (n, p))).collect();
    {
        let mut s = st.lock();
        let keep: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
        let before = s.in_conns.len();
        s.in_conns.retain(|(n, _)| keep.contains(n));
        if s.in_conns.len() != before {
            log::info!("MIDI in closed");
        }
    }
    for (name, port) in names {
        if st.lock().in_conns.iter().any(|(n, _)| *n == name) {
            continue;
        }
        let mut mi = MidiInput::new("djmix")?;
        mi.ignore(Ignore::All);
        let tx2 = tx.clone();
        let pname = name.clone();
        match mi.connect(
            &port,
            "djmix-in",
            move |_t, msg, _| {
                let d: Vec<i64> = msg.iter().map(|b| *b as i64).collect();
                inject(&tx2, &pname, &d);
            },
            (),
        ) {
            Ok(c) => {
                log::info!("MIDI in opened: {name}");
                st.lock().in_conns.push((name, c));
            }
            Err(e) => log::warn!("MIDI in open failed {name}: {e}"),
        }
    }
    let oprobe = MidiOutput::new("djmix-probe-out")?;
    let oports: Vec<(String, midir::MidiOutputPort)> =
        oprobe.ports().into_iter().filter_map(|p| oprobe.port_name(&p).ok().map(|n| (n, p))).collect();
    {
        let mut s = st.lock();
        let keep: Vec<String> = oports.iter().map(|(n, _)| n.clone()).collect();
        s.out_conns.retain(|(n, _)| keep.contains(n));
    }
    for (name, port) in oports {
        if st.lock().out_conns.iter().any(|(n, _)| *n == name) {
            continue;
        }
        let mo = MidiOutput::new("djmix")?;
        if let Ok(c) = mo.connect(&port, "djmix-out") {
            st.lock().out_conns.push((name, c));
        }
    }
    let mut s = st.lock();
    s.inputs = s.in_conns.iter().map(|(n, _)| n.clone()).collect();
    s.outputs = s.out_conns.iter().map(|(n, _)| n.clone()).collect();
    s.available = true;
    s.error = None;
    Ok(())
}
