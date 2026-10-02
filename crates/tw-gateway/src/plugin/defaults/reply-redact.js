// 回答内容打码
//
// 回答里出现符合格式的内容时，换成一段固定文字，例如内部主机名、工单号、员工编号。
// 只改发给客户端的回答，不改请求。出厂没有任何格式，要在设置里写上才会生效。
//
// 整段模式：一段文字到齐之后才处理，符合格式的内容不会被流式输出切成两半而漏掉。
// 代价是这段文字要等到齐了才出现在客户端里。
//
// 权限：reply.text，只改回答里的文字。
// 设置：格式，每行一个正则表达式（最多 50 个）；替换文字（最多 100 个字符）；是否区分大小写。

export const manifest = {
  name: "回答内容打码",
  api: 1,
  description: "把回答里符合格式的内容换成固定文字。格式在设置里每行写一个正则表达式。",
  permissions: ["reply.text"],
  settings: {
    patterns: { type: "string", label: "格式（每行一个正则表达式）", default: "" },
    replacement: { type: "string", label: "替换为", default: "[已隐藏]" },
    ignore_case: { type: "boolean", label: "不区分大小写", default: true },
  },
};

const MAX_PATTERNS = 50;
const MAX_REPLACEMENT = 100;

let compiled = null;

function patterns(ctx) {
  if (compiled !== null) return compiled;
  const flags = ctx.settings.ignore_case === false ? "gu" : "giu";
  const list = [];
  const lines = String(ctx.settings.patterns ?? "").split(/\r?\n/);
  lines.forEach((line, n) => {
    if (line.trim() === "") return;
    try {
      list.push(new RegExp(line.trim(), flags));
    } catch (e) {
      throw new Error(`格式第 ${n + 1} 行不是有效的正则表达式：${e.message}`);
    }
  });
  if (list.length > MAX_PATTERNS) {
    throw new Error(`格式最多 ${MAX_PATTERNS} 行`);
  }
  compiled = list;
  return compiled;
}

export function onReplyText(text, ctx) {
  const list = patterns(ctx);
  if (list.length === 0) return undefined;
  const replacement = String(ctx.settings.replacement ?? "");
  if (replacement.length > MAX_REPLACEMENT) {
    throw new Error(`替换文字最多 ${MAX_REPLACEMENT} 个字符`);
  }
  let out = text;
  for (const re of list) {
    // 用函数而不是字符串作替换：替换文字里的 $& 之类原样输出。匹配到空串的不算命中，
    // 否则「a*」这样的格式会在每个字之间插一遍替换文字
    out = out.replace(re, (m) => (m === "" ? "" : replacement));
  }
  return out === text ? undefined : out;
}
