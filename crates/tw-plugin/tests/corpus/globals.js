// 探查：沙箱里有哪些全局名字。把 globalThis 上的全部键（含不可枚举的、符号键）
// 写进系统提示词，由测试和允许的清单比对。
// 预期：只有 ECMAScript 标准内置、console 和 reject。
export const manifest = { name: "列出全局", api: 1, permissions: ["system"] };

export function onRequest(req) {
  const names = new Set();
  for (let o = globalThis; o !== null; o = Object.getPrototypeOf(o)) {
    if (o === Object.prototype) break;
    for (const key of Reflect.ownKeys(o)) names.add(String(key));
  }
  req.system = JSON.stringify([...names].sort());
  return req;
}
