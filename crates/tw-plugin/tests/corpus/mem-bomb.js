// 攻击：内存炸弹，不停地分配并留住。设置 kind 选哪一种：
//   buffers  每次 8 MiB 的 Uint8Array，很快就到上限；
//   strings  每次把字符串翻倍，可能先撞上引擎自己的字符串长度上限或 CPU 上限。
// 预期：buffers 是 MemoryLimit；strings 被三道上限之一拦下。宿主不受影响，下一次调用照常。
export const manifest = {
  name: "内存炸弹",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "方式", default: "buffers" } },
};

export function onRequest(req, ctx) {
  const hoard = [];
  if (ctx.settings.kind === "buffers") {
    for (;;) hoard.push(new Uint8Array(8 << 20));
  }
  let s = "x";
  for (;;) {
    s = s + s;
    hoard.push(s);
  }
}
