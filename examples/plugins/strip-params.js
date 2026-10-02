// 去掉上游不接受的参数
//
// 有的中转站或模型不接受某些采样参数，例如同时给了 temperature 和 top_p 就报错。
// 这个插件在请求发出之前删掉指定的参数，其余内容原样发出。
//
// 权限：params，读写模型名、max_tokens、temperature、top_p、stop。
// 设置：要删掉的参数，可写多个，用逗号分隔。只认 temperature、top_p、stop 三个。
//
// 请求钩子在路由之前运行，那时还不知道请求会发往哪个上游，所以适用范围只能按
// 客户端和模型收窄：在 Lite 的插件页里，把模型范围设成出问题的上游所用的模型。

export const manifest = {
  name: "去掉不接受的参数",
  api: 1,
  description: "请求发出之前删掉指定的采样参数。",
  permissions: ["params"],
  settings: {
    names: { type: "string", label: "要删掉的参数（用逗号分隔）", default: "top_p" },
  },
};

const REMOVABLE = ["temperature", "top_p", "stop"];

export function onRequest(req, ctx) {
  const names = String(ctx.settings.names ?? "")
    .split(/[,，\s]+/)
    .filter((name) => REMOVABLE.includes(name));
  let changed = false;
  for (const name of names) {
    if (req.params[name] !== undefined) {
      delete req.params[name];
      changed = true;
    }
  }
  // 什么都没删时不返回：请求原样发出，一个字节都不动
  return changed ? req : undefined;
}
