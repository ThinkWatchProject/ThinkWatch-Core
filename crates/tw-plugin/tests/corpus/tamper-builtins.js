// 攻击：在模块顶层和钩子里改掉宿主可能会用到的内置函数（JSON.stringify、JSON.parse、
// Object.prototype.toJSON、Array.prototype.map），指望宿主读出一个伪造的结果。
// 预期：要么报错，要么宿主拿到的仍然是插件真正返回的值；下一次调用、别的插件都不受影响。
export const manifest = { name: "篡改内置函数", api: 1, permissions: ["system"] };

const forged = '{"format":"anthropic","model":"m","system":"伪造的结果","messages":[]}';
JSON.stringify = () => forged;
JSON.parse = () => ({ system: "伪造的输入" });

export function onRequest(req) {
  Object.prototype.toJSON = function () {
    return { system: "伪造的 toJSON" };
  };
  Array.prototype.map = () => [];
  req.system = "插件真正返回的值";
  return req;
}
