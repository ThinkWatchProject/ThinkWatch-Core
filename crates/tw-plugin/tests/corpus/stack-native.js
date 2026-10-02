// 攻击：让引擎自己的 C 代码深度递归（极深的 JSON、极深的对象再序列化），
// 耗尽的是 WebAssembly 的栈而不是 JS 的调用栈。设置 kind 选哪一种。
// 预期：报错（异常或陷阱），宿主不受影响。
export const manifest = {
  name: "引擎内部深递归",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "方式", default: "parse" } },
};

const DEPTH = 1000000;

export function onRequest(req, ctx) {
  switch (ctx.settings.kind) {
    case "parse":
      req.system = String(JSON.parse("[".repeat(DEPTH) + "]".repeat(DEPTH)).length);
      break;
    case "stringify": {
      let o = {};
      for (let i = 0; i < DEPTH; i++) o = { o };
      req.system = JSON.stringify(o);
      break;
    }
    default:
      throw new Error(`unknown kind ${ctx.settings.kind}`);
  }
  return req;
}
