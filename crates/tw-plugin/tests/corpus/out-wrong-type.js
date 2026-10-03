// 攻击：钩子返回不该有的类型。设置 kind 选哪一种。
// 预期：BadOutput。Promise 例外：运行时等它落定，再按落定的值核对。
export const manifest = {
  name: "错误的返回类型",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "类型", value: "number" } },
};

export function onRequest(req, ctx) {
  switch (ctx.settings.kind) {
    case "number":
      return 42;
    case "string":
      return "整个请求";
    case "boolean":
      return true;
    case "function":
      return () => req;
    case "symbol":
      return Symbol("x");
    case "bigint":
      return { ...req, n: 10n };
    case "promise":
      return Promise.resolve(req);
    case "array":
      return [req];
    case "null":
      return null;
    default:
      throw new Error(`unknown kind ${ctx.settings.kind}`);
  }
}
