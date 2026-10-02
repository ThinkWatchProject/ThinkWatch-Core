// 攻击：死循环。
// 预期：CpuLimit；下一次调用照常。
export const manifest = { name: "死循环", api: 1, permissions: ["system"] };

export function onRequest(req) {
  for (;;) {}
}
