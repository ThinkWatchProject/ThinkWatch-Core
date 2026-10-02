// 攻击：在回答钩子里调用 reject。reject 只在 onRequest 里有效。
// 预期：这次调用出错，按 on_error 处理；回答不会被当成「请求被拒绝」。
export const manifest = { name: "回答里 reject", api: 1, permissions: ["reply.text"] };

export function onReplyText(text) {
  reject("在回答里拒绝");
  return text;
}
