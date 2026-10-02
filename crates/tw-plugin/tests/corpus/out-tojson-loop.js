// 攻击：返回值的 toJSON 是死循环。序列化返回值时才会执行到它。
// 预期：CpuLimit —— 序列化也在 CPU 上限之内。
export const manifest = { name: "toJSON 死循环", api: 1, permissions: ["system"] };

export function onRequest(req) {
  return {
    toJSON() {
      for (;;) {}
    },
  };
}
