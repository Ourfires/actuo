'use strict';
const {test} = require('node:test');
const assert = require('node:assert/strict');
const {spawnSync} = require('node:child_process');
const path = require('node:path');
const {profile} = require('../src/check-local-network.cjs');

test('exact connect rules preserve filesystem policy without binding or wildcard grants', () => {
  const original = '(version 1)\n(deny default)\n(deny file-read* (literal "/protected"))';
  assert.equal(profile({connect: [55522, 55421, 55522], listen: []}, original), original + '\n' +
    '(allow network-outbound (remote ip "localhost:55421"))\n' +
    '(allow network-outbound (remote ip "localhost:55522"))');
  for (const ports of [[], [0], [-1], [65536], [1.5], ['80'], ['80\")) (allow default)'], Array(65).fill(80)]) {
    assert.throws(() => profile({connect: ports, listen: []}, original));
  }
  assert.throws(() => profile({connect: [80], listen: []}, '(allow default)'));
});

test('preload inserts trusted launcher, scrubs policy inputs, and rejects unknown shell commands', () => {
  const preload = path.resolve(__dirname, '../src/srt-records.cjs');
  const script = `
    const assert = require('node:assert/strict');
    const cp = require('node:child_process');
    Object.defineProperty(process, 'platform', {value: 'darwin'});
    process.env.ACTUO_SRT_LOCAL_PORTS = JSON.stringify({connect: [55421], listen: [56000]});
    process.env.ACTUO_SRT_LOCAL_WRAPPER = "/trusted/actuo's files/wrapper.cjs";
    let captured;
    cp.spawn = (...args) => {captured = args;};
    require(${JSON.stringify(preload)});
    cp.spawn('env HTTP_PROXY=x /usr/bin/sandbox-exec -p policy /bin/bash -c test', {shell: true});
    assert(captured[0].includes(" '" + process.execPath + "' '/trusted/actuo"));
    assert(captured[0].includes(JSON.stringify({connect: [55421], listen: [56000]})));
    assert(captured[0].includes(" -p policy /bin/bash -c test"));
    assert.equal(captured[1].env.ACTUO_SRT_LOCAL_PORTS, undefined);
    assert.equal(captured[1].env.ACTUO_SRT_LOCAL_WRAPPER, undefined);
    assert.equal(captured[1].env.NODE_OPTIONS, undefined);
    assert.throws(() => cp.spawn('unexpected launcher', {shell: true}));
    cp.spawn('ordinary-helper', ['arg'], {});
    assert.equal(captured[0], 'ordinary-helper');
  `;
  const result = spawnSync(process.execPath, ['-e', script], {encoding: 'utf8'});
  assert.equal(result.status, 0, result.stderr);
});

test('listening requires exact explicitly selected ports', () => {
  const original = '(version 1)\n(deny default)';
  const result = profile({connect: [55422], listen: [56000, 56000]}, original);
  assert(result.includes('(allow network-bind (local ip "localhost:56000"))'));
  assert(result.includes('(allow network-inbound (local ip "localhost:56000"))'));
  assert(result.includes('(allow network-outbound (remote ip "localhost:56000"))'));
  assert(!result.includes('local ip "localhost:55422"'));
  assert(!result.includes('ip "*'));
  assert(!result.includes('local ip "localhost:*'));
  for (const listen of [[0], [65536], ['56000'], Array(65).fill(56000)])
    assert.throws(() => profile({connect: [], listen}, original));
  for (const policy of [null, [], {}, {connect: [80]}, {connect: [80], listen: [], extra: true}])
    assert.throws(() => profile(policy, original));
});
