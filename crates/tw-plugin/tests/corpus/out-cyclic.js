// 攻击：返回自引用的对象。
// 预期：BadOutput（无法序列化），不会卡住。
export const manifest = { name: "自引用", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.self = req;
  return req;
}
