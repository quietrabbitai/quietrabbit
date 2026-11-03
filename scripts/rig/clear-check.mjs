#!/usr/bin/env node
// Site-data clear check: sets the synthetic canary set on a page target, then clears the page's origin
// with DevTools Storage.clearDataForOrigin (storageTypes=all; browser target first, then the page
// target), and reports what is left. If any canary survives, it also runs Network.clearBrowserCookies
// and reports again. sessionStorage is shown but not part of the verdict (it is per tab and goes
// when the tab closes).
// Names/counts only. Exit 0 if every canary is gone at the end, 1 if not, 2 if no page target.
//
//   node clear-check.mjs [--port N] [--host SUBSTR] [--origin ORIGIN]
import { parseArgs } from './lib/args.mjs';
import {
  cdpPort, listTargets, originOf, connect, setCanary, jarState, pageState, fmtPage, canaryTotal,
} from './lib/cdp.mjs';

const { flags } = parseArgs(process.argv.slice(2));
const { version, list } = await listTargets(cdpPort(flags));
let pages = list.filter((t) => t.type === 'page' && t.webSocketDebuggerUrl);
if (flags.host) pages = pages.filter((t) => new URL(t.url).host.includes(flags.host));
const t = pages[0];
if (!t) {
  console.log('no matching page target');
  process.exit(2);
}
const origin = flags.origin ?? originOf(t.url);
const pg = await connect(t.webSocketDebuggerUrl);
const br = await connect(version.webSocketDebuggerUrl);

const report = async (label) => {
  const jar = await jarState(br);
  const s = await pageState(pg);
  console.log(`${label.padEnd(22)} jar canary=${jar.canary} total=${jar.total} | ${fmtPage(s)}`);
  return jar.canary + canaryTotal(s, { sessionStorage: false });
};

const set = await setCanary(pg, t.url);
console.log(`origin ${origin} cookieSet=${set.cookieSet}`);
await report('BEFORE');
// The browser target is tried first; some CEF builds only accept the command on a page target.
for (const [name, conn] of [['browser target', br], ['page target', pg]]) {
  const r = await conn.call('Storage.clearDataForOrigin', { origin, storageTypes: 'all' });
  console.log(`clearDataForOrigin (${name}):`, r.error ? `error ${r.error.message}` : 'ok');
  if (!r.error) break;
}
let left = await report('AFTER origin clear');
if (left > 0) {
  const c = await pg.call('Network.clearBrowserCookies');
  console.log('clearBrowserCookies:', c.error ? `error ${c.error.message}` : 'ok');
  left = await report('AFTER cookie clear');
}
pg.close();
br.close();
console.log(left === 0 ? 'RESULT: all canaries cleared' : `RESULT: ${left} canary item(s) remain`);
process.exit(left === 0 ? 0 : 1);
