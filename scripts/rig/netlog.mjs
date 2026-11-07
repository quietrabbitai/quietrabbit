#!/usr/bin/env node
// Net-log parser. Reads a Chromium net log (the app's --log-net-log=PATH file) and reports:
//   - URL requests, by origin and by method+URL, with time offsets from the start of the capture
//   - socket addresses of connect events (TCP/other and UDP), flagging any non-loopback one
//   - DNS names looked up, flagging any non-loopback one
//   - why: speculative DNS requests (is_speculative) and preconnects (is_preconnect), per source type
//     and event type, with every offset and its UTC wall-clock time (host or origin only, never a
//     path or query string, even with --full)
// A log from a killed app (no closing bracket) is read up to its last complete event.
// Query strings are stripped from printed URLs unless --full is given.
//
//   node netlog.mjs <net.json> [--host-regex RE] [--full] [--strict]
//
// Exit 0: nothing non-loopback. Exit 3: a non-loopback URL host, DNS name or TCP/other socket address.
// A non-loopback UDP connect() alone is printed as a route probe (it sends no packet) and does not
// change the exit code unless --strict is given, which also makes a non-loopback speculative DNS name
// or preconnect origin fail (without --strict that section is informational). Exit 64: bad usage. Exit 66: empty log.
import fs from 'node:fs';
import { parseArgs } from './lib/args.mjs';

const { pos, flags } = parseArgs(process.argv.slice(2), ['full', 'strict']);
if (!pos[0]) {
  console.error('usage: netlog.mjs <net.json> [--host-regex RE] [--full] [--strict]');
  process.exit(64);
}

function load(path) {
  const txt = fs.readFileSync(path, 'utf8');
  if (!txt.trim()) {
    console.error('net log is empty (the app died or was killed before the log was written)');
    process.exit(66);
  }
  try {
    return { j: JSON.parse(txt), truncated: false };
  } catch {
    // keep dropping the last line until the rest closes into valid JSON
  }
  const lines = txt.split('\n');
  for (let i = 0; i < 50 && lines.length > 1; i++) {
    lines.pop();
    try {
      return { j: JSON.parse(lines.join('\n').replace(/,\s*$/, '') + ']}'), truncated: true };
    } catch {
      // try one line fewer
    }
  }
  throw new Error('cannot parse net log');
}

const { j, truncated } = load(pos[0]);
const C = j.constants;
const evName = Object.fromEntries(Object.entries(C.logEventTypes).map(([k, v]) => [v, k]));
const srcName = Object.fromEntries(Object.entries(C.logSourceType).map(([k, v]) => [v, k]));
const base = Number(C.timeTickOffset);
const events = j.events ?? [];
const t0 = events.length ? Math.min(...events.map((e) => Number(e.time))) : 0;
const off = (e) => (Number(e.time) - t0) / 1000;
const clock = (e) => new Date(base + Number(e.time)).toISOString().slice(11, 19);
const hostRe = new RegExp(flags['host-regex'] || '.');

const bareHost = (h) => h.replace(/^\[|\]$/g, '').toLowerCase();
const isLoopback = (h) => {
  h = bareHost(h);
  return /^127\./.test(h) || h === '::1' || /^::ffff:127\./.test(h) || h === 'localhost' || h.endsWith('.localhost');
};
const addrHost = (a) => {
  const m = a.match(/^\[(.*)\]:\d+$/) || a.match(/^(.*):\d+$/);
  return m ? m[1] : a;
};
const stripQuery = (u) => (flags.full ? u : u.replace(/[?#].*$/, ''));

// URL requests, one per URL_REQUEST source id.
const reqs = new Map();
for (const e of events) {
  if (evName[e.type] !== 'URL_REQUEST_START_JOB' || srcName[e.source.type] !== 'URL_REQUEST') continue;
  const p = e.params || {};
  if (!p.url || reqs.has(e.source.id)) continue;
  reqs.set(e.source.id, { off: off(e), clk: clock(e), method: p.method || '?', url: p.url });
}
const reqList = [...reqs.values()].filter((r) => hostRe.test(r.url)).sort((a, b) => a.off - b.off);

const origins = new Map();
const groups = new Map();
let nonLoopbackUrls = 0;
for (const r of reqList) {
  let u;
  try {
    u = new URL(r.url);
  } catch {
    continue;
  }
  const o = origins.get(u.origin) || { n: 0, first: r.off, last: r.off, clk: r.clk, loop: isLoopback(u.hostname) };
  o.n++;
  o.last = r.off;
  origins.set(u.origin, o);
  const key = `${r.method} ${stripQuery(r.url)}`;
  const g = groups.get(key) || { n: 0, first: r.off, last: r.off };
  g.n++;
  g.last = r.off;
  groups.set(key, g);
  if (!o.loop) nonLoopbackUrls++;
}

// Socket addresses: any connect-type event carrying an address.
const socks = new Map(); // `${kind} ${address}` -> { perName: {eventName: n} }
for (const e of events) {
  const n = evName[e.type];
  const a = e.params?.address;
  if (typeof a !== 'string' || !/CONNECT/.test(n)) continue;
  const kind = /UDP/.test(n) || /UDP/.test(srcName[e.source.type] || '') ? 'udp' : 'tcp/other';
  const k = `${kind} ${a}`;
  const s = socks.get(k) || { kind, a, perName: {} };
  s.perName[n] = (s.perName[n] || 0) + 1;
  socks.set(k, s);
}
const sockRows = [...socks.values()].map((s) => ({ ...s, n: Math.max(...Object.values(s.perName)), loop: isLoopback(addrHost(s.a)) }));

// DNS names.
const dns = new Map();
for (const e of events) {
  const h = e.params?.host;
  if (typeof h !== 'string' || !/^HOST_RESOLVER_MANAGER_(REQUEST|JOB)$/.test(evName[e.type])) continue;
  const name = h.replace(/^[a-z]+:\/\//, '').replace(/:\d+$/, '');
  dns.set(name, (dns.get(name) || 0) + 1);
}
const nonLoopbackDns = [...dns.keys()].filter((h) => !isLoopback(h));

// Why: speculative DNS and preconnects, by source. Keyed on the event parameters (is_speculative with a
// host, is_preconnect with a url), not on event names. Names (host or origin only), counts and times only:
// a preconnect URL is reduced to its origin, never a path or query, even with --full.
const hostOfSpec = (h) => {
  try {
    if (/^[a-z][a-z0-9+.-]*:\/\//i.test(h)) return new URL(h).hostname;
  } catch {
    // fall through to the plain strip
  }
  return h.replace(/:\d+$/, '');
};
const whySrc = new Map(); // `${kind}|${source}|${event}|${name}` -> { kind, source, event, name, loop, offs, firstE, lastE }
for (const e of events) {
  const p = e.params;
  let kind;
  let name;
  let loop;
  if (p && p.is_speculative === true && typeof p.host === 'string') {
    kind = 'speculative DNS';
    name = hostOfSpec(p.host);
    loop = isLoopback(name);
  } else if (p && p.is_preconnect === true && typeof p.url === 'string') {
    kind = 'preconnect';
    try {
      const u = new URL(p.url);
      name = u.origin;
      loop = isLoopback(u.hostname);
    } catch {
      continue;
    }
  } else {
    continue;
  }
  const key = `${kind}|${srcName[e.source.type]}|${evName[e.type]}|${name}`;
  const r = whySrc.get(key) || { kind, source: srcName[e.source.type] || '?', event: evName[e.type] || '?', name, loop, offs: [], firstE: e, lastE: e };
  r.offs.push(off(e));
  if (off(e) < off(r.firstE)) r.firstE = e;
  if (off(e) > off(r.lastE)) r.lastE = e;
  whySrc.set(key, r);
}
const whyRows = [...whySrc.values()].sort((a, b) => off(a.firstE) - off(b.firstE));
const nonLoopWhy = whyRows.filter((r) => !r.loop);

const f1 = (x) => `+${x.toFixed(1)}s`;
console.log(`net log: ${events.length} events, span ${f1((Math.max(...events.map((e) => Number(e.time))) - t0) / 1000)}${truncated ? ', truncated (read to last complete event)' : ''}`);
console.log(`\nURL requests: ${reqList.length}`);
for (const [o, v] of [...origins].sort((a, b) => a[1].first - b[1].first)) {
  console.log(`  ${String(v.n).padStart(4)}  ${o}${v.loop ? '' : '   <-- NON-LOOPBACK'}  first ${f1(v.first)} (${v.clk} UTC) last ${f1(v.last)}`);
}
if (groups.size) console.log('  by method and URL:');
for (const [k, v] of [...groups].sort((a, b) => a[1].first - b[1].first)) {
  console.log(`  ${String(v.n).padStart(4)}  ${k}  ${v.n > 1 ? `${f1(v.first)}..${f1(v.last)}` : f1(v.first)}`);
}

console.log('\nSocket addresses (connect events):');
const nonLoopTcp = sockRows.filter((s) => !s.loop && s.kind === 'tcp/other');
const nonLoopUdp = sockRows.filter((s) => !s.loop && s.kind === 'udp');
const loopRows = sockRows.filter((s) => s.loop);
console.log(`  loopback: ${loopRows.length ? loopRows.map((s) => `${s.a} x${s.n}`).join(', ') : 'none'}`);
console.log(`  non-loopback TCP/other: ${nonLoopTcp.length ? nonLoopTcp.map((s) => `${s.a} x${s.n} (${Object.keys(s.perName).join('+')})`).join(', ') : 'none'}`);
console.log(`  non-loopback UDP connect (route probe, sends no packet): ${nonLoopUdp.length ? nonLoopUdp.map((s) => `${s.a} x${s.n} (${Object.keys(s.perName).join('+')})`).join(', ') : 'none'}`);

console.log(`\nDNS names looked up: ${dns.size ? [...dns].map(([h, n]) => `${h} x${n}`).join(', ') : 'none'}`);

console.log('\nSpeculative DNS and preconnects, by source (names, counts and offsets only):');
if (!whyRows.length) console.log('  none');
for (const r of whyRows) {
  const sorted = [...r.offs].sort((a, b) => a - b);
  const shown = sorted.slice(0, 12).map(f1).join(' ') + (sorted.length > 12 ? ` ... (+${sorted.length - 12} more)` : '');
  console.log(`  ${String(r.offs.length).padStart(4)}  ${r.kind}  ${r.name}${r.loop ? '' : '   <-- NON-LOOPBACK'}`);
  console.log(`        source ${r.source}, event ${r.event}`);
  console.log(`        first ${f1(off(r.firstE))} (${clock(r.firstE)} UTC) last ${f1(off(r.lastE))} (${clock(r.lastE)} UTC)`);
  console.log(`        at ${shown}`);
}

const fail =
  nonLoopbackUrls > 0 ||
  nonLoopTcp.length > 0 ||
  nonLoopbackDns.length > 0 ||
  (flags.strict && (nonLoopUdp.length > 0 || nonLoopWhy.length > 0));
const whyNote = !fail && nonLoopWhy.length ? ' (speculative DNS or preconnect to a non-loopback name listed above; --strict makes it fail)' : '';
console.log(`\nRESULT: ${fail ? 'NON-LOOPBACK ACTIVITY' : 'nothing non-loopback'}${!fail && nonLoopUdp.length ? ' (UDP route probe only)' : ''}${whyNote}`);
process.exit(fail ? 3 : 0);
