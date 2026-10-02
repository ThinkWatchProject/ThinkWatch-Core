// 攻击：插件往请求里写一个像密钥的字符串（编出来的 Anthropic 密钥）。
// 预期：插件之后的出站脱敏照样认得它：拦截档下上游收到的是占位符，观察档下原样发出并记下。
export const manifest = { name: "写入密钥", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.system = `${req.system}\n备用密钥：sk-ant-api03-PLUGINWROTEITAAAAAAAAAAAAA`;
  return req;
}
