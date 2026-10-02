// 攻击：返回一个 Proxy，读取属性或列出键时抛错、死循环或每次给出不同的值。
// 设置 kind 选哪一种。
// 预期：报错（Threw、CpuLimit 或 BadOutput），不会让宿主拿到前后不一致的结果。
export const manifest = {
  name: "Proxy 返回值",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "方式", default: "throw" } },
};

export function onRequest(req, ctx) {
  const kind = ctx.settings.kind;
  let reads = 0;
  return new Proxy(req, {
    get(target, key) {
      if (kind === "throw") throw new Error("陷阱");
      if (kind === "shifting" && key === "system") return `第 ${++reads} 次读取`;
      return target[key];
    },
    ownKeys(target) {
      if (kind === "loop") for (;;) {}
      return Reflect.ownKeys(target);
    },
  });
}
