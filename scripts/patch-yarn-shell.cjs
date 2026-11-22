#!/usr/bin/env node
'use strict';
// patch-yarn-shell.cjs — fix $? ($-question) support in the vendored
// @yarnpkg/shell. Two defects block `echo $?` / `echo "code=$?"`:
//
//  A. lib/index.js execute() initializes variables['?'] = 0 — a NUMBER.
//     evaluateVariable's default case hands it to split() which calls
//     raw.match → TypeError "raw.match is not a function" on the very
//     first $? use (executeShellLine later stores proper strings, but
//     the first reference crashes before any command completes).
//  B. lib/globUtils.js isGlobPattern() scans the RAW token with
//     micromatch — the '?' inside a $-reference ('a=$?') classifies the
//     whole token as a glob pattern, so the variable is never
//     interpolated and the shell throws `No matches found: "a=$?"`.
//     POSIX sh treats ? after $ as a parameter name, never a glob char.
//     Fix: strip $NAME / ${NAME} references before the glob scan (the
//     substituted VALUE may still legitimately contain glob chars —
//     those get re-checked after interpolation by the caller, which is
//     the correct place).
//
// Idempotent: each patch is guarded by a marker comment. Applied to
// .cache/shell/package (the unpacked tarball — rebuilt on fresh clones,
// so scripts/build-runtime-artifacts.cjs must re-run this after unpack).

const fs = require('node:fs');
const path = require('node:path');

const SHELL_LIB = path.resolve(__dirname, '../.cache/shell/package/lib');
const MARK = '// nodine-patch:';
let changed = 0;

// ---- A. variables['?'] must be a STRING --------------------------------
const idx = path.join(SHELL_LIB, 'index.js');
let idxSrc = fs.readFileSync(idx, 'utf8');
if (!idxSrc.includes(MARK + 'qm-string')) {
  const old = "variables: Object.assign({}, variables, {\n            [`?`]: 0,\n        }),";
  const neu = "variables: Object.assign({}, variables, {\n            [`?`]: `0`, // " + MARK + "qm-string — split() calls raw.match; a number 0 crashed the first $? use\n        }),";
  if (!idxSrc.includes(old)) throw new Error('patch A anchor not found in lib/index.js');
  idxSrc = idxSrc.replace(old, neu);
  fs.writeFileSync(idx, idxSrc);
  changed++;
  console.log('[patch-yarn-shell] A: variables[?] initialized as string');
} else {
  console.log('[patch-yarn-shell] A: already applied');
}

// ---- B. strip $refs before glob detection -------------------------------
const gu = path.join(SHELL_LIB, 'globUtils.js');
let guSrc = fs.readFileSync(gu, 'utf8');
if (!guSrc.includes(MARK + 'qm-noglob')) {
  const old = "function isGlobPattern(pattern) {\n    // The scanner extracts globs from a pattern, but doesn't throw errors\n    if (!micromatch_1.default.scan(pattern, exports.micromatchOptions).isGlob)\n        return false;";
  const neu = "function isGlobPattern(pattern) {\n    // " + MARK + "qm-noglob — a '?' inside a $-reference ('a=$?', '${?}') is a\n    // parameter name, not a glob char; scanning the raw token classified the\n    // whole word as a glob and 'echo a=$?' died with `No matches found`.\n    // Strip $NAME / ${NAME} before the scan; post-interpolation values are\n    // still glob-checked by the caller (interpolateArguments).\n    const stripped = String(pattern)\n        .replace(/\\$\\{[^}]*\\}/g, '')\n        .replace(/\\$[$@*?#a-zA-Z0-9_-]+/g, '');\n    if (stripped !== pattern && !/[*?[]/.test(stripped))\n        return false;\n    // The scanner extracts globs from a pattern, but doesn't throw errors\n    if (!micromatch_1.default.scan(pattern, exports.micromatchOptions).isGlob)\n        return false;";
  if (!guSrc.includes(old)) throw new Error('patch B anchor not found in lib/globUtils.js');
  guSrc = guSrc.replace(old, neu);
  fs.writeFileSync(gu, guSrc);
  changed++;
  console.log('[patch-yarn-shell] B: $-references excluded from glob detection');
} else {
  console.log('[patch-yarn-shell] B: already applied');
}

console.log('[patch-yarn-shell] done (' + changed + ' file(s) changed)');
