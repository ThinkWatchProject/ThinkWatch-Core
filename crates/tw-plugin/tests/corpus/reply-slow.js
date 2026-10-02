// 攻击：回答的每一段都耗 CPU。设置 kind 选哪一种：
//   over-call  每段都是死循环，单次的上限要把它停下；
//   under-call 每段做一份固定的计算（快的机器上十毫秒上下），单次不超，但累计会超过
//              整条回答的上限。
// 耗时按计算量来，不按墙钟忙等：机器忙的时候线程被抢占，墙钟走了 CPU 时间却没走，
// 按墙钟忙等的插件就测不到 CPU 上限了。
// 预期：CpuLimit；under-call 那种在累计超出时才出现，之前的调用照常。
export const manifest = {
  name: "慢慢耗时",
  api: 1,
  permissions: ["reply.text"],
  reply: "stream",
  settings: { kind: { type: "string", label: "方式", default: "over-call" } },
};

function work(n) {
  let x = 0;
  for (let i = 0; i < n; i++) x = (x + i * 7) % 1000003;
  return x;
}

export function onReplyText(text, ctx) {
  if (ctx.settings.kind === "over-call") {
    for (;;) {}
  }
  work(300000);
  return text;
}
