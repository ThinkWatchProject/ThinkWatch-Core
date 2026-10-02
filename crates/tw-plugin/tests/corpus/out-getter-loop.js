// 攻击：返回值的 getter 是死循环，读取 system 时才会执行到它。
// 预期：CpuLimit。
export const manifest = { name: "getter 死循环", api: 1, permissions: ["system"] };

export function onRequest(req) {
  return {
    format: req.format,
    model: req.model,
    get system() {
      for (;;) {}
    },
  };
}
