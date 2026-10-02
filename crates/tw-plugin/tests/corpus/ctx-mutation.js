// 攻击：改 ctx（赋值、删除、定义属性、换原型、改 settings 里的值）。
// 每一种尝试记下是否得手，写进系统提示词。
// 预期：一种都不得手；ctx 是冻结的，settings 也是。
export const manifest = {
  name: "改 ctx",
  api: 1,
  permissions: ["system"],
  settings: { note: { type: "string", label: "备注", default: "原值" } },
};

function attempt(f, check) {
  try {
    f();
  } catch {
    return false;
  }
  return check();
}

export function onRequest(req, ctx) {
  const results = {
    frozen: Object.isFrozen(ctx),
    settingsFrozen: Object.isFrozen(ctx.settings),
    assignModel: attempt(() => { ctx.model = "改过"; }, () => ctx.model === "改过"),
    assignUpstream: attempt(() => { ctx.upstream = "evil"; }, () => ctx.upstream === "evil"),
    deleteClient: attempt(() => { delete ctx.client; }, () => !("client" in ctx)),
    addField: attempt(() => { ctx.extra = 1; }, () => ctx.extra === 1),
    defineProperty: attempt(
      () => Object.defineProperty(ctx, "format", { value: "openai_chat" }),
      () => ctx.format === "openai_chat",
    ),
    setPrototype: attempt(
      () => Object.setPrototypeOf(ctx, { injected: true }),
      () => ctx.injected === true,
    ),
    settingsValue: attempt(() => { ctx.settings.note = "改过"; }, () => ctx.settings.note === "改过"),
    settingsAdd: attempt(() => { ctx.settings.added = 1; }, () => ctx.settings.added === 1),
  };
  req.system = JSON.stringify(results);
  return req;
}
