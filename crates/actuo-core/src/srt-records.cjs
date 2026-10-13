// Loaded into srt (sandbox-runtime) by actuo through NODE_OPTIONS, with
// `srt --debug`, so actuo can record what the sandbox refuses.
//
// srt's console output (its debug log and its own errors) goes to the pipe
// actuo passed (its descriptor number in ACTUO_SRT_RECORDS_FD), one JSON line
// per message ({"debug", "text"}), never to the stderr srt shares with the command
// it runs: a debug message can span lines, and on a shared stderr its continuation
// lines couldn't be told from the command's output. Without the pipe, debug lines
// are dropped and srt's other messages go to stderr.
//
// The pipe is opened again as a new descriptor (Node opens files close-on-exec)
// and the one passed is closed, so no process srt starts inherits it. Writes
// never wait: the pipe is non-blocking (actuo sets that on its side; the
// open asks for it too, which is what counts on Linux), and a message that finds
// it full is dropped, so a stalled reader can't stall srt and with it the
// command's network. A line a full pipe cut short is finished before any other,
// and {"dropped": n} says how many went missing once there is room again.
// NODE_OPTIONS, SRT_DEBUG and the ACTUO_SRT_* variables are kept out of every
// process srt starts.
'use strict';
const fs = require('fs');
const util = require('util');
const childProcess = require('child_process');
const { syncBuiltinESMExports } = require('module');

// With ACTUO_SRT_LOCAL_PORTS set (macOS only), the final `sandbox-exec` command srt
// builds runs through check-local-network.cjs, which adds exactly those loopback
// ports to srt's profile before any of the repo's commands start. srt runs that
// command with shell: true; any other command shape is refused, never widened.
const localPorts = process.env.ACTUO_SRT_LOCAL_PORTS;
const localWrapper = process.env.ACTUO_SRT_LOCAL_WRAPPER;
delete process.env.ACTUO_SRT_LOCAL_PORTS;
delete process.env.ACTUO_SRT_LOCAL_WRAPPER;
function withLocalPorts(args) {
  if (!localPorts) return args;
  const options = args.find((x, i) => i > 0 && x && typeof x === 'object' && !Array.isArray(x));
  if (!options?.shell) return args;
  const marker = ' /usr/bin/sandbox-exec -p ';
  if (process.platform !== 'darwin' || !localWrapper || typeof args[0] !== 'string' ||
      !args[0].startsWith('env ') || !args[0].includes(marker)) {
    throw new Error('Cannot apply local check service policy to sandbox command');
  }
  const quote = value => "'" + value.replaceAll("'", "'\\''") + "'";
  args[0] = args[0].replace(marker, ` ${quote(process.execPath)} ${quote(localWrapper)} ${quote(localPorts)} -p `);
  return args;
}

const passed = /^\d+$/.test(process.env.ACTUO_SRT_RECORDS_FD || '')
  ? Number(process.env.ACTUO_SRT_RECORDS_FD)
  : -1;
delete process.env.NODE_OPTIONS;
delete process.env.ACTUO_SRT_RECORDS_FD;

let out = -1;
if (passed > 2) {
  try {
    out = fs.openSync(`/dev/fd/${passed}`, fs.constants.O_WRONLY | fs.constants.O_NONBLOCK);
    fs.closeSync(passed);
  } catch {
    out = -1;
  }
}

// The rest of a line a full pipe cut short, and the messages dropped since the
// last one that went through.
let pending = null;
let dropped = 0;

// Writes `buf`, or its start with the rest kept in `pending`; false when the pipe
// took none of it (full) or is gone (actuo stopped reading: no more writes).
function tryWrite(buf) {
  try {
    const n = fs.writeSync(out, buf);
    if (n < buf.length) pending = buf.subarray(n);
    return true;
  } catch (e) {
    if (e.code !== 'EAGAIN') out = -1;
    return false;
  }
}

function flush() {
  if (pending === null) return true;
  const rest = pending;
  pending = null;
  if (!tryWrite(rest)) {
    pending = rest;
    return false;
  }
  return pending === null;
}

function emit(line) {
  if (!flush()) {
    dropped++;
    return;
  }
  if (dropped > 0) {
    if (!tryWrite(Buffer.from(JSON.stringify({ dropped }) + '\n'))) {
      dropped++;
      return;
    }
    dropped = 0;
    if (pending !== null) {
      dropped = 1;
      return;
    }
  }
  if (!tryWrite(Buffer.from(line + '\n'))) dropped++;
}

process.on('exit', () => {
  if (out >= 0 && dropped > 0 && flush()) {
    tryWrite(Buffer.from(JSON.stringify({ dropped }) + '\n'));
  }
});

const stderrWrite = process.stderr.write.bind(process.stderr);
function send(...args) {
  const text = util.format(...args);
  const debug = text.startsWith('[SandboxDebug]');
  if (out >= 0) {
    emit(JSON.stringify({ debug, text }));
  } else if (!debug) {
    stderrWrite(text + '\n');
  }
}
console.error = send;
console.warn = send;

// spawn(command, [args], [options]): the options are the first plain object after
// the command; without one, it goes before the first function (a callback) or at
// the end.
function withoutOurEnv(args) {
  const clean = options => {
    const env = { ...((options && options.env) || process.env) };
    delete env.SRT_DEBUG;
    delete env.NODE_OPTIONS;
    delete env.ACTUO_SRT_RECORDS_FD;
    delete env.ACTUO_SRT_LOCAL_PORTS;
    delete env.ACTUO_SRT_LOCAL_WRAPPER;
    return { ...options, env };
  };
  for (let i = 1; i < args.length; i++) {
    const a = args[i];
    if (a && typeof a === 'object' && !Array.isArray(a)) {
      args[i] = clean(a);
      return args;
    }
  }
  let at = args.findIndex((a, i) => i > 0 && typeof a === 'function');
  if (at < 0) at = args.length;
  args.splice(at, 0, clean({}));
  return args;
}
for (const name of ['spawn', 'spawnSync', 'execFile', 'execFileSync']) {
  const original = childProcess[name];
  childProcess[name] = function (...args) {
    return original.apply(this, withoutOurEnv(withLocalPorts(args)));
  };
}
syncBuiltinESMExports();
