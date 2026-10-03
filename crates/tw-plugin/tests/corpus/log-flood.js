// 攻击：日志洪水 —— 很多行、超长的一行、打印时死循环的对象、自引用的对象。
// 设置 kind 选哪一种。
// 预期：日志行数和每行长度都在上限之内（超出的报错或截断），不会卡住。
export const manifest = {
  name: "日志洪水",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "方式", value: "lines" } },
};

export function onRequest(req, ctx) {
  switch (ctx.settings.kind) {
    case "lines":
      for (let i = 0; i < 100000; i++) console.log(`第 ${i} 行`);
      break;
    case "long-line":
      console.error("y".repeat(8 * 1024 * 1024));
      break;
    case "getter-loop":
      console.warn({
        get x() {
          for (;;) {}
        },
      });
      break;
    case "cyclic": {
      const o = { name: "环" };
      o.self = o;
      console.info(o);
      break;
    }
    default:
      throw new Error(`unknown kind ${ctx.settings.kind}`);
  }
  return undefined;
}
