// 附加当前日期
//
// 在系统提示词末尾附上今天的日期。模型本身不知道今天是哪一天，问到截止日期、
// 版本新旧这类问题时，容易按训练数据所在的年份回答。
//
// 权限：system，只读写系统提示词。
// 设置：时区，相对 UTC 的小时数，默认 8（北京时间）。
//
// 日期每天变一次，系统提示词随之变化，上游的提示词缓存每天重建一次；同一天里
// 每一轮附上的内容相同，缓存照常命中。

export const manifest = {
  name: "附加当前日期",
  api: 1,
  description: "在系统提示词末尾附上今天的日期。",
  permissions: ["system"],
  settings: {
    utc_offset: { type: "number", label: "时区（相对 UTC 的小时数）", default: 8 },
  },
};

export function onRequest(req, ctx) {
  const offset = Number(ctx.settings.utc_offset ?? 8);
  const local = new Date(Date.now() + offset * 3600 * 1000);
  const line = `今天的日期：${local.toISOString().slice(0, 10)}`;
  req.system = req.system ? `${req.system}\n\n${line}` : line;
  return req;
}
