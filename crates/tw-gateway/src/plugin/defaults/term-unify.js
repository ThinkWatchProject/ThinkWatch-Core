// 统一用词
//
// 把回答里的用词换成统一的写法，例如「登陆」换成「登录」。
//
// 逐段模式：回答照常随流输出。一段文字的末尾恰好是某个原词的开头时（例如收到「请先登」），
// 这几个字先扣住，等下一段到了再判断；这段文字结束时扣住的全部放出。原词有重叠时，
// 长的优先。
//
// 权限：reply.text，只改回答里的文字。
// 设置：替换表，每行一条「原词=新词」。最多 100 条，每个词最多 64 个字符；空行忽略。

export const manifest = {
  name: "统一用词",
  api: 1,
  description: "把回答里的用词换成统一的写法，替换表在设置里每行写一条「原词=新词」。",
  permissions: ["reply.text"],
  reply: "stream",
  settings: {
    terms: {
      type: "string",
      label: "替换表（每行一条：原词=新词）",
      default: "登陆=登录\n帐号=账号",
    },
  },
};

const MAX_TERMS = 100;
const MAX_LENGTH = 64;

// 同一个回答里的几次调用共用一个实例，所以模块里的变量在这个回答里一直有效；
// 回答结束后实例丢弃，下一个回答从头开始
let table = null;
let held = "";

function terms(ctx) {
  if (table !== null) return table;
  const list = [];
  for (const line of String(ctx.settings.terms ?? "").split(/\r?\n/)) {
    if (line.trim() === "") continue;
    const at = line.indexOf("=");
    const from = at < 0 ? "" : line.slice(0, at).trim();
    const to = at < 0 ? "" : line.slice(at + 1).trim();
    if (from === "") {
      throw new Error(`替换表里的「${line.trim()}」不是「原词=新词」的写法`);
    }
    if (from.length > MAX_LENGTH || to.length > MAX_LENGTH) {
      throw new Error(`替换表里的词最多 ${MAX_LENGTH} 个字符`);
    }
    list.push([from, to]);
  }
  if (list.length > MAX_TERMS) {
    throw new Error(`替换表最多 ${MAX_TERMS} 条`);
  }
  // 长的原词优先：「登陆页」和「登陆」都在表里时，先认「登陆页」
  list.sort((a, b) => b[0].length - a[0].length);
  table = list;
  return table;
}

// 从头扫到尾。剩下的部分还可能是某个（更长的）原词的开头时先停下，留到下一段再判断 ——
// 「登陆」已经对上、而「登陆页」还差一个字时也要等；否则命中原词就换，长的优先
function convert(text, ctx, last) {
  const list = terms(ctx);
  let out = "";
  let i = 0;
  scan: while (i < text.length) {
    const rest = text.slice(i);
    if (!last && list.some(([from]) => from.length > rest.length && from.startsWith(rest))) {
      break;
    }
    for (const [from, to] of list) {
      if (text.startsWith(from, i)) {
        out += to;
        i += from.length;
        continue scan;
      }
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
