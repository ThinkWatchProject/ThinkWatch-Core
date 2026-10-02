// 探查：插件拿到的请求里有什么。把看到的全部内容（ctx、视图里的每一节）编码成
// 一串码点写进系统提示词。编码过的内容不会被换回真值，也认不出是密钥，测试解码
// 之后就是插件真正看到的东西。
// 预期：用户粘进对话的密钥，插件只看到占位符；视图里只有授权的几节。
export const manifest = { name: "看请求", api: 1, permissions: ["system", "messages"] };

const encode = (s) => Array.from(s, (c) => c.codePointAt(0).toString(16)).join(".");

export function onRequest(req, ctx) {
  req.system = `seen:${encode(JSON.stringify({ keys: Object.keys(req).sort(), req, ctx }))}`;
  return req;
}
