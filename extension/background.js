// The network lives here, not in the page. A content script's fetch is the
// page's fetch: subject to its CORS and, on an https site, to mixed-content
// blocking. The service worker has the host permissions and neither problem.

const settings = async () => {
  const s = await chrome.storage.local.get(['host', 'token', 'ws']);
  return { host: (s.host || 'http://127.0.0.1:7420').replace(/\/+$/, ''), token: s.token || '', ws: s.ws ?? null };
};

const api = (host, token, path) =>
  `${host}${path}${path.includes('?') ? '&' : '?'}t=${encodeURIComponent(token)}`;

async function list() {
  const { host, token, ws } = await settings();
  if (!token) return { error: 'no token' };
  const r = await fetch(api(host, token, '/thread?ws=0'), { cache: 'no-store' });
  if (!r.ok) return { error: r.status === 403 ? 'bad token' : `HTTP ${r.status}` };
  const d = await r.json();
  return { wss: d.wss || [], active: d.active ?? 0, ws };
}

async function say({ ws, text, imgs }) {
  const { host, token } = await settings();
  if (!token) return { error: 'no token' };
  const r = await fetch(api(host, token, `/say?ws=${ws}`), {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ text, imgs: imgs || [] }),
  });
  if (!r.ok) return { error: `HTTP ${r.status}` };
  await chrome.storage.local.set({ ws });
  return { ok: true };
}

async function openThread() {
  const { host, token } = await settings();
  chrome.tabs.create({ url: api(host, token, '/m') });
  return { ok: true };
}

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  const jobs = {
    list,
    say: () => say(msg),
    thread: openThread,
    options: async () => (chrome.runtime.openOptionsPage(), { ok: true }),
  };
  const job = jobs[msg.type];
  if (!job) return false;
  job(msg)
    .then(reply)
    .catch((e) => reply({ error: String(e.message || e) }));
  return true; // the reply comes later
});

// the toolbar icon and the shortcut both just open the panel in the page
const poke = async (tab) => {
  if (!tab || !tab.id) return;
  try {
    await chrome.tabs.sendMessage(tab.id, { type: 'toggle' });
  } catch (e) {
    // no content script here (chrome://, the Web Store): say why rather than
    // doing nothing at all
    chrome.action.setBadgeText({ tabId: tab.id, text: '—' });
    chrome.action.setTitle({ tabId: tab.id, title: "TIDE can't open on this page" });
  }
};

chrome.action.onClicked.addListener(poke);
chrome.commands.onCommand.addListener(async (cmd) => {
  if (cmd !== 'toggle-panel') return;
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  poke(tab);
});
