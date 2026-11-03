// はじめての起動（2026-11-01。友人と共有するため）: 音楽フォルダがまだ無ければ、選んでもらう。
// 選ぶと設定に入り（applySettings → サーバーが走査を始める）、次に何をすればよいかを案内する。
import { openModal, closeModal } from './modal.js';

const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

/** apply(patch): 設定を変えて保存する（app.js の applySettings）。flash(msg): 画面の下の知らせ。 */
export async function checkFirstRun({ apply, flash }) {
  const st = await fetch('/api/stats').then(r => r.json()).catch(() => null);
  if (!st || (st.roots ?? []).some(r => r.active)) return;
  openModal(`<h3>はじめに: 音楽フォルダを選んでください</h3>
    <p class="lbl2" style="line-height:1.8">このアプリは、パソコンの中の音楽ファイル（MP3・AAC/M4A・FLAC・WAV など）を読み込んで使います。
      曲が入っているフォルダ（例: <code>/Users/（ユーザー名）/Music</code>、ミュージック.app なら <code>…/Music/Music/Media.localized/Music</code>）を選んでください。
      あとから設定タブの「音楽フォルダ」で追加・変更できます。</p>
    <div class="exrow"><input data-fr placeholder="フォルダのパス" style="flex:1"><div class="btn" data-frpick>フォルダを選ぶ…</div></div>
    <div class="lbl2" data-frmsg style="color:var(--active)"></div>
    <div class="mfoot"><div class="btn" data-close>あとで</div><div class="btn primary" data-frok>このフォルダを使う</div></div>`, m => {
    const input = m.querySelector('[data-fr]'), msg = m.querySelector('[data-frmsg]');
    m.querySelector('[data-frpick]').addEventListener('click', async () => {
      const r = await fetch('/api/dialog/folder', { method: 'POST' });
      const j = await r.json().catch(() => ({}));
      if (!r.ok) { msg.textContent = j.detail ?? 'フォルダ選択を開けませんでした。パスを入力してください'; input.focus(); return; }
      if (j.path) input.value = j.path;
    });
    m.querySelector('[data-frok]').addEventListener('click', async () => {
      const p = input.value.trim();
      if (!p) { msg.textContent = 'フォルダを選んでください'; return; }
      try {
        await apply({ music_roots: [p] });
      } catch (e) { msg.textContent = e.message; return; }
      closeModal();
      openModal(`<h3>曲を読み込んでいます</h3>
        <p class="lbl2" style="line-height:1.9">「${esc(p)}」の曲を読み込んでいます（1 万曲で数分）。読み込んだ曲はライブラリタブに出ます。<br>
          次に、<b>おまかせタブの「ライブラリの解析」→「まとめて解析」</b>で曲を解析してください（拍・テンポ・キーなど。1 万曲で数時間、NVIDIA の GPU が無いともっとかかります）。
          おまかせ・相性の提案は、解析が済んだ曲だけを使います。デッキで鳴らす曲は、読み込んだときにその場で解析します。<br>
          曲のファイルと、作ったミックスは、個人で楽しむ範囲でお使いください（ファイルを人に渡したり公開したりしないでください）。</p>
        <div class="mfoot"><div class="btn primary" data-close>はじめる</div></div>`);
      flash?.('音楽フォルダを設定しました。曲を読み込んでいます', 4000);
    });
  });
}
