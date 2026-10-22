// The panel: a tab on the right edge of every page that opens into a composer.
// It lives in a shadow root so the page's CSS can't reach it and its own can't
// reach the page.
(() => {
  if (window.__tidePanel) return;
  window.__tidePanel = true;

  const HOST_ID = 'tide-panel-host';
  const host = document.createElement('div');
  host.id = HOST_ID;
  host.style.cssText = 'all:initial;position:fixed;top:0;right:0;z-index:2147483647';
  const root = host.attachShadow({ mode: 'open' });

  root.innerHTML = `
    <style>
      :host { all: initial }
      * { box-sizing: border-box; font-family: -apple-system, system-ui, "Segoe UI", sans-serif }
      #wrap { position: fixed; top: 0; right: 0; height: 100vh; display: flex;
              align-items: center; pointer-events: none }
      /* big enough to hit without aiming: a thumb-sized tab, not a seam */
      #tab { pointer-events: auto; display: flex; flex-direction: column; align-items: center;
             gap: 9px; background: #222426; color: #9aa0a8; border: 1px solid #2b2d30;
             border-right: 0; border-radius: 10px 0 0 10px; padding: 16px 9px;
             cursor: pointer; user-select: none; box-shadow: 0 3px 16px #0007;
             transition: color .12s, border-color .12s, padding .12s }
      #tab b { writing-mode: vertical-rl; text-orientation: mixed; font-size: 13px;
               font-weight: 600; letter-spacing: .22em }
      #tab svg { width: 20px; height: 20px; display: block }
      #tab:hover { color: #fff; border-color: #3574f0; padding-right: 13px }
      #tab.live { color: #6aaf6a }
      #panel { pointer-events: auto; width: 360px; max-height: 86vh; display: none;
               flex-direction: column; background: #1b1d1f; color: #d7d9dd;
               border: 1px solid #2b2d30; border-right: 0; border-radius: 8px 0 0 8px;
               box-shadow: 0 10px 40px #0009; overflow: hidden }
      #panel.open { display: flex }
      header { display: flex; align-items: center; gap: 8px; padding: 8px 9px;
               background: #222426; border-bottom: 1px solid #2b2d30 }
      header b { font-size: 11px; letter-spacing: .08em; color: #8b8f96 }
      select { flex: 1; min-width: 0; background: #1b1d1f; color: #d7d9dd; border: 1px solid #2b2d30;
               border-radius: 6px; padding: 4px 6px; font-size: 12px }
      select:focus { outline: none; border-color: #3574f0 }
      .x { cursor: pointer; color: #8b8f96; padding: 0 3px; font-size: 13px }
      /* parked and idle workspaces are most of the list and none of the work */
      #only { flex: 0 0 auto; cursor: pointer; user-select: none; font-size: 10px;
              letter-spacing: .06em; text-transform: uppercase; color: #8b8f96;
              border: 1px solid #2b2d30; border-radius: 999px; padding: 3px 8px }
      #only.on { background: #3574f0; border-color: #3574f0; color: #fff }
      .x:hover { color: #c75450 }
      main { padding: 9px; display: flex; flex-direction: column; gap: 8px; overflow-y: auto }
      textarea { width: 100%; min-height: 104px; resize: vertical; background: #121314;
                 color: #d7d9dd; border: 1px solid #2b2d30; border-radius: 6px; padding: 8px 9px;
                 font-size: 13px; line-height: 1.45 }
      textarea:focus { outline: none; border-color: #3574f0 }
      #shots { display: flex; flex-wrap: wrap; gap: 6px }
      .shot { position: relative; width: 74px; height: 50px; border: 1px solid #2b2d30;
              border-radius: 5px; overflow: hidden; background: #121314 }
      .shot img { width: 100%; height: 100%; object-fit: cover; display: block }
      .shot u { position: absolute; top: 1px; right: 1px; width: 15px; height: 15px; display: flex;
                align-items: center; justify-content: center; background: #000a; color: #fff;
                border-radius: 3px; font-size: 10px; cursor: pointer; text-decoration: none }
      footer { display: flex; align-items: center; gap: 8px; padding: 0 9px 9px }
      #note { flex: 1; min-width: 0; font-size: 11px; color: #8b8f96; overflow: hidden;
              text-overflow: ellipsis; white-space: nowrap }
      #note.bad { color: #c75450 }
      #note.good { color: #6aaf6a }
      #sel { font-size: 11px; color: #8b8f96; border-left: 2px solid #2b2d30; padding-left: 7px;
             overflow: hidden; text-overflow: ellipsis; white-space: nowrap }
      button { border: 0; border-radius: 6px; padding: 6px 12px; font-size: 12px; font-weight: 600;
               background: #3574f0; color: #fff; cursor: pointer }
      button.ghost { background: none; color: #8b8f96; border: 1px solid #2b2d30; font-weight: 400 }
      button:disabled { opacity: .5; cursor: default }
    </style>
    <div id="wrap">
      <div id="panel">
        <header>
          <b>TIDE</b>
          <select id="ws"></select>
          <span id="only" class="on" title="Which workspaces are listed">Active</span>
          <span class="x" id="close" title="Close">&#10005;</span>
        </header>
        <main>
          <textarea id="msg" placeholder="Tell TIDE what you're looking at…  ⌘V pastes a screenshot"></textarea>
          <div id="shots"></div>
          <div id="sel" hidden></div>
        </main>
        <footer>
          <div id="note">⌘↩ to send</div>
          <button class="ghost" id="thread">Thread</button>
          <button id="send">Send</button>
        </footer>
      </div>
      <div id="tab" title="Talk to TIDE  (⌥⇧T)">
        <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <path d="M3 9.5c2.2-2.6 4.1-2.6 6 0s3.8 2.6 6 0 4.1-2.6 6 0" stroke="#3574f0"
                stroke-width="2.6" stroke-linecap="round"/>
          <path d="M3 15.5c2.2-2.6 4.1-2.6 6 0s3.8 2.6 6 0 4.1-2.6 6 0" stroke="#6fa8ff"
                stroke-width="2.2" stroke-linecap="round" opacity=".75"/>
        </svg>
        <b>tide</b>
      </div>
    </div>`;

  (document.body || document.documentElement).append(host);

  const $ = (id) => root.getElementById(id);
  const panel = $('panel');
  const note = (text, kind) => {
    $('note').textContent = text;
    $('note').className = kind || '';
  };
  const ask = (msg) => chrome.runtime.sendMessage(msg);

  let shots = []; // [{mime, b64, url}]
  let loaded = false;
  let wss = [];   // every workspace TIDE has open, in its own order
  let onlyActive = true;
  // What was highlighted on the page. Taken as it happens, not at send time:
  // clicking into the panel is a click, and a click clears the page's
  // selection long before there is anything to send.
  let picked = '';
  document.addEventListener('selectionchange', () => {
    if (panel.classList.contains('open')) return; // our own typing isn't context
    const s = (window.getSelection() ? window.getSelection().toString() : '').trim();
    if (s) picked = s;
  });

  /// A pasted capture is a retina PNG: a few MB on the wire and again in the
  /// prompt. Scaled to 1800px wide it is a few hundred KB and reads the same.
  const shrink = (url) =>
    new Promise((done) => {
      const img = new Image();
      img.onload = () => {
        const scale = Math.min(1, 1800 / img.width);
        if (scale === 1 && url.length < 900000) return done(url);
        const c = document.createElement('canvas');
        c.width = Math.round(img.width * scale);
        c.height = Math.round(img.height * scale);
        c.getContext('2d').drawImage(img, 0, 0, c.width, c.height);
        try {
          done(c.toDataURL('image/jpeg', 0.9));
        } catch (e) {
          done(url);
        }
      };
      img.onerror = () => done(url);
      img.src = url;
    });

  const paintShots = () => {
    const box = $('shots');
    box.innerHTML = '';
    shots.forEach((s, i) => {
      const d = document.createElement('div');
      d.className = 'shot';
      const img = document.createElement('img');
      img.src = s.url;
      const x = document.createElement('u');
      x.textContent = '✕';
      x.title = 'Drop this one';
      x.onclick = () => {
        shots.splice(i, 1);
        paintShots();
      };
      d.append(img, x);
      box.append(d);
    });
  };

  async function attach(dataUrl) {
    const url = await shrink(dataUrl);
    shots.push({
      mime: url.startsWith('data:image/jpeg') ? 'image/jpeg' : 'image/png',
      b64: url.split(',')[1] || '',
      url,
    });
    paintShots();
    note(`${shots.length} image${shots.length === 1 ? '' : 's'} attached`);
  }

  $('msg').addEventListener('paste', (e) => {
    const items = [...(e.clipboardData ? e.clipboardData.items : [])];
    const pic = items.find((i) => i.type.startsWith('image/'));
    if (!pic) return; // ordinary text paste: leave it alone
    e.preventDefault();
    const file = pic.getAsFile();
    if (!file) return;
    const fr = new FileReader();
    fr.onload = () => attach(String(fr.result));
    fr.readAsDataURL(file);
  });

  /// The dropdown, filtered. The one you are on is always listed even when it
  /// is parked: a list that hides where you are is worse than a long one.
  function paintWorkspaces(want) {
    const sel = $('ws');
    const keep = want != null ? want : Number(sel.value || 0);
    sel.innerHTML = '';
    wss.forEach((w, i) => {
      if (onlyActive && w.off && i !== keep) return;
      const o = document.createElement('option');
      o.value = String(i);
      o.textContent = (w.n || `#${i}`) + (w.off ? '  · parked' : '') + (w.busy ? '  · working' : '');
      sel.append(o);
    });
    if (!sel.options.length) {
      const o = document.createElement('option');
      o.textContent = 'nothing active';
      o.disabled = true;
      sel.append(o);
      note('Nothing active — switch to All', 'bad');
    }
    sel.value = String(keep);
    if (!sel.value) sel.value = sel.options.length ? sel.options[0].value : '';
  }

  async function loadWorkspaces() {
    const sel = $('ws');
    sel.innerHTML = '<option>loading…</option>';
    const d = await ask({ type: 'list' });
    if (!d || d.error) {
      sel.innerHTML = '<option>offline</option>';
      note(d && d.error === 'no token' ? 'Set the host and token' : `Can't reach TIDE — ${d ? d.error : '?'}`, 'bad');
      if (d && d.error === 'no token') ask({ type: 'options' });
      return;
    }
    wss = d.wss || [];
    const want = d.ws != null && d.ws < wss.length ? d.ws : d.active || 0;
    paintWorkspaces(want);
    loaded = true;
    note('⌘↩ to send');
  }

  /// What you typed, then where you were. The page and anything you had
  /// highlighted go underneath as context, so the instruction leads.
  function compose() {
    const typed = $('msg').value.trim();
    const bits = [`On ${document.title || 'a page'} — ${location.href}`];
    if (picked) {
      const cut = picked.length > 2000 ? picked.slice(0, 2000) + '…' : picked;
      bits.push(`Selected on the page:\n> ${cut.replace(/\n/g, '\n> ')}`);
    }
    return `${typed}\n\n---\n${bits.join('\n\n')}`;
  }

  async function send() {
    if (!$('msg').value.trim()) return $('msg').focus();
    $('send').disabled = true;
    note('Sending…');
    const r = await ask({
      type: 'say',
      ws: Number($('ws').value || 0),
      text: compose(),
      imgs: shots.map((s) => ({ mime: s.mime, b64: s.b64 })),
    });
    $('send').disabled = false;
    if (!r || r.error) return note(`Didn't send — ${r ? r.error : '?'}`, 'bad');
    $('msg').value = '';
    shots = [];
    picked = '';
    paintShots();
    paintSel();
    note('Sent', 'good');
    setTimeout(() => open(false), 700);
  }

  // the selection is read when the panel opens, and clicking into the panel
  // clears it, so it is kept the moment it is still there
  /// What is going along with the message, said rather than asked: the page
  /// always, and whatever was highlighted when you opened the panel. Clicking
  /// it drops it.
  function paintSel() {
    const el = $('sel');
    el.hidden = !picked;
    if (!picked) return;
    const one = picked.replace(/\s+/g, ' ');
    el.textContent = `“${one.length > 70 ? one.slice(0, 70) + '…' : one}”`;
    el.title = 'Click to leave it out';
    el.style.cursor = 'pointer';
    el.onclick = () => {
      picked = '';
      paintSel();
    };
  }

  function open(yes) {
    panel.classList.toggle('open', yes);
    $('tab').style.display = yes ? 'none' : '';
    if (yes) {
      // the live selection wins over whatever was remembered
      const now = (window.getSelection() ? window.getSelection().toString() : '').trim();
      if (now) picked = now;
      paintSel();
      if (!loaded) loadWorkspaces();
      $('msg').focus();
    }
  }

  $('only').onclick = () => {
    onlyActive = !onlyActive;
    $('only').classList.toggle('on', onlyActive);
    $('only').textContent = onlyActive ? 'Active' : 'All';
    chrome.storage.local.set({ onlyActive });
    paintWorkspaces();
    if (loaded) note('⌘↩ to send');
  };
  chrome.storage.local.get(['onlyActive'], (s) => {
    onlyActive = s.onlyActive !== false;
    $('only').classList.toggle('on', onlyActive);
    $('only').textContent = onlyActive ? 'Active' : 'All';
  });

  $('tab').onclick = () => open(true);
  $('close').onclick = () => open(false);
  $('thread').onclick = () => ask({ type: 'thread' });
  $('send').onclick = send;
  $('msg').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      send();
    }
    if (e.key === 'Escape') open(false);
    e.stopPropagation(); // the page's own shortcuts are not ours to trigger
  });
  chrome.runtime.onMessage.addListener((m) => {
    if (m && m.type === 'toggle') open(!panel.classList.contains('open'));
  });
})();
