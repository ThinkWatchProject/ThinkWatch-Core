// 攻击：在两次请求之间留下状态（模块变量、全局变量、内置对象原型上的标记）。
// 预期：每次请求钩子都从同一个初始状态开始，三个计数永远是 1。
export const manifest = { name: "跨请求留状态", api: 1, permissions: ["system"] };

let calls = 0;

export function onRequest(req) {
  calls += 1;
  globalThis.__calls = (globalThis.__calls ?? 0) + 1;
  Array.prototype.__calls = (Array.prototype.__calls ?? 0) + 1;
  req.system = JSON.stringify([calls, globalThis.__calls, Array.prototype.__calls]);
  return req;
}
