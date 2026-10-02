// 探查：能连网、读文件、拿环境变量、加载模块的东西在不在。每一项记下 typeof，
// 动态 import 记下它是同步抛错还是给了一个 Promise。
// 预期：全部不存在；动态 import 不会成功加载任何东西。
export const manifest = { name: "探查宿主能力", api: 1, permissions: ["system"] };

const NAMES = [
  "fetch", "XMLHttpRequest", "WebSocket", "EventSource", "Request", "Response",
  "require", "module", "exports", "process", "Deno", "Bun", "std", "os", "scriptArgs",
  "print", "load", "read", "readFile", "writeFile", "Worker", "importScripts",
  "setTimeout", "setInterval", "setImmediate", "clearTimeout",
  "WebAssembly", "crypto", "navigator", "location", "document", "window", "self",
  "__wasi_fd_write", "wasi", "env", "gc", "queueMicrotask", "performance", "__tw_log",
];

export function onRequest(req) {
  const found = {};
  for (const name of NAMES) {
    found[name] = typeof globalThis[name];
  }
  let dynamicImport;
  try {
    const p = import("os");
    dynamicImport = p instanceof Promise ? "promise" : typeof p;
    p.then(
      () => console.log("dynamic import resolved"),
      () => {},
    );
  } catch (e) {
    dynamicImport = `threw: ${e}`;
  }
  let functionCtor;
  try {
    functionCtor = new Function("return typeof fetch")();
  } catch (e) {
    functionCtor = `threw: ${e}`;
  }
  req.system = JSON.stringify({ found, dynamicImport, functionCtor });
  return req;
}
