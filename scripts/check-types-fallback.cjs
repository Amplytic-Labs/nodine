/**
 * Type-checks the .test-d.ts surface file without emitting. Kept as a .cjs
 * fallback so `npm run test:types` works even if the project-level tsc
 * invocation is reorganized; the primary path is tsc -p tsconfig.test.json.
 */
const { spawnSync } = require('node:child_process');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const r = spawnSync(
  process.platform === 'win32' ? 'npx.cmd' : 'npx',
  ['tsc', '-p', 'tsconfig.test.json', '--noEmit'],
  { cwd: root, stdio: 'inherit' },
);
if (r.status !== 0) {
  // The tsc run already printed its errors; surface the exit code.
  process.exit(r.status ?? 1);
}
