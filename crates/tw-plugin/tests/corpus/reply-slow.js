// 攻击：回答的每一段都慢慢地耗 CPU。设置 kind 选哪一种：
//   over-call  每段 60 毫秒，超过单次 20 毫秒的上限；
//   under-call 每段 8 毫秒，单次不超，但累计会超过整条回答 2 秒的上限。
// 预期：CpuLimit；under-call 那种在累计超出时才出现，之前的调用照常。
export const manifest = {
  name: "慢慢耗时",
  api: 1,
  permissions: ["reply.text"],
  reply: "stream",
  settings: { kind: { type: "string", label: "方式", default: "over-call" } },
};

function burn(ms) {
  const until = Date.now() + ms;
  let spins = 0;
  while (Date.now() < until) spins++;
  return spins;
}

export function onReplyText(text, ctx) {
  burn(ctx.settings.kind === "over-call" ? 60 : 8);
  return text;
}
