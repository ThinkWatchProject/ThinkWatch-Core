// 逐段替换的切分性质：随机文字、随机规则（字符串和正则混着，文字里带占位符和像占位符
// 的字），三样必须一致 ——
//   1. 一个逐个位置去试的参照实现（每个位置按规则的先后试，正则用 y 标志钉在那个位置，
//      替换交给内建的 String.prototype.replace 去算）；
//   2. 整段的 tw.replace；
//   3. 任意切分之后逐段交给 tw.streamReplace，拼起来的结果。
// 另外查两样：逐段交出的每一段都不会把输入里的占位符切开，也不会把一个代理对切开。
//
// 三成的组是「全是字符串的规则 + 几百上千字的长文字 + 切得粗」：规则全是字符串、两两不交叠
// 时 tw.replace 和逐段替换都走 split 的快路径，这几组就是盯着它的。
//
// 在 onRequest 里跑（lane 一直是 0），每种切分是同一个 lane 上的一块文字，块和块之间
// 调 onReplyTextEnd。ctx.settings：seed（随机种子）、cases（几组规则和文字）、splits
// （每组切几种）。结果写进系统提示词：{ checked, failures }。
export const manifest = {
  name: "split property",
  api: 1,
  permissions: ["system", "reply.text"],
  reply: "stream",
};

export const { onReplyText, onReplyTextEnd } = tw.streamReplace([]);

let seed = 1;
function rand() {
  seed ^= seed << 13;
  seed ^= seed >>> 17;
  seed ^= seed << 5;
  return (seed >>> 0) / 4294967296;
}
const pick = (a) => a[Math.floor(rand() * a.length)];
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));

const PIECES = ["a", "b", "c", "ab", "A", "1", "2", " ", "\n", "<", ">", "<<", ">>", "T", "W", "_", "TW_", "<<TW_", "é", "😀", "$"];
const PLACEHOLDERS = ["<<TW_SECRET_1>>", "<<TW_SECRET_12>>", "<<TW_EMAIL_3>>", "<<TW_ID_NUMBER_2>>"];
const LOOKALIKES = ["<<TW_", "<<TW_x>>", "<<TW_A_1>", "<TW_A_1>>", "<<<TW_SECRET_1>>", "<<TW_SECRET_1>>>", "<<TW_SEC"];
const FINDS = ["a", "b", "c", "ab", "A", "1", " ", "\n", "<", ">", "<<", ">>", "T", "W", "_", "TW_", "SECRET", "_1", "é", "😀"];
// 多半两两不交叠的查找字符串（快路径能用）；<< 和两个换行自己和自己交叠
const WORDS = ["cat", "dog", "TW_", "SECRET", "é", "😀", "xyz", "\n\n", "<<", "12", "\ue000"];
const LONG_PIECES = [...PIECES, ...WORDS, "the ", "cat ", "dog ", "xy", "z", "\ue001"];
const TOS = ["", "x", "$&", "$1", "<", "TW_", "<<TW_SECRET_9>>", "😀", "[]"];
// 每一条一次命中连同前后断言看到的范围都不超过 7 个字（maxMatch 至少 8）
const REGEXES = [
  [/a{1,3}/, "[$&]"],
  [/ab|a/, "<$&>"],
  [/[ab]c/, "x"],
  [/(?<=a)b/, "B"],
  [/b(?=c)/, "β"],
  [/(?<!a)c/, "C"],
  [/a(?!b)/, "α"],
  [/^a/m, "^"],
  [/^b/, "^b"],
  [/c$/m, "$$"],
  [/c$/, "$$end"],
  [/\bab/, "w"],
  [/a\b/, "e"],
  [/a.b/s, "."],
  [/(a)(b)?c/, "$2$1"],
  [/(?<x>[ab])c/, "$<x>!"],
  [/A/i, "i"],
  [/\d/, "#"],
  [/T(?=W)/, "t"],
  [/<</, "«"],
  [/>>/, "»"],
  [/_1>/, "!"],
  [/😀/u, ":)"],
  [/[^\n]{2}\n/, "¶"],
  [/W_[A-Z]/, "w"],
  [/SECRET/, "s"],
  [/\d{1,2}>>/, "?"],
  [/./, "·"],
];
// 函数替换：一个看命中的字，一个看位置（按两个占位符之间那一节算）
const FUNCTIONS = [
  (m) => "{" + m + "}",
  (...a) => "@" + a[a.length - (typeof a[a.length - 1] === "object" ? 3 : 2)],
];

function text(long) {
  let s = "";
  const n = long ? int(100, 700) : int(0, 30);
  for (let i = 0; i < n; i++) {
    const r = rand();
    if (long) s += r < 0.01 ? pick(PLACEHOLDERS) : r < 0.02 ? pick(LOOKALIKES) : pick(LONG_PIECES);
    else s += r < 0.08 ? pick(PLACEHOLDERS) : r < 0.13 ? pick(LOOKALIKES) : pick(PIECES);
  }
  return s;
}

// 全是「字符串 → 字符串」的规则：多半取自 WORDS（不交叠），偶尔混一条会交叠的
function literalRules() {
  const out = [];
  const n = int(1, 5);
  for (let i = 0; i < n; i++) {
    const f = rand() < 0.85 ? pick(WORDS) : pick(FINDS) + pick(FINDS);
    out.push([f, pick(TOS)]);
  }
  return out;
}

function rules() {
  const out = [];
  const n = int(0, 4);
  for (let i = 0; i < n; i++) {
    const fn = rand() < 0.15 ? pick(FUNCTIONS) : null;
    if (rand() < 0.5) {
      let f = "";
      const k = int(1, 3);
      for (let j = 0; j < k; j++) f += pick(FINDS);
      out.push([f, fn ?? pick(TOS)]);
    } else {
      const [re, to] = pick(REGEXES);
      out.push([re, fn ?? to]);
    }
  }
  return out;
}

const PH = /<<TW_[A-Z0-9_]{1,64}>>/g;

// 参照实现：两个占位符之间每一节单独替换，占位符原样，记下它们在结果里的位置
function reference(s, rs) {
  let out = "";
  const spans = [];
  let from = 0;
  let m;
  PH.lastIndex = 0;
  while ((m = PH.exec(s)) !== null) {
    out += section(s.slice(from, m.index), rs);
    spans.push([out.length, out.length + m[0].length]);
    out += m[0];
    from = PH.lastIndex;
  }
  return { out: out + section(s.slice(from), rs), spans };
}

function section(t, rs) {
  let out = "";
  let i = 0;
  outer: while (i < t.length) {
    for (const [find, to] of rs) {
      if (typeof find === "string") {
        if (t.startsWith(find, i)) {
          out += typeof to === "function" ? String(to(find, i, t)) : to;
          i += find.length;
          continue outer;
        }
        continue;
      }
      const re = new RegExp(find.source, find.flags + "y");
      re.lastIndex = i;
      const m = re.exec(t);
      if (m === null) continue;
      const end = i + m[0].length;
      re.lastIndex = i;
      // 不带 g：只换钉在 i 的这一处，$ 写法和函数参数都交给内建的去算
      const whole = t.replace(re, to);
      out += whole.slice(i, whole.length - (t.length - end));
      i = end;
      continue outer;
    }
    out += t[i];
    i += 1;
  }
  return out;
}

// 随机切开，不切在代理对中间（宿主交来的每一段都是完整的 UTF-8）。coarse：切得粗（块中间
// 那几段也够长，走得到快路径）
function split(s, coarse) {
  const mode = rand();
  const p = coarse ? 0.004 : 0.35;
  const chunks = [];
  let start = 0;
  for (let i = 1; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c >= 0xdc00 && c <= 0xdfff) continue;
    if ((!coarse && mode < 0.2) || rand() < p) {
      chunks.push(s.slice(start, i));
      start = i;
    }
  }
  chunks.push(s.slice(start));
  // 偶尔夹一段空的
  if (rand() < 0.1) chunks.splice(int(0, chunks.length), 0, "");
  return chunks;
}

function show(rs) {
  return rs.map(([f, t]) => [typeof f === "string" ? f : String(f), typeof t === "function" ? "fn:" + FUNCTIONS.indexOf(t) : t]);
}

export function onRequest(req, ctx) {
  seed = ctx.settings.seed >>> 0 || 1;
  const failures = [];
  let checked = 0;
  for (let k = 0; k < ctx.settings.cases && failures.length < 5; k++) {
    const long = rand() < 0.3;
    const rs = long ? literalRules() : rules();
    const max = int(8, 16);
    const input = text(long);
    const ref = reference(input, rs);
    const finds = rs.map(([f]) => f);
    const asMap = rand() < 0.2 && new Set(finds).size === finds.length;
    const whole = tw.replace(input, asMap ? new Map(rs) : rs);
    if (whole !== ref.out) {
      failures.push({ what: "whole", input, rules: show(rs), want: ref.out, got: whole });
      continue;
    }
    const h = tw.streamReplace(rand() < 0.3 ? () => rs : rs, { maxMatch: max });
    for (let n = 0; n < ctx.settings.splits; n++) {
      const chunks = split(input, long && rand() < 0.7);
      const outs = chunks.map((c) => h.onReplyText(c, ctx));
      outs.push(h.onReplyTextEnd(ctx));
      const got = outs.join("");
      checked++;
      if (got !== ref.out) {
        failures.push({ what: "stream", input, chunks, max, rules: show(rs), want: ref.out, got, outs });
        break;
      }
      let at = 0;
      for (const o of outs) {
        at += o.length;
        const c = got.charCodeAt(at);
        if (ref.spans.some(([a, b]) => a < at && at < b) || (c >= 0xdc00 && c <= 0xdfff && at > 0)) {
          failures.push({ what: "cut", input, chunks, max, rules: show(rs), outs, at });
          break;
        }
      }
    }
  }
  req.system = JSON.stringify({ checked, failures });
}
