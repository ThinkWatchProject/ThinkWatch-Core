// 攻击：模块顶层就是死循环。加载（安装前的检查、重载配置）时就会执行到。
// 预期：加载在有限时间内失败，不会卡住 core。
for (;;) {}

export const manifest = { name: "顶层死循环", api: 1, permissions: ["system"] };

export function onRequest(req) {
  return req;
}
