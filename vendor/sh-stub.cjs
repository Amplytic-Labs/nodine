'use strict';
// SH_STUB + NODE_STUB — the /bin/sh and /usr/bin/node seed stubs.
// Extracted verbatim from poc/p2-007/server.cjs (Batch 3) so the artifact
// staging script and the POC server share one source of truth.
// Keep in sync with server.cjs if the POC evolves (the POC remains frozen).
const SH_STUB = '#!/usr/bin/env node\n' +
  'const { execute } = require("/usr/lib/node_modules/shell/lib/index.js");\n' +
  'const argv = process.argv.slice(2);\n' +
  'let command = null, args = [];\n' +
  'if (argv[0] === \'-c\') {\n' +
  '  command = argv[1];\n' +
  '  args = argv.slice(3);\n' +
  '} else if (argv.length > 0) {\n' +
  '  command = require(\'fs\').readFileSync(argv[0], \'utf8\');\n' +
  '  args = argv.slice(1);\n' +
  '}\n' +
  'if (command == null) process.exit(0);\n' +
  'execute(command, args, {\n' +
  '  cwd: process.cwd(),\n' +
  '  stdin: process.stdin,\n' +
  '  stdout: process.stdout,\n' +
  '  stderr: process.stderr,\n' +
  '}).then((code) => process.exit(code), (err) => {\n' +
  '  process.stderr.write(String((err && err.stack) || err) + \'\\n\');\n' +
  '  // hold the loop one beat so the host drains the stdio FIFO before the\n' +
  '  // endpoint dies (an immediate exit discarded the error output — the\n' +
  '  // terminal showed the child dying SILENTLY, observed live).\n' +
  '  setTimeout(() => process.exit(1), 150);\n' +
  '});\n';

const NODE_STUB = '#!/usr/bin/env node\n';
module.exports = { SH_STUB, NODE_STUB };
