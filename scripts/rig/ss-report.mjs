#!/usr/bin/env node
// Report on a file written by ss-sample.sh: every distinct remote address, with the owning process
// names, sample count and first/last sample time. Non-loopback remotes are flagged.
//
//   node ss-report.mjs <samples-file>
//
// Exit 0: no non-loopback remote address seen. Exit 3: at least one. Exit 64: bad usage.
// Unconnected sockets (peer `*`, `0.0.0.0:*`, `[::]:*`) have no remote and are ignored.
import fs from 'node:fs';

if (!process.argv[2]) {
  console.error('usage: ss-report.mjs <samples-file>');
  process.exit(64);
}

const addrHost = (a) => {
  const m = a.match(/^\[(.*)\]:[^:]+$/) || a.match(/^(.*):[^:]+$/);
  return (m ? m[1] : a).replace(/%.*$/, '').toLowerCase();
};
const isLoopback = (h) => /^127\./.test(h) || h === '::1' || /^::ffff:127\./.test(h);
const isWildcard = (a) => /^(\*|0\.0\.0\.0|\[::\]|::)?:?\*$/.test(a) || /:\*$/.test(a);

let samples = 0;
let rootNone = 0;
let clock = '';
const remotes = new Map();
for (const line of fs.readFileSync(process.argv[2], 'utf8').split('\n')) {
  if (!line.trim()) continue;
  if (line.startsWith('#')) {
    samples++;
    clock = line.split(/\s+/)[1];
    if (/root=none/.test(line)) rootNone++;
    continue;
  }
  const [proto, state, , peer, users = ''] = line.split(/\s+/);
  if (!peer || isWildcard(peer)) continue;
  const names = [...users.matchAll(/\("([^"]+)",pid=(\d+)/g)].map((m) => m[1]);
  const key = `${proto} ${peer}`;
  const r = remotes.get(key) || { proto, peer, states: new Set(), names: new Set(), n: 0, first: clock, last: clock };
  r.states.add(state);
  names.forEach((x) => r.names.add(x));
  r.n++;
  r.last = clock;
  remotes.set(key, r);
}

const rows = [...remotes.values()];
const loop = rows.filter((r) => isLoopback(addrHost(r.peer)));
const non = rows.filter((r) => !isLoopback(addrHost(r.peer)));
console.log(`samples: ${samples}${rootNone ? ` (${rootNone} with no process)` : ''}`);
console.log(`loopback remotes: ${loop.length ? loop.map((r) => `${r.proto} ${r.peer} x${r.n} [${[...r.names].join(',')}]`).join('; ') : 'none'}`);
if (!non.length) {
  console.log('non-loopback remotes: none');
} else {
  console.log('non-loopback remotes:');
  for (const r of non) {
    console.log(`  ${r.proto} ${r.peer} ${[...r.states].join('/')} x${r.n} ${r.first}..${r.last}  process: ${[...r.names].join(',') || '(unknown)'}`);
  }
}
console.log(`RESULT: ${non.length ? 'NON-LOOPBACK REMOTE' : 'no non-loopback remote'}`);
process.exit(non.length ? 3 : 0);
