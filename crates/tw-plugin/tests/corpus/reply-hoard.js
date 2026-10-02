// 攻击：逐段模式下扣住全部文字，结束时一次放出成倍放大的内容。
// 预期：OutputLimit（扣住的文字最多 1 MiB）。
export const manifest = {
  name: "扣住再放大",
  api: 1,
  permissions: ["reply.text"],
  reply: "stream",
};

let held = "";

export function onReplyText(text) {
  held += text;
  return "";
}

export function onReplyTextEnd() {
  return held.repeat(65536);
}
