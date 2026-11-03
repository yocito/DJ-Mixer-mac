// 書き出しの小窓（design_phase3.md §6。モックアップ mockup/phase3 の共通部品）。
// 「高速で書き出す」（サーバーで計算）／「再生しながら録音する」（通し再生の音をそのまま）を選ぶ。
import { openModal, closeModal } from './modal.js';
import { putSettings } from './settings-view.js';

const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const STORE = 'djmix.export';
/** Finder で開く（ファイルならフォルダを開いて選んだ状態に）。 */
export async function reveal(path) {
  const r = await fetch('/api/reveal', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ path }) });
  if (!r.ok) throw new Error((await r.json().catch(() => ({}))).detail ?? 'フォルダを開けませんでした');
}
const fmtTime = s => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, '0')}`;

/** o: {queueItems(), mixMinutes(), onRecord({folder, name, format, tracklist}), onFast({folder, name, format, tracklist})} */
export async function openExport(o) {
  let pref = { mode: 'fast', format: 'wav', folder: '', tracklist: true };
  // 設定はアプリの設定 export_pref に残す（専用ウィンドウは localStorage が再起動で消えるため）。前の版の localStorage は引き継ぐ
  try {
    const saved = (await fetch('/api/settings').then(r => r.json())).export_pref;
    if (saved && Object.keys(saved).length) Object.assign(pref, saved);
    else Object.assign(pref, JSON.parse(localStorage.getItem(STORE) || '{}'));
  } catch { /* 無視 */ }
  if (!pref.folder) { try { pref.folder = (await fetch('/api/export/default-dir').then(r => r.json())).folder; } catch { /* 無視 */ } }
  const n = o.queueItems().length, mins = o.mixMinutes();
  if (!n) { openModal('<h3>ミックスを書き出す</h3><p class="lbl2">ミックスキューが空です。</p><div class="mfoot"><div class="btn" data-close>閉じる</div></div>'); return; }
  const d = new Date(), pad = x => String(x).padStart(2, '0');
  const name = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}${pad(d.getMinutes())} ミックス`;
  openModal(`<h3>ミックスを書き出す <span class="lbl2">${n}曲 ・ 約${Math.round(mins)}分</span></h3>
    <div class="exopts">
      <label class="exopt ${pref.mode === 'fast' ? 'on' : ''}"><input type="radio" name="ex" value="fast" ${pref.mode === 'fast' ? 'checked' : ''}><b>高速で書き出す</b>
        <span>再生せずに計算して書き出します（${Math.round(mins)} 分のミックスで数分）。繋ぎは自動の動き（つなぎ目エディタの線）どおり。再生中に手で動かした操作は入りません。</span></label>
      <label class="exopt ${pref.mode === 'record' ? 'on' : ''}"><input type="radio" name="ex" value="record" ${pref.mode === 'record' ? 'checked' : ''}><b>再生しながら録音する</b>
        <span>「ミックスを通して再生」の音をそのまま録音します。ミックスの長さだけ時間がかかります（約 ${Math.round(mins)} 分）。途中で手で動かしたフェーダーやつまみもそのまま入ります。「■ 止める」でそこまでを保存します。</span></label>
    </div>
    <div class="exrow"><span class="lbl2">形式</span><div class="seg2" data-fmt>
      ${[['wav', 'WAV（劣化なし）'], ['mp3_320', 'MP3 320kbps'], ['mp3_192', 'MP3 192kbps']].map(([v, l]) => `<div data-v="${v}" class="${v === pref.format ? 'on' : ''}">${l}</div>`).join('')}</div></div>
    <div class="exrow"><span class="lbl2">保存先</span><input data-folder value="${esc(pref.folder)}" style="flex:1"><div class="btn" data-open title="保存先のフォルダを Finder で開きます">📂 開く</div></div>
    <div class="exrow"><span class="lbl2">名前</span><input data-name value="${esc(name)}" style="flex:1"></div>
    <div class="exrow"><label><input type="checkbox" data-tl ${pref.tracklist ? 'checked' : ''}> 曲目リスト（開始時刻つき）も書き出す</label></div>
    <p class="lbl2" style="line-height:1.7">書き出したミックスは個人で楽しむ範囲でお使いください。</p>
    <div class="mfoot" data-foot><div class="btn" data-close>キャンセル</div><div class="btn primary" data-go>書き出す</div></div>`, m => {
    m.querySelectorAll('.exopt input').forEach(x => x.addEventListener('change', () => m.querySelectorAll('.exopt').forEach(e => e.classList.toggle('on', e.querySelector('input').checked))));
    const open = (path, onErr) => reveal(path).catch(e => onErr(e.message));
    m.querySelector('[data-open]').addEventListener('click', () => open(m.querySelector('[data-folder]').value.trim(), msg => alertIn(m, msg)));
    m.querySelectorAll('[data-fmt] div').forEach(x => x.addEventListener('click', () => m.querySelectorAll('[data-fmt] div').forEach(e => e.classList.toggle('on', e === x))));
    m.querySelector('[data-go]').addEventListener('click', async () => {
      const opt = {
        mode: m.querySelector('.exopt input:checked').value,
        format: m.querySelector('[data-fmt] .on').dataset.v,
        folder: m.querySelector('[data-folder]').value.trim(),
        name: m.querySelector('[data-name]').value.trim(),
        tracklist: m.querySelector('[data-tl]').checked,
      };
      pref = { mode: opt.mode, format: opt.format, folder: opt.folder, tracklist: opt.tracklist };
      putSettings({ export_pref: pref }).catch(() => {});
      if (opt.mode === 'record') { closeModal(); o.onRecord(opt); return; }
      const foot = m.querySelector('[data-foot]');
      foot.innerHTML = `<div class="meter" style="flex:1;width:auto"><i style="width:0%"></i></div><span class="lbl2" data-p>準備しています…</span><div class="btn" data-cancel>中止</div>`;
      let cancelled = false;
      foot.querySelector('[data-cancel]').addEventListener('click', () => { cancelled = true; o.onCancelFast?.(); });
      try {
        const res = await o.onFast(opt, p => {
          foot.querySelector('.meter i').style.width = `${(p.progress * 100).toFixed(1)}%`;
          foot.querySelector('[data-p]').textContent = p.message ?? `書き出し中 ${Math.round(p.progress * 100)}%${p.eta_sec != null ? `（残り 約${fmtTime(p.eta_sec)}）` : ''}`;
        });
        foot.innerHTML = `<span style="color:var(--ok);line-height:1.6">書き出しました: ${esc(res.path)}（${fmtTime(res.seconds)}）${res.tracklist ? '<br>曲目リスト: ' + esc(res.tracklist) : ''}</span><div class="sp"></div>` +
          `<div class="btn primary" data-reveal title="書き出したファイルを選んだ状態でフォルダを開きます">📂 フォルダを開く</div><div class="btn" data-close>閉じる</div>`;
        foot.querySelector('[data-reveal]').addEventListener('click', () => open(res.path, msg => alertIn(m, msg)));
      } catch (e) {
        foot.innerHTML = `<span style="color:var(--active)">${cancelled ? '中止しました' : esc(e.message)}</span><div class="sp"></div><div class="btn" data-close>閉じる</div>`;
      }
    });
  });
}

function alertIn(m, msg) {
  let el = m.querySelector('[data-err]');
  if (!el) { el = document.createElement('p'); el.dataset.err = ''; el.className = 'lbl2'; el.style.color = 'var(--active)'; m.querySelector('[data-foot]').before(el); }
  el.textContent = msg;
}
