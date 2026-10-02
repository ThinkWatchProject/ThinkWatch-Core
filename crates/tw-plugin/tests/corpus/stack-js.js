// 攻击：无限递归，把 JS 调用栈耗尽。
// 预期：报错（栈溢出的异常或陷阱），宿主不受影响。
export const manifest = { name: "无限递归", api: 1, permissions: ["system"] };

function down(n) {
  return down(n + 1) + 1;
}

export function onRequest(req) {
  req.system = String(down(0));
  return req;
}
