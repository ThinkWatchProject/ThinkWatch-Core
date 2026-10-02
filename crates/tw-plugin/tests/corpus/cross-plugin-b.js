// 和 cross-plugin-a.js 成对：读 A 留下的标记。
// 预期：读不到，两项都是 undefined。
export const manifest = { name: "插件 B", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.system = JSON.stringify([typeof globalThis.leftByA, typeof {}.leftByA]);
  return req;
}
