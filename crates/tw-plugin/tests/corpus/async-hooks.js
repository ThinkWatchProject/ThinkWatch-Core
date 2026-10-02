// 攻击：钩子是 async 函数，或者留下一个死循环的微任务，指望它在钩子返回之后、
// CPU 计时之外执行。设置 kind 选哪一种。
// 预期：async 钩子的 Promise 在同一次调用里等到落定，再按返回值核对（或者算坏输出），
// 不会把 Promise 本身当成请求；微任务要么不执行，要么在上限之内被中止。
export const manifest = {
  name: "异步钩子",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "方式", default: "async" } },
};

export function onRequest(req, ctx) {
  if (ctx.settings.kind === "async") {
    return (async () => req)();
  }
  if (ctx.settings.kind === "microtask") {
    Promise.resolve().then(() => {
      for (;;) {}
    });
    return undefined;
  }
  throw new Error(`unknown kind ${ctx.settings.kind}`);
}
