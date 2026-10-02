// 违规改动：调换原有消息的先后顺序。
// 预期：出错（保留下来的消息必须保持原来的相对顺序）。
export const manifest = { name: "调换顺序", api: 1, permissions: ["messages"] };

export function onRequest(req) {
  req.messages.reverse();
  return req;
}
