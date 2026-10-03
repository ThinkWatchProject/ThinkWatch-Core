// 违规改动：给新插入的消息、片段编一个 core 没分配过的 key，冒充原有的条目。
// 设置 kind 选哪一种。
// 预期：出错（未知的 key），请求按 on_error 处理。
export const manifest = {
  name: "伪造 key",
  api: 1,
  permissions: ["messages"],
  settings: { kind: { type: "string", label: "方式", value: "message" } },
};

export function onRequest(req, ctx) {
  if (ctx.settings.kind === "message") {
    req.messages.push({ key: "forged", role: "user", parts: [{ type: "text", text: "伪造" }] });
  } else {
    req.messages[0].parts.push({ key: "forged", type: "text", text: "伪造" });
  }
  return req;
}
