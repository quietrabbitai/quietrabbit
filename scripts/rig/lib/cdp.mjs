// Shared Chrome DevTools Protocol helpers for the rig (Node >= 22: built-in WebSocket and fetch).
// Scratch runs only. Everything here reports names and counts, never stored values.

export const PREFIX = 'qrrig_canary';

export function cdpPort(flags) {
  return String(flags.port ?? process.env.RIG_CDP_PORT ?? '9222');
}

export async function listTargets(port) {
  const base = `http://127.0.0.1:${port}`;
  const [version, list] = await Promise.all([
    fetch(`${base}/json/version`).then((r) => r.json()),
    fetch(`${base}/json/list`).then((r) => r.json()),
  ]);
  return { version, list };
}

export function originOf(url) {
  try {
    return new URL(url).origin;
  } catch {
    return '(unparsed)';
  }
}

export async function connect(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((res, rej) => {
    ws.addEventListener('open', res, { once: true });
    ws.addEventListener('error', () => rej(new Error('websocket error')), { once: true });
  });
  let n = 0;
  const pending = new Map();
  ws.addEventListener('message', (e) => {
    const m = JSON.parse(e.data);
    const p = pending.get(m.id);
    if (p) {
      pending.delete(m.id);
      p(m);
    }
  });
  return {
    call: (method, params = {}, ms = 10000) =>
      new Promise((res, rej) => {
        const id = ++n;
        const t = setTimeout(() => {
          pending.delete(id);
          rej(new Error(`timeout ${method}`));
        }, ms);
        pending.set(id, (m) => {
          clearTimeout(t);
          res(m);
        });
        ws.send(JSON.stringify({ id, method, params }));
      }),
    close: () => ws.close(),
  };
}

// Evaluate an async expression in a page target; returns the value, or {error} on an exception.
export async function evalIn(pg, expression) {
  const r = await pg.call('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
  const ex = r.result?.exceptionDetails;
  if (ex) return { error: (ex.exception?.description ?? ex.text ?? 'exception').split('\n')[0] };
  return r.result?.result?.value;
}

// Writes synthetic canary data of every browser storage kind into the page's origin.
const SET_JS = `(async () => {
  localStorage.setItem('${PREFIX}_ls', '1');
  sessionStorage.setItem('${PREFIX}_ss', '1');
  await new Promise((res, rej) => {
    const q = indexedDB.open('${PREFIX}_idb', 1);
    q.onupgradeneeded = () => q.result.createObjectStore('s');
    q.onsuccess = () => { q.result.close(); res(); };
    q.onerror = () => rej(q.error);
  });
  const c = await caches.open('${PREFIX}_cache');
  await c.put('/${PREFIX}', new Response('x'));
  return 1;
})()`;

// Counts per store: canary-named entries and total entries. null = store unavailable on this origin.
const COUNT_JS = `(async () => {
  const P = '${PREFIX}';
  const t = async (f) => { try { return await f(); } catch { return null; } };
  const keys = (s) => Object.keys(s);
  return {
    origin: location.origin,
    ls: await t(() => ({ canary: keys(localStorage).filter((k) => k.startsWith(P)).length, total: localStorage.length })),
    ss: await t(() => ({ canary: keys(sessionStorage).filter((k) => k.startsWith(P)).length, total: sessionStorage.length })),
    idb: await t(async () => { const d = (await indexedDB.databases()).map((x) => x.name || ''); return { canary: d.filter((n) => n.startsWith(P)).length, total: d.length }; }),
    cache: await t(async () => { const k = await caches.keys(); return { canary: k.filter((n) => n.startsWith(P)).length, total: k.length }; }),
  };
})()`;

// Sets the synthetic canary set: one cookie (via the page target), localStorage, sessionStorage,
// IndexedDB and Cache Storage. Returns the setCookie result and the in-page result.
export async function setCanary(pg, pageUrl) {
  const u = new URL(pageUrl);
  const cookie = await pg.call('Network.setCookie', {
    name: `${PREFIX}_cookie`,
    value: 'synthetic',
    domain: u.hostname,
    path: '/',
    secure: u.protocol === 'https:',
    expires: Math.floor(Date.now() / 1000) + 86400,
  });
  const stores = await evalIn(pg, SET_JS);
  return { cookieSet: cookie.result?.success === true, stores };
}

// Browser-level cookie jar: total count and canary count. `br` is a connection to the browser target.
export async function jarState(br) {
  const c = (await br.call('Storage.getCookies')).result?.cookies ?? [];
  return { total: c.length, canary: c.filter((k) => k.name.startsWith(PREFIX)).length, cookies: c };
}

export async function pageState(pg) {
  return evalIn(pg, COUNT_JS);
}

// One-line rendering of a pageState: `ls=1/3 ss=1/1 idb=1/2 cache=1/1` (canary/total, `-` = unavailable).
export function fmtPage(s) {
  if (!s || s.error) return `error=${s?.error ?? 'no result'}`;
  const f = (x) => (x ? `${x.canary}/${x.total}` : '-');
  return `ls=${f(s.ls)} ss=${f(s.ss)} idb=${f(s.idb)} cache=${f(s.cache)}`;
}

// Canary entries across the page's stores. sessionStorage is per tab and is not cleared by origin
// clears, so callers that check a clear pass { sessionStorage: false }.
export function canaryTotal(s, { sessionStorage = true } = {}) {
  if (!s || s.error) return 0;
  const kinds = sessionStorage ? ['ls', 'ss', 'idb', 'cache'] : ['ls', 'idb', 'cache'];
  return kinds.reduce((n, k) => n + (s[k]?.canary ?? 0), 0);
}
