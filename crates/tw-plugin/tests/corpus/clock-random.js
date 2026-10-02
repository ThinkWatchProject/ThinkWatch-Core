// 时钟和随机数要是真的：快照里冻住的时间或随机数种子，会让每个实例看到同一个
// 「现在」、同一串「随机数」。
// 预期：Date.now() 接近宿主的当前时间；两次调用的随机数不同。
export const manifest = { name: "时钟与随机数", api: 1, permissions: ["system"] };

const loadedAt = Date.now();

export function onRequest(req) {
  req.system = JSON.stringify({ now: Date.now(), loadedAt, random: [Math.random(), Math.random()] });
  return req;
}
