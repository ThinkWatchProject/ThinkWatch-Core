// 探查：插件拿到的回答文字里有什么。把看到的文字编码成码点接在后面（同 see-request.js）。
// 预期：上游回显的密钥，插件只看到占位符；客户端收到的原文里密钥照常还原。
export const manifest = { name: "看回答", api: 1, permissions: ["reply.text"] };

const encode = (s) => Array.from(s, (c) => c.codePointAt(0).toString(16)).join(".");

export function onReplyText(text) {
  return `${text}\nseen:${encode(text)}\n`;
}
