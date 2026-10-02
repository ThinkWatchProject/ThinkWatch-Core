// 统一用词
//
// 把回答里的用词换成统一的写法，例如「登陆」换成「登录」。
//
// 逐段模式：回答照常随流输出。一段文字的末尾恰好是某个原词的开头时（例如收到
// 「请先登」），这几个字先扣住，等下一段到了再判断；整段结束时扣住的文字全部放出。
//
// 权限：reply.text，只改回答里的文字。
// 设置：替换表，每条写成「原词=新词」，条与条之间用逗号或分号隔开。

export const manifest = {
  name: "统一用词",
  api: 1,
  description: "把回答里的用词换成统一的写法。",
  permissions: ["reply.text"],
  reply: "stream",
  settings: {
    terms: {
      type: "string",
      label: "替换表（原词=新词，用逗号分隔）",
      default: "登陆=登录，帐号=账号",
    },
  },
};

// 同一个回答里的几次调用共用一个实例，所以模块里的变量在这个回答里一直有效；
// 回答结束后实例丢弃，下一个回答从头开始
let table = null;
let held = "";

function terms(ctx) {
  if (table === null) {
    table = String(ctx.settings.terms ?? "")
      .split(/[,，;；\n]/)
      .map((entry) => entry.split("="))
      .filter((pair) => pair.length === 2 && pair[0].trim() !== "")
      .map(([from, to]) => [from.trim(), to.trim()]);
  }
  return table;
}

// 从头扫到尾：命中原词就换，剩下的部分可能是某个原词的开头时停下，留到下一段
function convert(text, ctx, last) {
  const list = terms(ctx);
  let out = "";
  let i = 0;
  scan: while (i < text.length) {
    for (const [from, to] of list) {
      if (text.startsWith(from, i)) {
        out += to;
        i += from.length;
        continue scan;
      }
    }
    const rest = text.slice(i);
    if (!last && list.some(([from]) => from.length > rest.length && from.startsWith(rest))) {
      break;
    }
    out += text[i];
    i += 1;
  }
  held = text.slice(i);
  return out;
}

export function onReplyText(text, ctx) {
  return convert(held + text, ctx, false);
}

export function onReplyTextEnd(ctx) {
  return convert(held, ctx, true);
}
