# vendor/ — upstream inputs

Everything the runtime build derives its shipped artifacts from. Nothing in
here is Nodine-authored except `coreutils/` and `undici-shim.js` (see the
root NOTICE for the license inventory).

| Path | What it is | Used for |
| --- | --- | --- |
| `node-v26.13.0/lib/` + `LICENSE` | the `lib/` subset of the upstream Node.js **v26.13.0** source tree (MIT-style, "Copyright Node.js contributors"). The full upstream source is at https://github.com/nodejs/node (tag `v26.13.0`); only `lib/` participates in the build. | `scripts/build-runtime.mjs` reads it verbatim into `runtime/libsources.json` (ids are extensionless — the runtime worker's builtin loader requests `_http_agent`, not `_http_agent.js`). |
| `deps/*.tgz` | pinned upstream package tarballs: `npm 12.2.2` (Artistic-2.0), `@yarnpkg/shell 4.1.3` (BSD-2-Clause), `pako 2.1.0` (MIT AND Zlib), `@noble/hashes 1.8.0` (MIT), `js-md5 0.8.3` (MIT), `acorn 8.15.0` (MIT). | unpacked into `.cache/` and embedded into `runtime/npm-seed-blob` (the container's `/usr` tree) and `runtime/unpkg/` (the worker-imported ESM). `acorn` is the provenance tarball for `runtime/acorn-vendor.mjs` and `internal-deps/acorn`. |
| `internal-deps/` | Node's `internal/deps` dist copies (`acorn`, `acorn-walk`, `minimatch`) that the vendored `lib/` tree does not carry but `require('internal/deps/...')` needs. | injected into `libsources.json` with the exact ids Node uses. |
| `undici-shim.js` | Nodine-authored browser shim standing in for Node's internal `undici` (global fetch → browser APIs). | injected into `libsources.json` as `internal/deps/undici/undici`. |
| `uv-errno-linux.txt` | the libuv errno table for the linux build. | compiled into `runtime/uvmap.json` (the runtime worker's errno map). |
| `sh-stub.cjs` | source of the `/bin/sh` stub seeded into the container (executes commands through the vendored `@yarnpkg/shell`). | embedded into `runtime/npm-seed-blob`. |
| `coreutils/coreutils.js` | Nodine's multi-call `/usr/bin` coreutils (`ls`, `cat`, `grep`, …; argv[1] basename dispatch). | split per command and seeded as real executable files into the container image. |

The `/bin/sh` dependency **closure** (the shell tarball ships without its
deps) is not vendored as files: `scripts/build-shell-deps.cjs` installs the
pinned closure from the registry once into `.cache/` when the runtime bundle
is rebuilt. The closure itself is committed inside
`runtime/npm-seed-blob`, so publishing never needs network access.
