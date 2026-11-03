// 起動時の実行環境チェック（design_phase1.md §9「WebView で setSinkId や AudioWorklet の挙動がブラウザと違う」への対策。Mac 版は WKWebView）。
// 結果はバックエンドのログ(data/djmix.log)に送り、ヘッダーにも短く表示する。
//
// Signalsmith Stretch のノードは、音声スレッドが動き出してから準備完了を返す。自動再生制限で AudioContext が
// suspended のままだと完了しないので、resume できなければ最初のクリックまで待ってから確認する。
// （専用ウィンドウでは起動オプションで自動再生制限を外しているので、通常は起動直後に完了する）
import SignalsmithStretch from './vendor/signalsmith-stretch/SignalsmithStretch.mjs';

const withTimeout = (p, ms) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error('timeout')), ms))]);
const firstGesture = () => new Promise(res => addEventListener('pointerdown', res, { once: true, capture: true }));

function show(text, ng) {
  const el = document.getElementById('diag');
  el.textContent = text;
  el.classList.toggle('ng', !!ng);
}

async function report(checks) {
  try {
    await fetch('/api/diagnostics', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ userAgent: navigator.userAgent, checks }),
    });
  } catch { /* サーバーに届かなくても画面は動かす */ }
}

export async function runDiagnostics() {
  const checks = {};
  checks.AudioContext = typeof AudioContext === 'function';
  checks.AudioWorklet = checks.AudioContext && 'audioWorklet' in AudioContext.prototype;
  checks.setSinkId = String(checks.AudioContext && 'setSinkId' in AudioContext.prototype);   // Mac（WKWebView）には無い。出力は Mac の設定で選ぶので NG にしない
  checks.WebAssembly = typeof WebAssembly === 'object';
  checks.WebSocket = typeof WebSocket === 'function';
  const ctx = new AudioContext({ latencyHint: 'interactive', sampleRate: 44100 });
  try {
    await withTimeout(ctx.resume(), 1000).catch(() => {});
    if (ctx.state !== 'running') {
      show('音声エンジン: クリックで開始');
      await firstGesture();
      await ctx.resume();
    }
    checks.gestureRequired = ctx.state === 'running' ? 'no' : 'yes';
    checks.sampleRate = String(ctx.sampleRate);
    const node = await withTimeout(SignalsmithStretch(ctx), 5000);
    checks.SignalsmithStretch = typeof node.schedule === 'function' && typeof node.addBuffers === 'function';
    checks.baseLatencyMs = String(Math.round((ctx.baseLatency || 0) * 1000));
    checks.outputLatencyMs = String(Math.round((ctx.outputLatency || 0) * 1000));
  } catch (e) {
    checks.SignalsmithStretch = false;
    checks.error = String(e?.message ?? e);
  } finally {
    ctx.close();
  }
  const ok = Object.values(checks).every(v => v !== false);
  show(ok ? '音声エンジン OK' : '音声エンジン NG（ログ参照）', !ok);
  await report(checks);
  return { ok, checks };
}
