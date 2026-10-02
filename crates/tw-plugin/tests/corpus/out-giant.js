// 攻击：返回远大于输入的结果（8 MiB 的系统提示词）。
// 预期：OutputLimit（请求的上限是输入的两倍加 1 MiB）。
export const manifest = { name: "超大输出", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.system = "x".repeat(8 * 1024 * 1024);
  return req;
}
