// 遮住回答里的特定内容
//
// 回答里出现符合格式的内容时，换成一段固定文字。默认遮住内部主机名，也可以改成
// 工单号、员工编号等任何能用正则表达式描述的格式。只改发给客户端的回答。
//
// 整段模式：一段文字到齐之后才处理，符合格式的内容不会被流式输出切成两半而漏掉。
// 代价是这段文字要等到齐了才出现在客户端里。
//
// 权限：reply.text，只改回答里的文字。
// 设置：格式（正则表达式，不区分大小写）和替换文字。

export const manifest = {
  name: "遮住特定内容",
  api: 1,
  description: "把回答里符合格式的内容换成固定文字。",
  permissions: ["reply.text"],
  settings: {
    pattern: {
      type: "string",
      label: "格式（正则表达式）",
      default: "\\b[a-z0-9-]+\\.corp\\.example\\.com\\b",
    },
    replacement: { type: "string", label: "替换为", default: "[内部地址]" },
  },
};

let compiled = null;

function pattern(ctx) {
  if (compiled === null) {
    try {
      compiled = new RegExp(String(ctx.settings.pattern), "gi");
    } catch (e) {
      throw new Error(`设置里的格式不是有效的正则表达式：${e.message}`);
    }
  }
  compiled.lastIndex = 0;
  return compiled;
}

export function onReplyText(text, ctx) {
  const replacement = String(ctx.settings.replacement ?? "");
  // 用函数而不是字符串作替换：替换文字里的 $& 之类原样输出，不当作特殊写法
  return text.replace(pattern(ctx), () => replacement);
}
