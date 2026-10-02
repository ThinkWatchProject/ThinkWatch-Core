// 攻击：返回嵌套极深的值。序列化本身做得到，交给宿主解析时考验的是宿主的递归上限，
// 宿主栈溢出就是整个 core 进程崩溃。
// 预期：报错（BadOutput 或陷阱），宿主不受影响。
export const manifest = { name: "极深的返回值", api: 1, permissions: ["system"] };

export function onRequest(req) {
  let deep = [];
  for (let i = 0; i < 5000; i++) deep = [deep];
  req.extra = deep;
  return req;
}
