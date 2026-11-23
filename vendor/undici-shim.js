'use strict';
// P2-006-008: the libsources shim for internal/deps/undici/undici.
//
// In real Node this module is the ROLLED-UP undici (a build artifact from
// deps/undici — not part of the vendored lib/ tree). Node's own lib lazily
// requires it for: the global fetch implementation, the WHATWG interfaces
// (FormData/Headers/Request/Response/MessageEvent/CloseEvent/ErrorEvent/
// EventSource/WebSocket), createFastMessageEvent, and the global-dispatcher
// plumbing (EnvHttpProxyAgent — guarded by NODE_USE_ENV_PROXY).
//
// In Nodine the transport is the BROWSER's web platform: fetch is the
// platform fetch (already relay-routed by the http bridge), and the WHATWG
// classes are the platform's own. The runtime worker exposes the captured
// platform classes on globalThis.__nodinePlatformWeb BEFORE the Node
// bootstrap replaces the globals with lazy undici-backed versions (reading
// them from globalThis at shim-eval time would be circular).
const P = globalThis.__nodinePlatformWeb || {};

module.exports = {
  // fetch path (internal/bootstrap/web/exposed-window-or-worker.js)
  fetch: P.fetch,
  // WHATWG interfaces (exposeLazyInterfaces lists)
  FormData: P.FormData,
  Headers: P.Headers,
  Request: P.Request,
  Response: P.Response,
  MessageEvent: P.MessageEvent,
  CloseEvent: P.CloseEvent,
  ErrorEvent: P.ErrorEvent,
  EventSource: P.EventSource,
  WebSocket: P.WebSocket,
  // internal/worker/io.js lazyMessageEvent
  createFastMessageEvent(type, init) {
    return new P.MessageEvent(type, init);
  },
  // internal/process/pre_execution.js (NODE_USE_ENV_PROXY — never enabled in
  // the browser runtime; the platform fetch owns proxying)
  setGlobalDispatcher() {},
  getGlobalDispatcher() { return null; },
  EnvHttpProxyAgent: class EnvHttpProxyAgent {},
  ProxyAgent: class ProxyAgent {},
  Agent: class Agent {},
  Dispatcher: class Dispatcher {},
  Pool: class Pool {},
  Client: class Client {},
  // internal/wasm_web_api.js — Response-driven streaming compilation
  ...(P.Response ? {} : {}),
};

// WebAssembly.compileStreaming support (internal/wasm_web_api.js consumes
// fetch + Response through this module's exports — the platform Response
// satisfies it).
