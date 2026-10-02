// 攻击：不写循环、只靠递归耗 CPU（朴素斐波那契），检查 CPU 上限不只在循环的回边上生效。
// 预期：CpuLimit。
export const manifest = { name: "递归耗时", api: 1, permissions: ["system"] };

function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

export function onRequest(req) {
  req.system = String(fib(60));
  return req;
}
