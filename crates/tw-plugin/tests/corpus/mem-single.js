// 攻击：一次申请一大块内存（1 GiB 的 ArrayBuffer、两亿个元素的数组）。
// 设置 kind 选哪一种。
// 预期：报错（MemoryLimit，或引擎直接拒绝分配而抛出的异常），不会真的分配出来。
export const manifest = {
  name: "一次申请大块内存",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "方式", default: "arraybuffer" } },
};

export function onRequest(req, ctx) {
  let held;
  switch (ctx.settings.kind) {
    case "arraybuffer":
      held = new Uint8Array(new ArrayBuffer(1024 * 1024 * 1024));
      held[held.length - 1] = 1;
      break;
    case "array":
      held = new Array(200 * 1000 * 1000).fill(1);
      break;
    case "string":
      held = "x".repeat(1024 * 1024 * 1024);
      break;
    default:
      throw new Error(`unknown kind ${ctx.settings.kind}`);
  }
  req.system = String(held.length);
  return req;
}
