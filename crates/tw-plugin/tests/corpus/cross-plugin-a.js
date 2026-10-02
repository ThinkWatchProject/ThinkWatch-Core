// 和 cross-plugin-b.js 成对：A 在全局、内置原型上留下标记，B 读不到才算隔离。
export const manifest = { name: "插件 A", api: 1, permissions: ["system"] };

globalThis.leftByA = "A 留下的";
Object.prototype.leftByA = "A 留在原型上的";

export function onRequest(req) {
  globalThis.leftByA = "A 在钩子里留下的";
  req.system = "A";
  return req;
}
