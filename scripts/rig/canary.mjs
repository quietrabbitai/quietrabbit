#!/usr/bin/env node
// Set or read a synthetic canary set on page target(s) of a running scratch app:
// cookie (browser-level jar), localStorage, sessionStorage, IndexedDB, Cache Storage.
// Prints names/counts only, never values.
//
//   node canary.mjs set|read [--port N] [--host SUBSTR] [--all-pages] [--expect present|absent]
//
// Output per store is canary/total. --expect makes the exit code a check:
//   absent  : exit 1 if any canary exists in the jar or in any selected page's stores
//   present : exit 1 if the cookie, localStorage, IndexedDB or Cache Storage canary is missing
//             on any selected page (sessionStorage is not required: it does not outlive a reload)
// Exit 2: no page target to act on. Port: --port, else RIG_CDP_PORT, else 9222.
import { parseArgs } from './lib/args.mjs';
import {
  cdpPort, listTargets, originOf, connect, setCanary, jarState, pageState, fmtPage, canaryTotal,
} from './lib/cdp.mjs';

const { pos, flags } = parseArgs(process.argv.slice(2), ['all-pages']);
const mode = pos[0];
if (!['set', 'read'].includes(mode) || (flags.expect && !['present', 'absent'].includes(flags.expect))) {
  console.error('usage: canary.mjs set|read [--port N] [--host SUBSTR] [--all-pages] [--expect present|absent]');
  process.exit(64);
}

const { version, list } = await listTargets(cdpPort(flags));
console.log('targets:', list.map((t) => `${t.type} ${originOf(t.url)}`).join(' | ') || '(none)');
let pages = list.filter((t) => t.type === 'page' && t.webSocketDebuggerUrl);
if (flags.host) pages = pages.filter((t) => new URL(t.url).host.includes(flags.host));
if (!flags['all-pages']) pages = pages.slice(0, 1);
if (!pages.length) {
  console.log('no matching page target');
  process.exit(2);
}

const br = await connect(version.webSocketDebuggerUrl);
let bad = false;
const states = [];
for (const t of pages) {
  const pg = await connect(t.webSocketDebuggerUrl);
  if (mode === 'set') {
    const r = await setCanary(pg, t.url);
    console.log(`SET ${originOf(t.url)} cookieSet=${r.cookieSet}${r.stores?.error ? ` stores-error=${r.stores.error}` : ''}`);
  }
  states.push({ t, s: await pageState(pg) });
  pg.close();
}
const jar = await jarState(br);
br.close();
console.log(`${mode.toUpperCase()} jar total=${jar.total} canary=${jar.canary}`);
for (const { t, s } of states) console.log(`  ${originOf(t.url).padEnd(34)} ${fmtPage(s)}`);

if (flags.expect === 'absent') {
  bad = jar.canary > 0 || states.some(({ s }) => canaryTotal(s) > 0);
} else if (flags.expect === 'present') {
  bad = jar.canary === 0 || states.some(({ s }) => !s || s.error || ['ls', 'idb', 'cache'].some((k) => s[k] && s[k].canary === 0));
}
if (flags.expect) console.log(`expect ${flags.expect}: ${bad ? 'FAIL' : 'ok'}`);
process.exit(bad ? 1 : 0);
