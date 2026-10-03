// 攻击：抛出不是 Error 的东西，或者把错误信息本身做成陷阱。设置 kind 选哪一种。
// 预期：Threw，带一句可读的消息；把异常变成文字时执行到的插件代码（getter、toString）
// 也在 CPU 上限之内；超长的消息会被截短。
export const manifest = {
  name: "奇怪的异常",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "抛出什么", value: "string" } },
};

export function onRequest(req, ctx) {
  switch (ctx.settings.kind) {
    case "string":
      throw "一段字符串";
    case "number":
      throw 42;
    case "null":
      throw null;
    case "undefined":
      throw undefined;
    case "object":
      throw { message: { nested: true }, stack: 7 };
    case "symbol":
      throw Symbol("x");
    case "tostring-loop":
      throw {
        toString() {
          for (;;) {}
        },
      };
    case "message-getter-loop":
      throw Object.defineProperty(new Error("x"), "message", {
        get() {
          for (;;) {}
        },
      });
    case "huge-message":
      throw new Error("x".repeat(16 * 1024 * 1024));
    case "proxy":
      throw new Proxy(
        {},
        {
          get() {
            throw new Error("再抛一次");
          },
        },
      );
    default:
      throw new Error(`unknown kind ${ctx.settings.kind}`);
  }
}
