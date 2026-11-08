// Tests for netlog.mjs. Run by hand (CI does not run them; items.id=871):
//
//   node --test scripts/rig/netlog.test.mjs
//
// Each test writes a small net log to a temp dir and runs netlog.mjs on it as a child process.
// The fixtures keep the structure of the events a real capture produced (event, source and phase names,
// parameter names, nesting, how a request, a job, a cache-hit result and a connect attempt tie together
// by source id) and invent every value: hostnames under example.test, addresses from the documentation
// ranges (192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24, 2001:db8::/32), no query strings, nothing
// taken from a real log.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const NETLOG = path.join(path.dirname(fileURLToPath(import.meta.url)), 'netlog.mjs');
const EV = {
  URL_REQUEST_START_JOB: 1,
  HOST_RESOLVER_MANAGER_REQUEST: 2,
  HOST_RESOLVER_MANAGER_JOB: 3,
  HOST_RESOLVER_MANAGER_CACHE_HIT: 4,
  TCP_CONNECT_ATTEMPT: 5,
  UDP_CONNECT: 6,
  SOCKET_CONNECT: 7,
  HTTP_STREAM_JOB_CONTROLLER_PRECONNECT: 8,
};
const SRC = { URL_REQUEST: 1, SOCKET: 2, UDP_SOCKET: 3, UDP_CLIENT_SOCKET: 4, NETWORK_QUALITY_ESTIMATOR: 5, HOST_RESOLVER_IMPL_JOB: 6, HTTP_STREAM_JOB_CONTROLLER: 7 };
const PHASE = { NONE: 0, BEGIN: 1, END: 2 };
const CONSTANTS = {
  timeTickOffset: '1700000000000',
  logEventTypes: EV,
  logSourceType: SRC,
  logEventPhase: { PHASE_NONE: 0, PHASE_BEGIN: 1, PHASE_END: 2 },
};

let dir;
before(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), 'netlog-test-'));
});
after(() => {
  fs.rmSync(dir, { recursive: true, force: true });
});

// Event builders: time ticks up by 10 ms per event so offsets are stable.
function builder() {
  let t = 0;
  const ev = (type, src, id, phase, params) => ({
    phase: PHASE[phase],
    source: { id, type: SRC[src] },
    time: String((t += 10)),
    type: EV[type],
    ...(params ? { params } : {}),
  });
  return {
    ev,
    url: (id, method, url) => ev('URL_REQUEST_START_JOB', 'URL_REQUEST', id, 'BEGIN', { method, url }),
    // One resolver request on a source: BEGIN with the host, an optional cache-hit result, END.
    request: (id, host, addrs, speculative = false) => [
      ev('HOST_RESOLVER_MANAGER_REQUEST', 'NETWORK_QUALITY_ESTIMATOR', id, 'BEGIN', { allow_cached_response: true, dns_query_type: 'UNSPECIFIED', host, is_speculative: speculative }),
      ...(addrs ? [ev('HOST_RESOLVER_MANAGER_CACHE_HIT', 'NETWORK_QUALITY_ESTIMATOR', id, 'NONE', { results: { ip_endpoints: addrs.map((a) => ({ endpoint_address: a, endpoint_port: 0 })) } })] : []),
      ev('HOST_RESOLVER_MANAGER_REQUEST', 'NETWORK_QUALITY_ESTIMATOR', id, 'END'),
    ],
    // One resolver job: BEGIN with the host, END with no parameters.
    job: (id, host) => [
      ev('HOST_RESOLVER_MANAGER_JOB', 'HOST_RESOLVER_IMPL_JOB', id, 'BEGIN', { dns_query_types: ['A'], host }),
      ev('HOST_RESOLVER_MANAGER_JOB', 'HOST_RESOLVER_IMPL_JOB', id, 'END'),
    ],
    // One TCP connect attempt: BEGIN with the address, END with os_error on failure and no parameters on success.
    tcp: (id, address, osError) => [
      ev('TCP_CONNECT_ATTEMPT', 'SOCKET', id, 'BEGIN', { address }),
      ev('TCP_CONNECT_ATTEMPT', 'SOCKET', id, 'END', osError === undefined ? undefined : { os_error: osError }),
    ],
    udp: (id, address, netError) => [
      ev('UDP_CONNECT', 'UDP_SOCKET', id, 'BEGIN', { address }),
      ev('UDP_CONNECT', 'UDP_SOCKET', id, 'END', { net_error: netError }),
      ev('SOCKET_CONNECT', 'UDP_CLIENT_SOCKET', id + 1, 'NONE', { address, net_error: netError }),
    ],
  };
}

function writeLog(name, events) {
  const file = path.join(dir, name);
  fs.writeFileSync(file, JSON.stringify({ constants: CONSTANTS, events }));
  return file;
}
function run(file, ...args) {
  const r = spawnSync(process.execPath, [NETLOG, file, ...args], { encoding: 'utf8' });
  return { status: r.status, out: r.stdout, err: r.stderr };
}
// The lines under a group heading of the socket section (indented four spaces) or the DNS section.
function groupLines(out, headingStart) {
  const lines = out.split('\n');
  const i = lines.findIndex((l) => l.startsWith(`  ${headingStart}`));
  assert.notEqual(i, -1, `no "${headingStart}" heading in:\n${out}`);
  const rows = [];
  for (let k = i + 1; k < lines.length && lines[k].startsWith('    '); k++) rows.push(lines[k].trim());
  return rows;
}

// A page host that resolves normally, and a second name the local resolver sinkholes.
function sinkholeScenario() {
  const b = builder();
  return [
    b.url(1, 'GET', 'https://page.example.test/'),
    b.url(2, 'POST', 'https://sinkholed.example.test/collect'),
    b.url(3, 'POST', 'https://sinkholed.example.test/collect'),
    ...b.job(10, 'https://page.example.test'),
    ...b.request(11, 'https://page.example.test', ['203.0.113.7']),
    ...b.request(11, 'https://page.example.test', ['203.0.113.7']),
    ...b.request(12, 'https://page.example.test', ['203.0.113.7']),
    ...b.job(13, 'https://sinkholed.example.test'),
    ...b.request(14, 'https://sinkholed.example.test', ['0.0.0.0', '::']),
    ...b.request(14, 'https://sinkholed.example.test', ['0.0.0.0', '::']),
    ...b.tcp(20, '203.0.113.7:443'),
    ...b.tcp(21, '0.0.0.0:443', 111),
    ...b.tcp(22, '[::]:443', 111),
    ...b.tcp(23, '0.0.0.0:443', 111),
    ...b.udp(30, '[2001:db8::53]:443', -109),
  ];
}

test('DNS: each name shows its request count, its job count and the addresses it resolved to', () => {
  const { out } = run(writeLog('sinkhole.json', sinkholeScenario()));
  assert.match(out, /page\.example\.test {3}requests 3, jobs 1\n\s+resolved to: 203\.0\.113\.7\n/);
  assert.match(out, /sinkholed\.example\.test {3}requests 2, jobs 1\n\s+resolved to: 0\.0\.0\.0, ::/);
  assert.match(out, /resolved only to unspecified addresses: sinkholed/);
  assert.doesNotMatch(out.split('page.example.test   requests')[1].split('\n')[1], /sinkholed/);
});

test('DNS: a request that has BEGIN, a cache hit and END events is one request, not three', () => {
  const b = builder();
  const { out } = run(writeLog('one-request.json', [...b.request(1, 'https://one.example.test', ['192.0.2.1'])]));
  assert.match(out, /one\.example\.test {3}requests 1, jobs 0/);
});

test('DNS: a name with no logged result says so', () => {
  const b = builder();
  const { out } = run(writeLog('no-result.json', [...b.request(1, 'https://bare.example.test', null)]));
  assert.match(out, /bare\.example\.test {3}requests 1, jobs 0\n\s+resolved to: no address logged/);
});

test('connects: the outcome of each attempt is printed per socket address', () => {
  const { out } = run(writeLog('sinkhole.json', sinkholeScenario()));
  assert.match(out, /203\.0\.113\.7:443 x1 \[ok x1\]/);
  assert.match(out, /0\.0\.0\.0:443 x2 \[os_error=111 x2\]/);
  assert.match(out, /\[::\]:443 x1 \[os_error=111 x1\]/);
  assert.match(out, /\[2001:db8::53\]:443 x1 \[net_error=-109 x1\]/);
});

test('connects: an attempt with a BEGIN and no END is reported as having no result', () => {
  const b = builder();
  const events = [b.ev('TCP_CONNECT_ATTEMPT', 'SOCKET', 1, 'BEGIN', { address: '198.51.100.9:443' })];
  const { out } = run(writeLog('open-attempt.json', events));
  assert.match(out, /198\.51\.100\.9:443 x1 \[no result in log x1\]/);
});

test('connects: 0.0.0.0 and :: are listed apart from real non-loopback addresses', () => {
  const { out } = run(writeLog('sinkhole.json', sinkholeScenario()));
  const real = groupLines(out, 'non-loopback TCP/other');
  const unspecified = groupLines(out, 'unspecified address');
  assert.equal(real.length, 1);
  assert.match(real[0], /^203\.0\.113\.7:443/);
  assert.equal(unspecified.length, 2);
  assert.ok(unspecified.some((l) => l.startsWith('0.0.0.0:443')));
  assert.ok(unspecified.some((l) => l.startsWith('[::]:443')));
  assert.ok(groupLines(out, 'non-loopback UDP connect')[0].startsWith('[2001:db8::53]:443'));
});

test('exit: connects to 0.0.0.0 and :: alone exit 0 and are noted, --strict makes them exit 3', () => {
  const b = builder();
  const file = writeLog('unspecified-only.json', [
    b.url(1, 'GET', 'http://127.0.0.1:8181/a.html'),
    ...b.request(2, 'http://127.0.0.1:8181', ['127.0.0.1']),
    ...b.tcp(3, '0.0.0.0:443', 111),
    ...b.tcp(4, '[::]:443', 111),
  ]);
  const plain = run(file);
  assert.equal(plain.status, 0, plain.out);
  assert.match(plain.out, /RESULT: nothing non-loopback \(2 connect attempts to 0\.0\.0\.0 or :: not counted; --strict counts them\)/);
  const strict = run(file, '--strict');
  assert.equal(strict.status, 3, strict.out);
  assert.match(strict.out, /RESULT: NON-LOOPBACK ACTIVITY \(unspecified addresses 2\)/);
});

test('exit: a sinkholed name still exits 3 through the DNS name and URL host rules', () => {
  const b = builder();
  const file = writeLog('sinkholed-name.json', [
    b.url(1, 'POST', 'https://sinkholed.example.test/collect'),
    ...b.request(2, 'https://sinkholed.example.test', ['0.0.0.0', '::']),
    ...b.tcp(3, '0.0.0.0:443', 111),
    ...b.tcp(4, '[::]:443', 111),
  ]);
  const r = run(file);
  assert.equal(r.status, 3, r.out);
  assert.match(r.out, /URL requests to non-loopback hosts 1/);
  assert.match(r.out, /non-loopback DNS names 1/);
  assert.doesNotMatch(r.out, /RESULT:.*non-loopback TCP\/other addresses/);
});

test('exit: a connect to a real non-loopback address exits 3 with no name or URL behind it', () => {
  const b = builder();
  const r = run(writeLog('real-address.json', [...b.tcp(1, '203.0.113.7:443')]));
  assert.equal(r.status, 3, r.out);
  assert.match(r.out, /RESULT: NON-LOOPBACK ACTIVITY \(non-loopback TCP\/other addresses 1\)/);
});

test('exit: a loopback-only log exits 0', () => {
  const b = builder();
  const r = run(writeLog('loopback.json', [b.url(1, 'GET', 'http://127.0.0.1:8181/a.html'), ...b.request(2, 'http://127.0.0.1:8181', ['127.0.0.1']), ...b.tcp(3, '127.0.0.1:8181')]));
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /RESULT: nothing non-loopback$/m);
});

test('exit: a UDP route probe alone exits 0 and --strict makes it exit 3', () => {
  const b = builder();
  const file = writeLog('udp-probe.json', [...b.udp(1, '[2001:db8::53]:443', -109)]);
  const plain = run(file);
  assert.equal(plain.status, 0, plain.out);
  assert.match(plain.out, /\(UDP route probe only\)/);
  assert.equal(run(file, '--strict').status, 3);
});

test('why section: a preconnect to a non-loopback origin is informational unless --strict, and prints no path', () => {
  const b = builder();
  const file = writeLog('preconnect.json', [b.ev('HTTP_STREAM_JOB_CONTROLLER_PRECONNECT', 'HTTP_STREAM_JOB_CONTROLLER', 1, 'NONE', { is_preconnect: true, url: 'https://pre.example.test/deep/path' })]);
  const plain = run(file);
  assert.equal(plain.status, 0, plain.out);
  assert.match(plain.out, /preconnect {2}https:\/\/pre\.example\.test {3}<-- NON-LOOPBACK/);
  assert.doesNotMatch(plain.out, /deep\/path/);
  assert.match(plain.out, /--strict makes it fail/);
  assert.equal(run(file, '--strict').status, 3);
});

test('a log from a killed app is read up to its last complete event', () => {
  const b = builder();
  const events = [...b.request(1, 'https://one.example.test', ['192.0.2.1'])];
  const file = path.join(dir, 'truncated.json');
  fs.writeFileSync(file, `{"constants":${JSON.stringify(CONSTANTS)},\n"events":[\n${events.map((e) => JSON.stringify(e)).join(',\n')},\n{"phase":1,"source":{"id":2,"ty`);
  const r = run(file);
  assert.match(r.out, /truncated \(read to last complete event\)/);
  assert.match(r.out, /one\.example\.test {3}requests 1/);
});

test('usage errors: no argument exits 64, an empty log exits 66', () => {
  assert.equal(spawnSync(process.execPath, [NETLOG], { encoding: 'utf8' }).status, 64);
  const file = path.join(dir, 'empty.json');
  fs.writeFileSync(file, '');
  assert.equal(run(file).status, 66);
});
