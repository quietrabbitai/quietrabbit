#!/usr/bin/env node
// Net-log parser. Reads a Chromium net log (the app's --log-net-log=PATH file) and reports:
//   - URL requests, by origin and by method+URL, with time offsets from the start of the capture
//   - socket addresses of connect events (TCP/other and UDP) with the outcome of each connect attempt
//     (ok, os_error=N or net_error=N), in four groups: loopback, non-loopback TCP/other, unspecified
//     (0.0.0.0 and ::, which Linux treats as the local host and which a sinkholing resolver hands out)
//     and non-loopback UDP
//   - DNS names looked up, each with its request count, its resolver-job count and the addresses the
//     log shows it resolved to (flagging any non-loopback name, and a name that resolved only to
//     unspecified addresses)
//   - why: speculative DNS requests (is_speculative) and preconnects (is_preconnect), per source type
//     and event type, with every offset and its UTC wall-clock time (host or origin only, never a
//     path or query string, even with --full)
// A log from a killed app (no closing bracket) is read up to its last complete event.
// Query strings are stripped from printed URLs unless --full is given.
//
//   node netlog.mjs <net.json> [--host-regex RE] [--full] [--strict]
//
// Exit 0: nothing non-loopback. Exit 3: a non-loopback URL host, DNS name or TCP/other socket address.
// Not counted by themselves (printed, and noted on the RESULT line): a TCP/other connect to an
// UNSPECIFIED address (0.0.0.0, ::; items.id=871), and a non-loopback UDP connect(), which is
// Chromium's IPv6 route probe and sends no packet. A name that resolved to an unspecified address is
// still a looked-up non-loopback name and its URL host is still a non-loopback origin, so such a
// sinkholed name keeps the exit at 3 through those two rules; only an unspecified address with no
// non-loopback name or URL behind it stops failing the run. --strict also fails on an unspecified or
// non-loopback UDP address, and on a non-loopback speculative DNS name or preconnect origin (without
// --strict that section is informational). Exit 64: bad usage. Exit 66: empty log.
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
const PH = C.logEventPhase ?? { PHASE_NONE: 0, PHASE_BEGIN: 1, PHASE_END: 2 };
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
// 0.0.0.0 and :: (also ::ffff:0.0.0.0 and the long form): "this host", what a sinkholing resolver returns.
const isUnspecified = (h) => {
  h = bareHost(h);
  return h === '0.0.0.0' || h === '::ffff:0.0.0.0' || /^(0{1,4}:){2,7}0{0,4}$/.test(h) || h === '::';
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
// Outcome of each connect attempt: the BEGIN of TCP_CONNECT_ATTEMPT / UDP_CONNECT carries the address, the
// END of the same source carries os_error (TCP) or net_error (UDP) on failure and no such parameter on success.
const ATTEMPT = { TCP_CONNECT_ATTEMPT: 'tcp/other', UDP_CONNECT: 'udp' };
const openAttempt = new Map(); // `${source id}|${event}` -> address
const outcomes = new Map(); // `${kind} ${address}` -> { label: n }
const addOutcome = (kind, a, label) => {
  const o = outcomes.get(`${kind} ${a}`) || {};
  o[label] = (o[label] || 0) + 1;
  outcomes.set(`${kind} ${a}`, o);
};
for (const e of events) {
  const n = evName[e.type];
  if (!(n in ATTEMPT)) continue;
  const key = `${e.source.id}|${n}`;
  if (e.phase === PH.PHASE_BEGIN && typeof e.params?.address === 'string') {
    openAttempt.set(key, e.params.address);
  } else if (e.phase === PH.PHASE_END && openAttempt.has(key)) {
    const p = e.params || {};
    const label = p.os_error !== undefined ? `os_error=${p.os_error}` : p.net_error !== undefined ? `net_error=${p.net_error}` : 'ok';
    addOutcome(ATTEMPT[n], openAttempt.get(key), label);
    openAttempt.delete(key);
  }
}
for (const [key, a] of openAttempt) addOutcome(ATTEMPT[key.split('|')[1]], a, 'no result in log');
const sockRows = [...socks.values()].map((s) => {
  const h = addrHost(s.a);
  const group = isLoopback(h) ? 'loopback' : isUnspecified(h) ? 'unspecified' : 'non-loopback';
  return { ...s, n: Math.max(...Object.values(s.perName)), group, out: outcomes.get(`${s.kind} ${s.a}`) };
});

// DNS names. Counted per name: requests (HOST_RESOLVER_MANAGER_REQUEST BEGIN events: one per lookup request,
// most answered from the cache) and jobs (HOST_RESOLVER_MANAGER_JOB BEGIN events: actual resolver jobs, which
// share one resolution between requests). The addresses come from HOST_RESOLVER_MANAGER_CACHE_HIT results,
// attached to the name of the request open on the same source; a resolution the log does not show a result for
// has no addresses here.
const dns = new Map(); // name -> { requests, jobs, addrs: Set }
const dnsEntry = (name) => {
  if (!dns.has(name)) dns.set(name, { requests: 0, jobs: 0, addrs: new Set() });
  return dns.get(name);
};
const openReq = new Map(); // source id -> name of the request in flight on it
for (const e of events) {
  const n = evName[e.type];
  const h = e.params?.host;
  if (n === 'HOST_RESOLVER_MANAGER_REQUEST' || n === 'HOST_RESOLVER_MANAGER_JOB') {
    if (typeof h === 'string') {
      const name = h.replace(/^[a-z]+:\/\//, '').replace(/:\d+$/, '');
      const d = dnsEntry(name);
      if (n === 'HOST_RESOLVER_MANAGER_REQUEST') {
        d.requests++;
        openReq.set(e.source.id, name);
      } else {
        d.jobs++;
      }
    } else if (n === 'HOST_RESOLVER_MANAGER_REQUEST' && e.phase === PH.PHASE_END) {
      openReq.delete(e.source.id);
    }
  } else if (n === 'HOST_RESOLVER_MANAGER_CACHE_HIT' && openReq.has(e.source.id)) {
    for (const ep of e.params?.results?.ip_endpoints ?? []) {
      if (typeof ep?.endpoint_address === 'string') dnsEntry(openReq.get(e.source.id)).addrs.add(ep.endpoint_address);
    }
  }
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

console.log('\nSocket addresses (connect events), with the outcome of each connect attempt:');
const inGroup = (g, kind) => sockRows.filter((s) => s.group === g && (!kind || s.kind === kind));
const loopRows = inGroup('loopback');
const nonLoopTcp = inGroup('non-loopback', 'tcp/other');
const nonLoopUdp = inGroup('non-loopback', 'udp');
const unspecRows = inGroup('unspecified');
const fmtOut = (s) => (s.out ? Object.entries(s.out).map(([l, n]) => `${l} x${n}`).join(', ') : 'outcome not logged');
const printGroup = (title, rows) => {
  console.log(`  ${title}:${rows.length ? '' : ' none'}`);
  for (const s of rows) console.log(`    ${s.a} x${s.n} [${fmtOut(s)}] (${Object.keys(s.perName).join('+')})`);
};
printGroup('loopback', loopRows);
printGroup('non-loopback TCP/other', nonLoopTcp);
printGroup('unspecified address, 0.0.0.0 or :: (not counted by itself; Linux treats it as the local host; usually a resolver sinkhole)', unspecRows);
printGroup('non-loopback UDP connect (route probe, sends no packet)', nonLoopUdp);

const nameNote = (d) => {
  const addrs = [...d.addrs];
  if (addrs.length && addrs.every(isUnspecified)) return '   (resolved only to unspecified addresses: sinkholed)';
  if (addrs.some(isUnspecified)) return '   (includes an unspecified address)';
  return '';
};
console.log('\nDNS names looked up (requests = resolver requests, mostly cache hits; jobs = resolver jobs, one per real resolution):');
if (!dns.size) console.log('  none');
for (const [name, d] of dns) {
  const shown = [...d.addrs].slice(0, 8).join(', ') + (d.addrs.size > 8 ? ` ... (+${d.addrs.size - 8} more)` : '');
  console.log(`  ${name}   requests ${d.requests}, jobs ${d.jobs}`);
  console.log(`        resolved to: ${d.addrs.size ? shown : 'no address logged'}${nameNote(d)}`);
}

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

const unspecAttempts = unspecRows.reduce((t, r) => t + r.n, 0);
const strictOnly = [];
if (flags.strict && nonLoopUdp.length) strictOnly.push(`UDP addresses ${nonLoopUdp.length}`);
if (flags.strict && unspecRows.length) strictOnly.push(`unspecified addresses ${unspecRows.length}`);
if (flags.strict && nonLoopWhy.length) strictOnly.push(`speculative DNS or preconnect names ${nonLoopWhy.length}`);
const reasons = [
  nonLoopbackUrls > 0 && `URL requests to non-loopback hosts ${nonLoopbackUrls}`,
  nonLoopTcp.length > 0 && `non-loopback TCP/other addresses ${nonLoopTcp.length}`,
  nonLoopbackDns.length > 0 && `non-loopback DNS names ${nonLoopbackDns.length}`,
  ...strictOnly,
].filter(Boolean);
const fail = reasons.length > 0;
const whyNote = !fail && nonLoopWhy.length ? ' (speculative DNS or preconnect to a non-loopback name listed above; --strict makes it fail)' : '';
const unspecNote =
  !flags.strict && unspecRows.length
    ? ` (${unspecAttempts} connect attempt${unspecAttempts === 1 ? '' : 's'} to 0.0.0.0 or :: not counted; --strict counts them)`
    : '';
console.log(
  `\nRESULT: ${fail ? `NON-LOOPBACK ACTIVITY (${reasons.join('; ')})` : 'nothing non-loopback'}${!fail && nonLoopUdp.length ? ' (UDP route probe only)' : ''}${whyNote}${unspecNote}`,
);
process.exit(fail ? 3 : 0);
