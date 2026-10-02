// 攻击：manifest 的字段是死循环的 getter，宿主读 manifest 时才会执行到。
// 预期：加载在有限时间内失败。
export const manifest = {
  get name() {
    for (;;) {}
  },
  api: 1,
  permissions: ["system"],
};

export function onRequest(req) {
  return req;
}
