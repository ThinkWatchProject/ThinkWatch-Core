// 攻击：模块顶层就是内存炸弹。
// 预期：加载失败，宿主的内存不受影响。
const hoard = [];
for (;;) hoard.push("x".repeat(1 << 20) + hoard.length);

export const manifest = { name: "顶层内存炸弹", api: 1, permissions: ["system"] };

export function onRequest(req) {
  return req;
}
