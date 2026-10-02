// 违规改动：把一条消息、一个片段原样复制一份，两份带着同一个 key。
// 设置 kind 选哪一种。
// 预期：出错（重复的 key）。
export const manifest = {
  name: "重复 key",
  api: 1,
  permissions: ["messages"],
  settings: { kind: { type: "string", label: "方式", default: "message" } },
};

const copy = (v) => JSON.parse(JSON.stringify(v));

export function onRequest(req, ctx) {
  if (ctx.settings.kind === "message") {
    req.messages.push(copy(req.messages[0]));
  } else {
    req.messages[0].parts.push(copy(req.messages[0].parts[0]));
  }
  return req;
}
