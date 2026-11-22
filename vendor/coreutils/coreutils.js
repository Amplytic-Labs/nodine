#!/usr/bin/env node
// nodine coreutils — real Node programs seeded into the runtime image /usr/bin.
// Each is executed by the runtime as a REAL process (node worker + kernel
// syscalls against the VFS). No host-side simulation anywhere.
//
// Conventions: argv parsing minimal (the requested audit surface, not full
// GNU), errors to stderr with non-zero exit, exit codes: 0 ok, 1 error,
// 2 usage. UTF-8 text I/O.
'use strict';
const fs = require('fs');

function die(msg, code = 1) { process.stderr.write(msg + '\n'); halt(code); }
function readStdin() {
  return new Promise((res) => {
    let s = '';
    let settled = false;
    const done = () => { if (!settled) { settled = true; res(s); } };
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', (c) => (s += c));
    process.stdin.on('end', done);
    // E6: the 50ms give-up only applies to an INTERACTIVE tty (a user who
    // typed nothing). A PIPED stdin gets its bytes from an upstream
    // process whose spawn handshake can legitimately take longer than
    // 50ms (multi-stage pipelines) — timing out there read an empty string
    // and 'echo x | cat | cat | wc -c' printed 0. Pipes now wait for the
    // genuine 'end'.
    if (process.stdin.isTTY) setTimeout(done, 50).unref?.();
    process.stdin.resume?.();
  });
}

async function main() {
  let dispatched = false;
  // ---------------------------------------------------------------- ls
  if (process.argv[1].endsWith('/ls')) {
    dispatched = true;
    const long = process.argv.includes('-l') || process.argv.includes('-la') || process.argv.includes('-al');
    const all = process.argv.includes('-a') || process.argv.includes('-la') || process.argv.includes('-al');
    const target = process.argv.slice(2).filter((a) => !a.startsWith('-'))[0] || '.';
    let st;
    try { st = fs.statSync(target); } catch (e) { die(`ls: cannot access '${target}': No such file or directory`); }
    const one = (name, dir) => {
      if (!long) return name;
      const s = fs.statSync(dir ? `${dir}/${name}` : name);
      const type = s.isDirectory() ? 'd' : s.isSymbolicLink() ? 'l' : '-';
      // GNU-style rwx string (was an octal '644' — technically readable but
      // not what any ls user expects from -l)
      const bit = (c, on) => (on ? c : '-');
      const m = s.mode;
      const perm = bit('r', m & 0o400) + bit('w', m & 0o200) + bit('x', m & 0o100)
        + bit('r', m & 0o040) + bit('w', m & 0o020) + bit('x', m & 0o010)
        + bit('r', m & 0o004) + bit('w', m & 0o002) + bit('x', m & 0o001);
      return `${type}${perm} ${String(s.size).padStart(8)} ${name}${s.isDirectory() ? '/' : ''}`;
    };
    if (st.isFile()) { process.stdout.write(one(...(target.includes('/') ? [target.split('/').pop(), target.slice(0, target.lastIndexOf('/'))] : [target, ''])) + '\n'); halt(0); }
    const names = fs.readdirSync(target).filter((n) => all || !n.startsWith('.')).sort();
    if (long) process.stdout.write(`total ${names.length}\n`);
    for (const n of names) process.stdout.write(one(n, target) + '\n');
    halt(0);
  }

  // ---------------------------------------------------------------- cat
  if (process.argv[1].endsWith('/cat')) {
    dispatched = true;
    const files = process.argv.slice(2);
    if (files.length === 0) { readStdin().then((s) => { process.stdout.write(s); halt(0); }); }
    else {
      let out = '';
      for (const f of files) {
        try { out += fs.readFileSync(f, 'utf8'); }
        catch (e) { process.stderr.write(`cat: ${f}: No such file or directory\n`); process.exitCode = 1; }
      }
      process.stdout.write(out);
      halt(process.exitCode || 0);
    }
  }

  // ---------------------------------------------------------------- echo
  if (process.argv[1].endsWith('/echo')) {
    dispatched = true;
    const args = process.argv.slice(2);
    let noNL = false;
    if (args[0] === '-n') { noNL = true; args.shift(); }
    // minimal escape handling for -e
    if (args[0] === '-e') { args.shift(); args[0] = args[0]?.replace(/\\n/g, '\n').replace(/\\t/g, '\t'); }
    process.stdout.write(args.join(' ') + (noNL ? '' : '\n'));
    halt(0);
  }

  // ---------------------------------------------------------------- head/tail
  if (process.argv[1].endsWith('/head') || process.argv[1].endsWith('/tail')) {
    dispatched = true;
    const isHead = process.argv[1].endsWith('/head');
    const argv = process.argv.slice(2);
    let n = 10, files = [];
    for (let i = 0; i < argv.length; i++) {
      if (argv[i] === '-n') n = Number(argv[++i]);
      else if (/^-\d+$/.test(argv[i])) n = Number(argv[i].slice(1));
      else files.push(argv[i]);
    }
    const text = files.length ? fs.readFileSync(files[0], 'utf8') : await readStdin();
    const lines = text.split('\n');
    if (lines[lines.length - 1] === '') lines.pop();
    const out = isHead ? lines.slice(0, n) : lines.slice(-n);
    process.stdout.write(out.join('\n') + (out.length ? '\n' : ''));
    halt(0);
  }

  // ---------------------------------------------------------------- grep
  if (process.argv[1].endsWith('/grep')) {
    dispatched = true;
    const argv = process.argv.slice(2);
    let invert = false, countOnly = false, ignoreCase = false, numberLines = false;
    const files = [];
    for (const a of argv) {
      if (a.startsWith('-') && a.length > 1 && !/^-\d+$/.test(a)) {
        for (const c of a.slice(1)) { if (c === 'v') invert = true; else if (c === 'c') countOnly = true; else if (c === 'i') ignoreCase = true; else if (c === 'n') numberLines = true; }
      } else files.push(a);
    }
    const pattern = files.shift();
    if (!pattern) die('usage: grep [-ivcn] PATTERN [FILE...]', 2);
    let re; try { re = new RegExp(pattern, ignoreCase ? 'i' : ''); } catch (e) { die(`grep: invalid pattern: ${e.message}`, 2); }
    const sources = [];
    if (files.length === 0) sources.push({ name: null, text: await readStdin() });
    else for (const f of files) {
      try { sources.push({ name: f, text: fs.readFileSync(f, 'utf8') }); }
      catch { process.stderr.write(`grep: ${f}: No such file or directory\n`); process.exitCode = 2; }
    }
    const multi = sources.length > 1;
    let matched = 0, total = 0;
    for (const src of sources) {
      const lines = src.text.split('\n');
      if (lines[lines.length - 1] === '') lines.pop();
      let n = 0;
      lines.forEach((line, i) => {
        const hit = re.test(line) !== invert;
        if (hit) { matched++; n++; if (!countOnly) process.stdout.write(`${multi && src.name ? src.name + ':' : ''}${numberLines ? (i + 1) + ':' : ''}${line}\n`); }
      });
      if (countOnly) process.stdout.write(`${multi && src.name ? src.name + ':' : ''}${n}\n`);
      total += n;
    }
    halt(matched > 0 ? 0 : 1);
  }

  // ---------------------------------------------------------------- wc
  if (process.argv[1].endsWith('/wc')) {
    dispatched = true;
    const argv = process.argv.slice(2);
    const flags = { l: false, w: false, c: false };
    let explicit = false;
    const files = [];
    for (const a of argv) {
      if (a.startsWith('-') && a.length > 1) { for (const c of a.slice(1)) if (flags[c] !== undefined) { flags[c] = true; explicit = true; } }
      else files.push(a);
    }
    if (!explicit) flags.l = flags.w = flags.c = true;
    const stats = (text) => {
      const lines = text.split('\n'); if (lines[lines.length - 1] === '') lines.pop();
      return { l: lines.length, w: text.split(/\s+/).filter(Boolean).length, c: Buffer.byteLength(text) };
    };
    const emit = (s, name) => {
      const parts = [];
      if (flags.l) parts.push(String(s.l).padStart(7));
      if (flags.w) parts.push(String(s.w).padStart(7));
      if (flags.c) parts.push(String(s.c).padStart(7));
      process.stdout.write(parts.join('') + (name ? ' ' + name : '') + '\n');
    };
    if (files.length === 0) emit(stats(await readStdin()), null);
    else {
      let total = { l: 0, w: 0, c: 0 };
      for (const f of files) {
        let text; try { text = fs.readFileSync(f, 'utf8'); } catch { process.stderr.write(`wc: ${f}: No such file or directory\n`); process.exitCode = 2; continue; }
        const s = stats(text); emit(s, f);
        total = { l: total.l + s.l, w: total.w + s.w, c: total.c + s.c };
      }
      if (files.length > 1) emit(total, 'total');
    }
    halt(process.exitCode || 0);
  }

  // ---------------------------------------------------------------- touch
  if (process.argv[1].endsWith('/touch')) {
    dispatched = true;
    const files = process.argv.slice(2).filter((a) => !a.startsWith('-'));
    if (!files.length) die('usage: touch FILE...', 2);
    for (const f of files) {
      try { if (fs.existsSync(f)) fs.utimesSync(f, new Date(), new Date()); else fs.writeFileSync(f, ''); }
      catch (e) { process.stderr.write(`touch: cannot touch '${f}': ${e.message}\n`); process.exitCode = 1; }
    }
    halt(process.exitCode || 0);
  }

  // ---------------------------------------------------------------- mkdir / rmdir
  if (process.argv[1].endsWith('/mkdir') || process.argv[1].endsWith('/rmdir')) {
    dispatched = true;
    const isMkdir = process.argv[1].endsWith('/mkdir');
    const argv = process.argv.slice(2);
    const recursive = argv.includes('-p');
    const dirs = argv.filter((a) => !a.startsWith('-'));
    if (!dirs.length) die(`usage: ${isMkdir ? 'mkdir [-p]' : 'rmdir'} DIR...`, 2);
    for (const d of dirs) {
      try { isMkdir ? fs.mkdirSync(d, { recursive }) : fs.rmdirSync(d); }
      catch (e) { process.stderr.write(`${isMkdir ? 'mkdir' : 'rmdir'}: cannot ${isMkdir ? 'create' : 'remove'} '${d}': ${e.message}\n`); process.exitCode = 1; }
    }
    halt(process.exitCode || 0);
  }

  // ---------------------------------------------------------------- rm
  if (process.argv[1].endsWith('/rm')) {
    dispatched = true;
    const argv = process.argv.slice(2);
    const recursive = argv.includes('-r') || argv.includes('-R') || argv.includes('-rf') || argv.includes('-fr');
    const force = argv.includes('-f') || argv.includes('-rf') || argv.includes('-fr');
    const files = argv.filter((a) => !a.startsWith('-'));
    if (!files.length && !force) die('usage: rm [-rf] FILE...', 2);
    for (const f of files) {
      try {
        const st = fs.lstatSync(f);
        if (st.isDirectory() && !recursive) { process.stderr.write(`rm: cannot remove '${f}': Is a directory\n`); process.exitCode = 1; continue; }
        fs.rmSync(f, { recursive, force });
      } catch (e) { if (!force) { process.stderr.write(`rm: cannot remove '${f}': ${e.message}\n`); process.exitCode = 1; } }
    }
    halt(process.exitCode || 0);
  }

  // ---------------------------------------------------------------- cp / mv
  if (process.argv[1].endsWith('/cp') || process.argv[1].endsWith('/mv')) {
    dispatched = true;
    const isCp = process.argv[1].endsWith('/cp');
    const argv = process.argv.slice(2);
    const recursive = argv.includes('-r') || argv.includes('-R');
    const args = argv.filter((a) => !a.startsWith('-'));
    if (args.length < 2) die(`usage: ${isCp ? 'cp [-r]' : 'mv'} SOURCE... DEST`, 2);
    const dest = args.pop();
    const copyOne = (src, dst) => {
      const st = fs.statSync(src);
      if (st.isDirectory()) {
        if (!recursive) { process.stderr.write(`${isCp ? 'cp' : 'mv'}: -r not specified; omitting directory '${src}'\n`); process.exitCode = 1; return; }
        fs.mkdirSync(dst, { recursive: true });
        for (const e of fs.readdirSync(src)) copyOne(`${src}/${e}`, `${dst}/${e}`);
      } else fs.copyFileSync(src, dst);
    };
    let destIsDir = false;
    try { destIsDir = fs.statSync(dest).isDirectory(); } catch {}
    if (args.length > 1 && !destIsDir) die(`${isCp ? 'cp' : 'mv'}: target '${dest}' is not a directory`, 1);
    for (const src of args) {
      const base = src.split('/').pop();
      const dst = destIsDir ? `${dest}/${base}` : dest;
      try {
        if (isCp) copyOne(src, dst);
        else {
          const st = fs.statSync(src);
          if (st.isDirectory()) die(`mv: renaming directories is not supported by the kernel VFS (ABI v1.0.2)`, 1);
          fs.copyFileSync(src, dst); fs.unlinkSync(src);
        }
      } catch (e) { process.stderr.write(`${isCp ? 'cp' : 'mv'}: cannot ${isCp ? 'copy' : 'move'} '${src}' to '${dst}': ${e.message}\n`); process.exitCode = 1; }
    }
    halt(process.exitCode || 0);
  }

  // ---------------------------------------------------------------- find
  if (process.argv[1].endsWith('/find')) {
    dispatched = true;
    const argv = process.argv.slice(2);
    let root = argv[0] && !argv[0].startsWith('-') ? argv.shift() : '.';
    let namePat = null, typeF = null;
    for (let i = 0; i < argv.length; i++) {
      if (argv[i] === '-name') namePat = argv[++i];
      else if (argv[i] === '-type') typeF = argv[++i];
    }
    if (namePat) { const g = namePat.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.'); var re = new RegExp(`^${g}$`); }
    const walk = (dir) => {
      let out = [];
      let entries;
      try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return out; }
      for (const e of entries) {
        const p = `${dir === '.' ? './' : dir + '/'}${e.name}`;
        const t = e.isDirectory() ? 'd' : e.isSymbolicLink() ? 'l' : 'f';
        if ((!namePat || re.test(e.name)) && (!typeF || t === typeF)) out.push(p);
        if (e.isDirectory()) out = out.concat(walk(p));
      }
      return out;
    };
    try { fs.statSync(root); } catch { die(`find: '${root}': No such file or directory`); }
    const st = fs.statSync(root);
    let results = st.isDirectory() ? [root, ...walk(root)] : [root];
    results = results.filter((p) => {
      const name = p.split('/').pop() || p;
      return !namePat || re.test(name) || p === root;
    });
    process.stdout.write(results.join('\n') + (results.length ? '\n' : ''));
    halt(0);
  }

  // ---------------------------------------------------------------- which
  if (process.argv[1].endsWith('/which')) {
    dispatched = true;
    const names = process.argv.slice(2).filter((a) => !a.startsWith('-'));
    if (!names.length) die('usage: which COMMAND...', 2);
    const PATH = (process.env.PATH || '').split(':');
    let found = 0;
    for (const n of names) {
      const hit = PATH.map((d) => `${d}/${n}`).find((p) => { try { fs.accessSync(p, fs.constants.X_OK); return true; } catch { return false; } });
      if (hit) { process.stdout.write(hit + '\n'); found++; }
      else process.exitCode = 1;
    }
    halt(process.exitCode || 0);
  }

  // ---------------------------------------------------------------- env
  if (process.argv[1].endsWith('/env')) {
    dispatched = true;
    const argv = process.argv.slice(2);
    // env [NAME=VALUE...] [COMMAND [ARGS...]]
    const assigns = [];
    let cmd = null, args = [];
    for (let i = 0; i < argv.length; i++) {
      if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(argv[i])) assigns.push(argv[i]);
      else { cmd = argv[i]; args = argv.slice(i + 1); break; }
    }
    if (!cmd) { for (const [k, v] of Object.entries(process.env)) process.stdout.write(`${k}=${v}\n`); halt(0); }
    const { spawn } = require('child_process');
    const env = { ...process.env };
    for (const a of assigns) { const i = a.indexOf('='); env[a.slice(0, i)] = a.slice(i + 1); }
    const child = spawn(cmd, args, { env, stdio: 'inherit' });
    child.on('exit', (c) => halt(c ?? 0));
    child.on('error', (e) => { process.stderr.write(`env: ${cmd}: ${e.message}\n`); halt(127); });
    return; // wait for child exit — do not fall through
  }

  // ---------------------------------------------------------------- chmod
  if (process.argv[1].endsWith('/chmod')) {
    dispatched = true;
    const argv = process.argv.slice(2);
    const modeArg = argv[0];
    const files = argv.slice(1).filter((a) => !a.startsWith('-') || a === '-R');
    if (!modeArg || !files.length) die('usage: chmod MODE FILE...', 2);
    const recursive = argv.includes('-R');
    const applyTo = (p) => {
      try {
        const mode = /^[0-7]+$/.test(modeArg) ? parseInt(modeArg, 8) : null;
        if (mode === null) throw new Error(`invalid mode: '${modeArg}'`);
        fs.chmodSync(p, mode);
      } catch (e) { process.stderr.write(`chmod: cannot change '${p}': ${e.message}\n`); process.exitCode = 1; }
    };
    const walkChmod = (p) => {
      applyTo(p);
      try { if (fs.statSync(p).isDirectory()) for (const e of fs.readdirSync(p)) walkChmod(`${p}/${e}`); } catch {}
    };
    for (const f of files) recursive ? walkChmod(f) : applyTo(f);
    halt(process.exitCode || 0);
  }


  // ------------------------------------------------------- true / false
  // B3 follow-up: previously only the yarn-shell provided these as builtins;
  // the direct-spawn fast path (and any non-shell caller) needs them as real
  // /usr/bin programs like the rest of the set.
  if (process.argv[1].endsWith('/true')) {
    dispatched = true;
    halt(0);
  }
  if (process.argv[1].endsWith('/false')) {
    dispatched = true;
    halt(1);
  }

if (!dispatched) die(`nodine coreutils: unhandled invocation (argv[1]=${process.argv[1]})`, 2);
  return;
}

// Nodine runtime: halt() reports the exit but cannot halt the
// current JS stack (a worker cannot kill itself mid-call). Every
// 'print; halt(0)' pattern therefore LEAKED into the code after
// it — observed live: 'ls -l file' printed the entry then crashed with
// ENOTDIR from the readdir below the exit (LSDBG: isFile=true — the
// isFile branch DID run; execution continued past exit). Command bodies
// throw HALT instead; the single process.exit at the end of main() has
// nothing after it, so nothing can leak.
class Halt { constructor(code) { this.code = code || 0; } }
const halt = (code) => { throw new Halt(code); };
main().then(
  () => { process.exit(0); },
  (e) => {
    if (e instanceof Halt) { process.exit(e.code); return; }
    process.stderr.write(String((e && e.stack) || e) + '\n');
    process.exit(1);
  },
);
