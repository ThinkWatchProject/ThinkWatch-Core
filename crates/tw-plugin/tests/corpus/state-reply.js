// 同一个回答里的几次调用共用一个实例，换一个回答就是新实例。
// 每次调用返回这个实例里的第几次调用。
// 预期：同一个回答里 1、2、3……递增；下一个回答又从 1 开始。
export const manifest = {
  name: "回答内的状态",
  api: 1,
  permissions: ["reply.text"],
  reply: "stream",
};

let calls = 0;

export function onReplyText() {
  calls += 1;
  return String(calls);
}
