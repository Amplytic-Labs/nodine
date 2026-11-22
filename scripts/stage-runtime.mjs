#!/usr/bin/env node
// stage-runtime.mjs — copy the Nodine runtime bundle out of the installed
// package into a directory your static server serves.
//
//   npx nodine-stage public/nodine
//   npx nodine-stage --root public        # serves the bundle at the origin
//                                        # root (best for previews)
//
// Then boot with:
//   Container.boot({ runtimeUrl: '/nodine/supervisor.mjs' })
// or, for --root staging:
//   Container.boot({ runtimeUrl: '/supervisor.mjs' })

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const RUNTIME = path.resolve(HERE, '../runtime');

const args = process.argv.slice(2);
let atRoot = false;
const dirs = [];
for (const a of args) {
  if (a === '--root') atRoot = true;
  else if (a === '-h' || a === '--help') {
    console.log(fs.readFileSync(fileURLToPath(import.meta.url), 'utf8').split('\n').slice(1, 11).join('\n'));
    process.exit(0);
  } else dirs.push(a);
}
if (dirs.length !== 1) {
  console.error('usage: npx nodine-stage [--root] <output-dir>');
  process.exit(2);
}
const out = path.resolve(process.cwd(), dirs[0]);

if (!fs.existsSync(RUNTIME)) {
  console.error('runtime bundle not found at ' + RUNTIME + ' — broken package install?');
  process.exit(1);
}

fs.mkdirSync(out, { recursive: true });
let n = 0, bytes = 0;
const copy = (src, dst) => {
  for (const e of fs.readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, e.name);
    const d = path.join(dst, e.name);
    if (e.isDirectory()) { fs.mkdirSync(d, { recursive: true }); copy(s, d); }
    else { fs.copyFileSync(s, d); n++; bytes += fs.statSync(s).size; }
  }
};
copy(RUNTIME, out);
const rel = path.relative(process.cwd(), out);
const usable = rel && !rel.startsWith('..') ? rel : out;
const base = usable === '.' ? '' : path.isAbsolute(usable) ? usable : '/' + usable;
console.log(`nodine: staged ${n} files (${(bytes / 1048576).toFixed(1)} MB) -> ${usable}/`);
if (path.isAbsolute(usable)) {
  console.log(`boot with: Container.boot({ runtimeUrl: '${base}/supervisor.mjs' }) (adjust the URL to where your server serves it)`);
} else {
  console.log(`boot with: Container.boot({ runtimeUrl: '${base}/supervisor.mjs' })`);
}
if (!atRoot) {
  console.log('note: previews need the bundle at the origin root, or a Service-Worker-Allowed: / header (see README).');
}
