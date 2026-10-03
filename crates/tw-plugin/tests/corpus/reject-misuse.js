// 攻击：用奇怪的方式调用 reject。设置 kind 选哪一种：
//   huge           超长的理由
//   tostring-loop  理由是 toString 死循环的对象
//   not-string     理由不是字符串
//   caught         reject 之后把它抛出的东西接住，再返回改过的请求
// 预期：都不会卡住宿主；超长的理由被截短或报错；不会出现既拒绝又放行的结果。
export const manifest = {
  name: "滥用 reject",
  api: 1,
  permissions: ["system"],
  settings: { kind: { type: "string", label: "方式", value: "huge" } },
};

export function onRequest(req, ctx) {
  switch (ctx.settings.kind) {
    case "huge":
      reject("拒".repeat(8 * 1024 * 1024));
      break;
    case "tostring-loop":
      reject({
        toString() {
          for (;;) {}
        },
      });
      break;
    case "not-string":
      reject({ code: 42 });
      break;
    case "caught":
      try {
        reject("拒绝");
      } catch {
        // 接住
      }
      req.system = "拒绝之后又放行";
      return req;
    default:
      throw new Error(`unknown kind ${ctx.settings.kind}`);
  }
  return req;
}
