// 附加当前日期
//
// 在系统提示词末尾附上今天的日期。模型本身不知道今天是哪一天，问到截止日期、版本新旧
// 这类问题时，容易按训练数据所在的年份回答。
//
// 沙箱里的时钟是 UTC，所以「今天」按设置里的时区算，默认 UTC+8（北京时间）。
//
// 日期每天变一次，系统提示词随之变化，上游的提示词缓存每天重建一次；同一天里每一轮
// 附上的内容相同，缓存照常命中。
//
// 权限：system，只读写系统提示词。
// 设置：时区，相对 UTC 的小时数，-12 到 14，可以带小数（例如 5.5）。

export const manifest = {
  name: "附加当前日期",
  api: 1,
  description: "在系统提示词末尾附上今天的日期，按设置的时区计算。",
  permissions: ["system"],
  settings: {
    utc_offset: { type: "number", label: "时区（相对 UTC 的小时数）", default: 8 },
  },
};

export function onRequest(req, ctx) {
  const offset = Number(ctx.settings.utc_offset ?? 8);
  if (!Number.isFinite(offset) || offset < -12 || offset > 14) {
    throw new Error(`设置「时区」要在 -12 到 14 之间，当前是 ${ctx.settings.utc_offset}`);
  }
  const date = new Date(Date.now() + offset * 3600 * 1000).toISOString().slice(0, 10);
  const zone = `UTC${offset < 0 ? "-" : "+"}${Math.abs(offset)}`;
  const line = `Today's date: ${date} (${zone}).`;
  req.system = req.system ? `${req.system}\n\n${line}` : line;
  return req;
}
