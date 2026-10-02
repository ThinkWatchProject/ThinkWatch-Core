// 攻击：在模块顶层调用 reject（加载时执行）。
// 预期：加载失败，或者 reject 在顶层什么都不做；不会让之后的请求被拒绝。
reject("加载时拒绝");

export const manifest = { name: "顶层 reject", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.system = "照常运行";
  return req;
}
