const $ = (id) => document.getElementById(id);

chrome.storage.local.get(['host', 'token'], (s) => {
  $('host').value = s.host || 'http://127.0.0.1:7420';
  $('token').value = s.token || '';
});

// the whole phone link pasted into Host: take it apart rather than making the
// human do it. It is the one string TIDE hands out, and it has both halves.
const split = (raw) => {
  const text = (raw || '').trim();
  try {
    const u = new URL(text);
    const t = u.searchParams.get('t');
    if (t) return { host: u.origin, token: t };
    return { host: u.origin, token: null };
  } catch (e) {
    return { host: text.replace(/\/+$/, ''), token: null };
  }
};

$('host').addEventListener('blur', () => {
  const { host, token } = split($('host').value);
  $('host').value = host;
  if (token && !$('token').value.trim()) $('token').value = token;
});

$('save').onclick = () => {
  const { host, token } = split($('host').value);
  chrome.storage.local.set(
    { host, token: ($('token').value.trim() || token || '') },
    () => {
      $('token').value = $('token').value.trim() || token || '';
      $('ok').hidden = false;
      setTimeout(() => ($('ok').hidden = true), 1500);
    },
  );
};
