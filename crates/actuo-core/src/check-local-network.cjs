'use strict';

// Run by srt-records.cjs in place of `sandbox-exec`: adds loopback connect (and,
// separately, listen) rules for the given ports to srt's profile and keeps the rest
// of it. Seatbelt's localhost bind rule also matches wildcard binds, so a listening
// port can be reached from the network (see `checks_local_listen_ports`); test
// servers should bind 127.0.0.1.
const { spawnSync } = require('node:child_process');

function profile(policy, original) {
  if (!policy || Array.isArray(policy) || Object.keys(policy).sort().join(',') !== 'connect,listen')
    throw new Error('Invalid local check policy');
  const {connect, listen} = policy;
  for (const ports of [connect, listen]) {
    if (!Array.isArray(ports) || ports.length > 64 ||
        ports.some(p => !Number.isInteger(p) || p < 1 || p > 65535))
      throw new Error('Invalid local check ports');
  }
  if (!connect.length && !listen.length) throw new Error('Empty local check policy');
  if (typeof original !== 'string' || !original.startsWith('(version 1)'))
    throw new Error('Invalid generated sandbox policy');
  const unique = ports => [...new Set(ports)].sort((a, b) => a - b);
  return [
    original,
    ...unique([...connect, ...listen]).map(p =>
      `(allow network-outbound (remote ip "localhost:${p}"))`),
    ...unique(listen).flatMap(p => [
      `(allow network-bind (local ip "localhost:${p}"))`,
      `(allow network-inbound (local ip "localhost:${p}"))`,
    ]),
  ].join('\n');
}

if (require.main === module) {
  let rules;
  try {
    if (process.platform !== 'darwin' || process.argv[3] !== '-p' || !process.argv[5]) {
      throw new Error('Unsupported invocation');
    }
    rules = profile(JSON.parse(process.argv[2]), process.argv[4]);
  } catch {
    console.error('Cannot enforce local check service policy; command was not started.');
    process.exit(1);
  }
  const result = spawnSync('/usr/bin/sandbox-exec', ['-p', rules, ...process.argv.slice(5)], {
    stdio: 'inherit', env: process.env,
  });
  if (result.signal) process.kill(process.pid, result.signal);
  process.exit(result.status ?? 1);
}

module.exports = { profile };
