#!/usr/bin/env node
// Snapshot of what a running scratch app's browser holds: targets (type and origin only), the
// browser-level cookie jar as domain -> cookie names, and per page the storage entry counts.
// Never prints values, paths or query strings.
//
//   node jar.mjs [--port N]
import { parseArgs } from './lib/args.mjs';
import { cdpPort, listTargets, originOf, connect, jarState, pageState, fmtPage } from './lib/cdp.mjs';

const { flags } = parseArgs(process.argv.slice(2));
const { version, list } = await listTargets(cdpPort(flags));
console.log('targets:', list.map((t) => `${t.type} ${originOf(t.url)}`).join(' | ') || '(none)');

const br = await connect(version.webSocketDebuggerUrl);
const jar = await jarState(br);
br.close();
const byDomain = {};
for (const k of jar.cookies) (byDomain[k.domain] ||= []).push(k.name);
console.log(`JAR total=${jar.total}`, JSON.stringify(byDomain));

for (const t of list.filter((x) => x.type === 'page' && x.webSocketDebuggerUrl)) {
  const pg = await connect(t.webSocketDebuggerUrl);
  console.log(`PAGE ${originOf(t.url).padEnd(34)} ${fmtPage(await pageState(pg))}`);
  pg.close();
}
