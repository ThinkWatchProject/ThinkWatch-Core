// 攻击：灾难性回溯的正则。时间花在引擎内置的正则实现里，不在插件的 JS 代码里。
// 预期：CpuLimit。
export const manifest = { name: "正则回溯", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.system = String(/^(a+)+$/.test("a".repeat(48) + "!"));
  return req;
}
