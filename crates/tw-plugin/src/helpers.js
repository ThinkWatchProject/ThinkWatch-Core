// 插件辅助函数：全局的 `tw`。build.rs 把它拼在桥脚本（bridge.js）前面、一起求值一次，
// 连同整个 QuickJS 做进快照 —— 每个实例起来时它已经在了，不多花一点时间。
//
// 它省掉的是常见改写里的样板：附加系统提示词、逐段改文字、改工具参数、按规则替换（整段
// 和逐段两种，同一套语义）、拆多行设置。**它不是安全边界**：tw 和插件在同一个领域里，
// 插件能改它、绕开它；它交出去的结果和插件手写的一样，宿主照常解析、按权限核对。
//
// 所以这里的纪律和桥一样，只为一件事：插件改了原型、改了内建函数，tw 也不会出莫名其妙
// 的错。用到的内建函数在插件运行之前拿住，之后不再从全局或原型上取；往数组里追加用没有
// 原型的数组（Array.prototype 上挂的 setter 拦不住），属性描述符也没有原型
// （Object.prototype 上挂一个 get 改不了它的意思）。抛给插件的错误一律英文，和桥一致。
//
// 和桥之间只有一条私下的路：求值完在全局上留一个 `__tw_helpers`，桥在插件之前拿走它，
// 再连同别的多余全局一起删掉。桥用它告诉这里两件插件代码看不到的事：
//   - 每次调钩子之前：这是这次回答里的第几块文字（lane）。同时进行的几块文字（比如
//     Chat Completions 的 n > 1）由 tw.streamReplace 分开缓冲；
//   - 模块求值完之后：manifest 是不是逐段模式、有没有导出 onReplyTextEnd。
"use strict";
(function () {
  const G = globalThis;

  const ObjectDefineProperty = Object.defineProperty;
  const ObjectFreeze = Object.freeze;
  const ObjectKeys = Object.keys;
  const ObjectSetPrototypeOf = Object.setPrototypeOf;
  const ObjectGetPrototypeOf = Object.getPrototypeOf;
  const ObjectGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
  const ArrayIsArray = Array.isArray;
  const ArrayPrototype = Array.prototype;
  const ArrayJoin = Array.prototype.join;
  const ReflectApply = Reflect.apply;
  const FunctionCall = Function.prototype.call;
  const FunctionBind = Function.prototype.bind;
  const NumberIsInteger = Number.isInteger;
  const MathMin = Math.min;
  const MathMax = Math.max;
  const StringCtor = String;
  const StringFromCharCode = String.fromCharCode;
  const StringIndexOf = String.prototype.indexOf;
  const StringSlice = String.prototype.slice;
  const StringTrim = String.prototype.trim;
  const StringCharCodeAt = String.prototype.charCodeAt;
  const StringSplit = String.prototype.split;
  const RegExpCtor = RegExp;
  const RegExpPrototype = RegExp.prototype;
  const RegExpExec = RegExp.prototype.exec;
  const MapCtor = Map;
  const MapGet = Map.prototype.get;
  const MapSet = Map.prototype.set;
  const MapHas = Map.prototype.has;
  const MapDelete = Map.prototype.delete;
  const MapEntries = Map.prototype.entries;
  const MapIteratorNext = ObjectGetPrototypeOf(new Map().entries()).next;
  const SetCtor = Set;
  const SetAdd = Set.prototype.add;
  const SetHas = Set.prototype.has;
  const SymbolIterator = Symbol.iterator;
  const ErrorCtor = Error;
  const TypeErrorCtor = TypeError;

  // 正则的来源和标志直接从内部槽读（原型上的 getter）：实例上另挂一个 source、flags
  // 改不了这里读到的
  function getter(name) {
    const d = ObjectGetOwnPropertyDescriptor(RegExpPrototype, name);
    return d === undefined ? undefined : d.get;
  }
  const RegExpSource = getter("source");
  const FLAGS = [
    ["i", getter("ignoreCase")],
    ["m", getter("multiline")],
    ["s", getter("dotAll")],
    ["u", getter("unicode")],
    ["v", getter("unicodeSets")],
  ];
  const RegExpSticky = getter("sticky");

  // 桥告诉这里的（见开头）
  let lane = 0;
  let stream = false;
  let ends = false;

  // ── 小工具 ───────────────────────────────────────────────────

  // 内建方法绑成普通函数：slice(s, a, b) 就是 s.slice(a, b)，绑的是上面拿住的那个，不经原型。
  // 比每次 Reflect.apply 再包一层快得多 —— 替换时每处命中都要调好几次
  const unbind = (f) => ReflectApply(FunctionBind, FunctionCall, [f]);
  const slice = unbind(StringSlice);
  const indexOf = unbind(StringIndexOf);
  const code = unbind(StringCharCodeAt);
  const trim = unbind(StringTrim);
  // 分隔符是字符串时 QuickJS 不去找 Symbol.split，插件改不了它
  const split = unbind(StringSplit);
  const joinWith = unbind(ArrayJoin);
  const execAt = unbind(RegExpExec);
  const mapGet = unbind(MapGet);
  const mapSet = unbind(MapSet);
  const mapDelete = unbind(MapDelete);
  const setAdd = unbind(SetAdd);
  const setHas = unbind(SetHas);

  function exec(re, s, from) {
    re.lastIndex = from;
    return execAt(re, s);
  }

  // 拼字、攒结果用的数组：没有原型
  function list() {
    const a = [];
    ObjectSetPrototypeOf(a, null);
    return a;
  }

  // 要交给插件的那些换回普通数组
  function array(a) {
    ObjectSetPrototypeOf(a, ArrayPrototype);
    return a;
  }

  function join(a) {
    return joinWith(a, "");
  }

  // 建一个普通的自有属性。`__proto__` 这样的键也是一个普通的键，不会去改原型
  function define(o, k, v) {
    ObjectDefineProperty(o, k, { __proto__: null, value: v, writable: true, enumerable: true, configurable: true });
  }

  function typeName(v) {
    if (v === null) return "null";
    if (v === undefined) return "undefined";
    if (ArrayIsArray(v)) return "an array";
    return typeof v === "object" ? "an object" : "a " + typeof v;
  }

  function isHigh(s, i) {
    const c = code(s, i);
    return c >= 0xd800 && c <= 0xdbff;
  }

  function isLow(s, i) {
    const c = code(s, i);
    return c >= 0xdc00 && c <= 0xdfff;
  }

  function isMap(v) {
    try {
      ReflectApply(MapHas, v, [undefined]);
      return true;
    } catch (_) {
      return false;
    }
  }

  function isRegExp(v) {
    if (v === null || typeof v !== "object" || v === RegExpPrototype) return false;
    try {
      ReflectApply(RegExpSource, v, []);
      return true;
    } catch (_) {
      return false;
    }
  }

  function needRequest(req, who) {
    if (req === null || typeof req !== "object") {
      throw new TypeErrorCtor(who + " expects the request object, not " + typeName(req));
    }
  }

  function needFunction(fn, who) {
    if (typeof fn !== "function") {
      throw new TypeErrorCtor(who + " expects a function, not " + typeName(fn));
    }
  }

  function needString(s, who, what) {
    if (typeof s !== "string") {
      throw new TypeErrorCtor(who + ": " + what + " must be a string, not " + typeName(s));
    }
  }

  // 视图里的一节没给（没申请这项权限，宿主就不给这一节）
  function messagesOf(req, who) {
    const ms = req.messages;
    if (ms === undefined) {
      throw new TypeErrorCtor(who + ' needs req.messages: add the "messages" permission');
    }
    if (!ArrayIsArray(ms)) throw new TypeErrorCtor(who + ": req.messages is " + typeName(ms) + ", not an array");
    return ms;
  }

  function partsOf(m, i, who) {
    const ps = m !== null && typeof m === "object" ? m.parts : undefined;
    if (!ArrayIsArray(ps)) throw new TypeErrorCtor(who + ": req.messages[" + i + "] has no parts array");
    return ps;
  }

  // ── 占位符 ───────────────────────────────────────────────────
  //
  // 插件看到的密钥是占位符（`<<TW_SECRET_1>>`、`<<TW_EMAIL_2>>`……，写法见 tw-guard 的
  // `Scheme::SECRET` 和规则的标签：`TW_` 加大写字母、数字、下划线，再是号码）。替换整个
  // 跳过它们：不匹配、不拆开，一处命中也不能跨过一个占位符 —— 两个占位符之间的每一节当作
  // 一段单独的文字。这里认的比宿主宽：宁可多护住一段像占位符的字，也不能把真的拆开或换掉
  // （拆开了宿主就还原不了，客户端拿到的是占位符而不是原值）
  const PLACEHOLDER = /<<TW_[A-Z0-9_]{1,64}>>/g;
  // 文字末尾可能是一个还没收全的占位符
  const PARTIAL = /^<(?:<(?:T(?:W(?:_[A-Z0-9_]{0,64}>?)?)?)?)?$/;
  const PARTIAL_MAX = 70;

  // `s` 末尾那个可能还没收全的占位符从哪儿起；没有就是 `s.length`。只看末尾那几十个字里
  // 的 `<`（逐段时每段都要查一次，多半一个 `<` 都没有）
  function partial(s) {
    const n = s.length;
    for (let p = indexOf(s, "<", MathMax(0, n - PARTIAL_MAX)); p !== -1; p = indexOf(s, "<", p + 1)) {
      if (exec(PARTIAL, slice(s, p), 0) !== null) return p;
    }
    return n;
  }

  // ── 替换规则 ─────────────────────────────────────────────────
  //
  // 规则是 [查找, 替换] 对的列表（数组、Map 或别的可迭代对象）。查找是字符串（按字面）或
  // 正则（按全局找，`g` 这里加）；替换是字符串（查找为字符串时按字面写入，为正则时认 `$1`、
  // `$<name>` 这些）或函数，和 String.prototype.replace 一样。
  //
  // 扫一遍、从左到右：同一位置几条规则都命中时取列表里靠前的；换进去的字不再参与匹配。

  function pairOf(e, n, who) {
    if (!ArrayIsArray(e) || e.length !== 2) {
      throw new TypeErrorCtor(who + ": rule " + n + " is not a [find, replacement] pair");
    }
    return e;
  }

  function pairsOf(rules, who) {
    const out = list();
    if (ArrayIsArray(rules)) {
      const n = rules.length;
      for (let i = 0; i < n; i++) out[out.length] = pairOf(rules[i], i + 1, who);
      return out;
    }
    if (isMap(rules)) {
      const it = ReflectApply(MapEntries, rules, []);
      for (;;) {
        const r = ReflectApply(MapIteratorNext, it, []);
        if (r.done) return out;
        out[out.length] = pairOf(r.value, out.length + 1, who);
      }
    }
    const f = rules !== null && (typeof rules === "object" || typeof rules === "function") ? rules[SymbolIterator] : undefined;
    if (typeof f !== "function") {
      throw new TypeErrorCtor(who + ": the rules must be an array or a Map of [find, replacement] pairs, not " + typeName(rules));
    }
    // 插件自己的可迭代对象：照协议走，它的 next 是它自己的
    const it = ReflectApply(f, rules, []);
    const next = it.next;
    for (;;) {
      const r = ReflectApply(next, it, []);
      if (r === null || typeof r !== "object") throw new TypeErrorCtor(who + ": the rules' iterator returned " + typeName(r));
      if (r.done) return out;
      out[out.length] = pairOf(r.value, out.length + 1, who);
    }
  }

  // 核对一遍，编成扫描用的样子。`who` 是报错时的名字
  function compile(rules, who) {
    const pairs = pairsOf(rules, who);
    const out = list();
    let regex = false;
    let literal = true;
    let longest = 0;
    for (let i = 0; i < pairs.length; i++) {
      const n = i + 1;
      const find = pairs[i][0];
      const to = pairs[i][1];
      // at / end / hit：扫描时这条规则从游标起的下一处命中（见 scan）
      const r = { __proto__: null, n, find: null, re: null, to: null, fn: null, plain: true, at: -2, end: 0, hit: null };
      if (typeof find === "string") {
        if (find === "") throw new TypeErrorCtor(who + ": rule " + n + " looks for an empty string");
        r.find = find;
        if (find.length > longest) longest = find.length;
      } else if (isRegExp(find)) {
        if (RegExpSticky !== undefined && ReflectApply(RegExpSticky, find, [])) {
          throw new TypeErrorCtor(who + ": rule " + n + " has the y flag, which a rule cannot use");
        }
        let flags = "g";
        for (let k = 0; k < FLAGS.length; k++) {
          const g = FLAGS[k][1];
          if (g !== undefined && ReflectApply(g, find, [])) flags += FLAGS[k][0];
        }
        r.re = new RegExpCtor(ReflectApply(RegExpSource, find, []), flags);
        if (exec(r.re, "", 0) !== null) {
          throw new TypeErrorCtor(who + ": rule " + n + " can match an empty string; a rule has to match at least one character");
        }
        regex = true;
      } else {
        throw new TypeErrorCtor(who + ": rule " + n + " looks for " + typeName(find) + "; use a string or a regular expression");
      }
      if (typeof to === "string") {
        r.to = to;
        r.plain = r.re === null || indexOf(to, "$", 0) === -1;
      } else if (typeof to === "function") {
        r.fn = to;
        literal = false;
      } else {
        throw new TypeErrorCtor(who + ": rule " + n + " replaces with " + typeName(to) + "; use a string or a function");
      }
      out[out.length] = r;
    }
    return { __proto__: null, who, rules: out, regex, longest, prefixes: null, literal: literal && !regex, split: undefined };
  }

  // 逐段要的：每个查找字符串的真前缀（文字末尾是不是某个查找字符串的开头，查一次）
  function withPrefixes(c) {
    const set = new SetCtor();
    for (let k = 0; k < c.rules.length; k++) {
      const f = c.rules[k].find;
      if (f === null) continue;
      for (let n = 1; n < f.length; n++) setAdd(set, slice(f, 0, n));
    }
    c.prefixes = set;
    return c;
  }

  // 替换字符串里的 `$` 写法，照 String.prototype.replace（规范的 GetSubstitution）
  function expand(t, m, s, at) {
    const matched = m[0];
    const captures = m.length - 1;
    const groups = m.groups;
    let out = "";
    let i = 0;
    for (;;) {
      const d = indexOf(t, "$", i);
      if (d === -1) return out + slice(t, i);
      out += slice(t, i, d);
      const c = code(t, d + 1);
      if (c === 36) {
        out += "$";
        i = d + 2;
      } else if (c === 38) {
        out += matched;
        i = d + 2;
      } else if (c === 96) {
        out += slice(s, 0, at);
        i = d + 2;
      } else if (c === 39) {
        out += slice(s, at + matched.length);
        i = d + 2;
      } else if (c >= 48 && c <= 57) {
        const c2 = code(t, d + 2);
        let count = c2 >= 48 && c2 <= 57 ? 2 : 1;
        let index = count === 2 ? (c - 48) * 10 + (c2 - 48) : c - 48;
        if (index > captures && count === 2) {
          count = 1;
          index = c - 48;
        }
        if (index >= 1 && index <= captures) {
          const v = m[index];
          if (v !== undefined) out += v;
        } else {
          out += slice(t, d, d + 1 + count);
        }
        i = d + 1 + count;
      } else if (c === 60) {
        const gt = groups === undefined ? -1 : indexOf(t, ">", d);
        if (gt === -1) {
          out += "$<";
          i = d + 2;
        } else {
          const v = groups[slice(t, d + 2, gt)];
          if (v !== undefined) out += `${v}`;
          i = gt + 1;
        }
      } else {
        out += "$";
        i = d + 1;
      }
    }
  }

  // 一处命中换成什么。`s` 是正在扫的文字，`at` 是命中在它里面的位置。函数收到的位置按
  // 整节（两个占位符之间那一段）算：`base` 是 `s` 在那一节里的起点，整段和逐段一样
  function replacement(r, m, s, at, base) {
    if (r.fn === null) return r.plain ? r.to : expand(r.to, m, s, at);
    let args;
    if (r.re === null) {
      args = [r.find, base + at, s];
    } else {
      args = list();
      for (let i = 0; i < m.length; i++) args[args.length] = m[i];
      args[args.length] = base + at;
      args[args.length] = s;
      if (m.groups !== undefined) args[args.length] = m.groups;
    }
    return `${ReflectApply(r.fn, undefined, args)}`;
  }

  // 正则规则从 `cur` 起的下一处命中，记进它的 at / end / hit。没有是 -1
  function locate(c, r, s, cur) {
    let m = exec(r.re, s, cur);
    // 带 u 的正则从一个代理对中间起找时，命中可能落在 cur 前面一格：从下一格重找
    if (m !== null && m.index < cur) m = exec(r.re, s, cur + 1);
    if (m === null || m.index < cur) {
      r.at = -1;
      return -1;
    }
    const a = m.index;
    const e = a + m[0].length;
    if (e === a) {
      throw new TypeErrorCtor(c.who + ": rule " + r.n + " matched an empty string; a rule has to match at least one character");
    }
    r.at = a;
    r.end = e;
    r.hit = m;
    return a;
  }

  // 扫 `s` 里 [from, limit) 这一段，换好的追加到 `out`。起点在 limit 之前的命中都算（它可以
  // 越过 limit）；limit 之前没有命中的原样追加。返回扫到了哪儿（不小于 limit）。
  //
  // 每条规则记着它从游标起的下一处命中（at：-2 还没找过，-1 往后再也没有），只有被别的命中
  // 盖过去的才重找：每处命中只多花一次 indexOf 或 exec
  function scan(c, s, from, limit, out, base) {
    const rules = c.rules;
    const n = rules.length;
    for (let k = 0; k < n; k++) rules[k].at = -2;
    let cur = from;
    while (cur < limit) {
      let best = null;
      let bestAt = limit;
      for (let k = 0; k < n; k++) {
        const r = rules[k];
        let a = r.at;
        if (a === -1) continue;
        if (a < cur) {
          if (r.re === null) {
            a = indexOf(s, r.find, cur);
            r.at = a;
            r.end = a + r.find.length;
          } else {
            a = locate(c, r, s, cur);
          }
          if (a === -1) continue;
        }
        if (a < bestAt) {
          bestAt = a;
          best = r;
        }
      }
      if (best === null) break;
      if (bestAt > cur) out[out.length] = slice(s, cur, bestAt);
      out[out.length] = best.fn === null && best.plain ? best.to : replacement(best, best.hit, s, bestAt, base);
      cur = best.end;
    }
    if (cur < limit) {
      out[out.length] = slice(s, cur, limit);
      cur = limit;
    }
    return cur;
  }

  // ── 全是字符串的快路径 ───────────────────────────────────────
  //
  // 规则全是「字符串 → 字符串」、两两之间不会在文字里交叠（谁也不是谁的一部分，谁的结尾也
  // 不是谁的开头）时：每处命中都只可能是一条规则的，一次扫描挑出来的就是每条规则各自从左
  // 到右、不重叠地找到的那些。于是每条规则用一次原生的 split 就找全了，不必一处一处在
  // JavaScript 里挑。
  //
  // 规则两两不交叠时先后无所谓，排一个遍数最少的顺序：一条规则的替换文字不空、和还没换的
  // 那些查找字符串没有一个共同的字（按 UTF-16 码元比），就直接换 —— 后面的规则在它里面找
  // 不到东西，也不能拿它和旁边的字拼出新的命中。不行的先放一个文字里没有的私用区字符占位，
  // 全部规则找完再换成替换文字。最后剩下的一条后面没有规则了，直接换。

  // 走快路径的最短文字和最多规则：短文字逐个挑也很快，规则多了 split 的遍数跟着多
  const LITERAL_MIN = 256;
  const LITERAL_MAX_RULES = 16;
  // 同一套规则判断一次（tw.texts 里每段文字都会调一次 tw.replace）。攒多了整个换掉
  let splitPlans = new MapCtor();
  let splitPlanCount = 0;

  // 这套规则能不能走快路径：去掉重复的查找字符串（同一个查找字符串取第一条），再看两两之间
  // 会不会交叠。不能就是 null
  function splitPlan(c) {
    if (c.split !== undefined) return c.split;
    const rules = c.rules;
    let key = "";
    for (let k = 0; k < rules.length; k++) key += rules[k].find.length + ":" + rules[k].find + rules[k].to.length + ":" + rules[k].to;
    let plan = mapGet(splitPlans, key);
    if (plan === undefined) {
      plan = makePlan(rules);
      if (splitPlanCount >= 32) {
        splitPlans = new MapCtor();
        splitPlanCount = 0;
      }
      mapSet(splitPlans, key, plan);
      splitPlanCount++;
    }
    c.split = plan;
    return plan;
  }

  function makePlan(rules) {
    const finds = list();
    const tos = list();
    let chars = "";
    for (let k = 0; k < rules.length; k++) {
      const f = rules[k].find;
      let seen = false;
      for (let j = 0; j < finds.length; j++) if (finds[j] === f) seen = true;
      if (seen) continue;
      if (finds.length >= LITERAL_MAX_RULES) return null;
      finds[finds.length] = f;
      tos[tos.length] = rules[k].to;
      chars += f + rules[k].to;
    }
    let selfOverlap = false;
    for (let a = 0; a < finds.length; a++) {
      for (let b = 0; b < finds.length; b++) {
        if (a !== b && (indexOf(finds[b], finds[a], 0) !== -1 || endStarts(finds[a], finds[b]))) return null;
      }
      if (endStarts(finds[a], finds[a])) selfOverlap = true;
    }
    // 排顺序：steps 里每一步是 [查找, 替换, 要不要占位]
    const steps = list();
    const left = list();
    for (let k = 0; k < finds.length; k++) left[k] = k;
    while (left.length > 0) {
      let pick = -1;
      for (let i = 0; i < left.length && pick === -1; i++) {
        const to = tos[left[i]];
        let others = "";
        for (let j = 0; j < left.length; j++) if (j !== i) others += finds[left[j]];
        if (left.length === 1 || (to !== "" && !sharesUnit(to, others))) pick = i;
      }
      const marked = pick === -1;
      if (marked) pick = 0;
      const k = left[pick];
      steps[steps.length] = [finds[k], tos[k], marked];
      for (let i = pick; i < left.length - 1; i++) left[i] = left[i + 1];
      left.length = left.length - 1;
    }
    // selfOverlap：有查找字符串自己和自己交叠（aa、abab）。逐段时它的一处命中可能跨过
    // frontier，块中间那几段不走快路径（见 drain）
    return { __proto__: null, steps, chars, selfOverlap };
  }

  // a 里有没有 b 里也有的 UTF-16 码元
  function sharesUnit(a, b) {
    for (let i = 0; i < a.length; i++) if (indexOf(b, slice(a, i, i + 1), 0) !== -1) return true;
    return false;
  }

  // a 的一段真后缀是 b 的开头
  function endStarts(a, b) {
    const first = slice(b, 0, 1);
    for (let i = indexOf(a, first, 1); i !== -1; i = indexOf(a, first, i + 1)) {
      const tail = slice(a, i);
      if (tail.length < b.length && slice(b, 0, tail.length) === tail) return true;
    }
    return false;
  }

  // 用快路径换一节文字。找不到够用的占位字符时是 null（回到逐个挑）
  function splitReplace(plan, seg) {
    const steps = plan.steps;
    const marks = list();
    const later = list();
    let ch = 0xe000;
    let t = seg;
    for (let k = 0; k < steps.length; k++) {
      const step = steps[k];
      if (!step[2]) {
        t = joinWith(split(t, step[0]), step[1]);
        continue;
      }
      // 占位字符：文字里没有，查找、替换文字里也没有
      let m = "";
      for (; ch < 0xf900 && m === ""; ch++) {
        const c = StringFromCharCode(ch);
        if (indexOf(seg, c, 0) === -1 && indexOf(plan.chars, c, 0) === -1) m = c;
      }
      if (m === "") return null;
      t = joinWith(split(t, step[0]), m);
      marks[marks.length] = m;
      later[later.length] = step[1];
    }
    for (let k = 0; k < marks.length; k++) t = joinWith(split(t, marks[k]), later[k]);
    return t;
  }

  // 一节文字（两个占位符之间那一段）整个换掉
  function replaceSection(c, seg, out) {
    if (c.literal && seg.length >= LITERAL_MIN) {
      const plan = splitPlan(c);
      const t = plan === null ? null : splitReplace(plan, seg);
      if (t !== null) {
        out[out.length] = t;
        return;
      }
    }
    scan(c, seg, 0, seg.length, out, 0);
  }

  // 整段替换：按占位符分节，一节一节地换
  function replaceWhole(c, text) {
    if (c.rules.length === 0) return text;
    const out = list();
    let from = 0;
    if (indexOf(text, "<<TW_", 0) !== -1) {
      for (;;) {
        const m = exec(PLACEHOLDER, text, from);
        if (m === null) break;
        replaceSection(c, slice(text, from, m.index), out);
        out[out.length] = m[0];
        from = m.index + m[0].length;
      }
    }
    replaceSection(c, from === 0 ? text : slice(text, from), out);
    return join(out);
  }

  // ── 逐段替换 ─────────────────────────────────────────────────
  //
  // 一块文字一份状态（一个 lane 一份）：
  //   pending：收到了、还没定下来的原文；
  //   ctx：这一节里已经定下来的原文的末尾（至多 maxMatch 个字），只给正则的后顾断言看，
  //        不再放出；
  //   base：ctx 在这一节里的起点（函数替换收到的位置按整节算）。
  //
  // 每收到一段：先把收全了的占位符前面那一节整个定下来；剩下的文字里，末尾可能还没收全的
  // 占位符先扣着，其余的扫到「再来什么字也改变不了结果」的地方（frontier）为止。块结束时
  // 全部放出。这样拼起来的结果和对整块调 tw.replace 一样：字符串规则总是如此；正则规则在
  // 一处命中（连同前后断言看到的范围）不超过 maxMatch 个字时如此。替换函数收到的「整段
  // 文字」和替换字符串里的 $`、$' 例外：逐段时它们只看得到缓冲着的那一段。

  // 扫到哪儿为止，结果不会再随后面的字变：字符串规则扣住末尾可能是某个查找字符串开头的
  // 部分；有正则就扣住末尾 max 个字。不把一个代理对切成两半
  function frontier(c, a, cur, max) {
    const n = a.length;
    let f = c.regex ? n - max : n;
    if (c.longest > 1) {
      for (let p = MathMax(cur, n - c.longest + 1); p < f; p++) {
        if (setHas(c.prefixes, slice(a, p))) {
          f = p;
          break;
        }
      }
    }
    if (f <= cur) return cur;
    if (f < n && isHigh(a, f - 1) && isLow(a, f)) f -= 1;
    return f;
  }

  // 一节到头了（遇到占位符或者块结束），从 from 起整个换掉。全是字符串的规则没有前文
  // （ctx 总是空的），能走快路径
  function finishSection(c, a, from, out, base) {
    if (c.literal && from === 0) replaceSection(c, a, out);
    else scan(c, a, from, a.length, out, base);
  }

  function drain(st, max, end) {
    const c = st.c;
    const out = list();
    // 收全了的占位符：它前面那一节到头了，整个定下来
    while (indexOf(st.pending, "<<TW_", 0) !== -1) {
      const m = exec(PLACEHOLDER, st.pending, 0);
      if (m === null) break;
      finishSection(c, st.ctx + slice(st.pending, 0, m.index), st.ctx.length, out, st.base);
      out[out.length] = m[0];
      st.ctx = "";
      st.base = 0;
      st.pending = slice(st.pending, m.index + m[0].length);
    }
    if (end) {
      finishSection(c, st.ctx + st.pending, st.ctx.length, out, st.base);
      st.ctx = "";
      st.pending = "";
      return join(out);
    }
    const h = partial(st.pending);
    const a = st.ctx + slice(st.pending, 0, h);
    let cur = st.ctx.length;
    for (;;) {
      const lim = frontier(c, a, cur, max);
      // 全是字符串、两两不交叠、也没有自己和自己交叠的：起点在 frontier 之前的命中不会越过
      // 它（越过的话，它的一段真后缀就是 frontier 处那个查找字符串的开头），前面这一截可以
      // 单独整个换
      if (c.literal && lim - cur >= LITERAL_MIN) {
        const plan = splitPlan(c);
        const t = plan === null || plan.selfOverlap ? null : splitReplace(plan, slice(a, cur, lim));
        if (t !== null) {
          out[out.length] = t;
          cur = lim;
          break;
        }
      }
      const next = scan(c, a, cur, lim, out, st.base);
      // 一处命中越过了 frontier：从它后面再算一次
      if (next <= lim) {
        cur = next;
        break;
      }
      cur = next;
    }
    const keep = c.regex ? MathMin(max, cur) : 0;
    st.base += cur - keep;
    st.ctx = keep === 0 ? "" : slice(a, cur - keep, cur);
    st.pending = slice(a, cur) + slice(st.pending, h);
    return join(out);
  }

  // ── tw ───────────────────────────────────────────────────────

  const tw = {
    // 在系统提示词末尾附加一段文字：和原文之间空一行；原来是空的就直接写入；已经包含
    // 这段文字就不再附加
    appendSystem(req, text) {
      const who = "tw.appendSystem";
      needRequest(req, who);
      const system = req.system;
      if (system === undefined) {
        throw new TypeErrorCtor(who + ' needs req.system: add the "system" permission');
      }
      needString(system, who, "req.system");
      needString(text, who, "the text");
      if (indexOf(system, text, 0) !== -1) return;
      req.system = system === "" ? text : system + "\n\n" + text;
    },

    // 逐段处理能改的文字：系统提示词、消息里的文字片段和工具结果。fn 返回字符串就换掉，
    // 返回 undefined 不动。思考、图片、工具调用和别的片段不交给它
    texts(req, fn) {
      const who = "tw.texts";
      needRequest(req, who);
      needFunction(fn, who);
      const system = req.system;
      const ms = req.messages;
      if (system === undefined && ms === undefined) {
        throw new TypeErrorCtor(who + ' needs req.system or req.messages: add the "system" or "messages" permission');
      }
      const call = (text, kind, role) => {
        const r = ReflectApply(fn, undefined, [text, { kind, role }]);
        if (r !== undefined && typeof r !== "string") {
          throw new TypeErrorCtor(who + ": the function returned " + typeName(r) + "; return a string, or undefined to keep the text");
        }
        return r;
      };
      if (system !== undefined) {
        needString(system, who, "req.system");
        const r = call(system, "system", null);
        if (r !== undefined) req.system = r;
      }
      if (ms === undefined) return;
      if (!ArrayIsArray(ms)) throw new TypeErrorCtor(who + ": req.messages is " + typeName(ms) + ", not an array");
      for (let i = 0; i < ms.length; i++) {
        const m = ms[i];
        const ps = partsOf(m, i, who);
        for (let j = 0; j < ps.length; j++) {
          const p = ps[j];
          if (p === null || typeof p !== "object") continue;
          const kind = p.type;
          if (kind !== "text" && kind !== "tool_result") continue;
          const text = p.text;
          if (typeof text !== "string") continue;
          const r = call(text, kind, m.role);
          if (r !== undefined) p.text = r;
        }
      }
    },

    // 对话历史里的每个工具调用片段交给 fn（就是那个片段本身：改它的 input 即生效）
    toolCalls(req, fn) {
      const who = "tw.toolCalls";
      needRequest(req, who);
      needFunction(fn, who);
      const ms = messagesOf(req, who);
      for (let i = 0; i < ms.length; i++) {
        const ps = partsOf(ms[i], i, who);
        for (let j = 0; j < ps.length; j++) {
          const p = ps[j];
          if (p !== null && typeof p === "object" && p.type === "tool_call") ReflectApply(fn, undefined, [p]);
        }
      }
    },

    // 一份副本，其中每个字符串值换成 fn(字符串, 键)：键是属性名、数组下标，最外层是 null。
    // 对象的键、数字、布尔值、null 原样
    mapStrings(value, fn) {
      const who = "tw.mapStrings";
      needFunction(fn, who);
      const path = list();
      const walk = (v, key) => {
        if (typeof v === "string") {
          const r = ReflectApply(fn, undefined, [v, key]);
          if (typeof r !== "string") {
            throw new TypeErrorCtor(who + ": the function returned " + typeName(r) + "; it has to return a string");
          }
          return r;
        }
        if (v === null || typeof v !== "object") return v;
        for (let i = 0; i < path.length; i++) {
          if (path[i] === v) throw new TypeErrorCtor(who + ": the value refers to itself");
        }
        path[path.length] = v;
        let out;
        if (ArrayIsArray(v)) {
          out = [];
          const n = v.length;
          for (let i = 0; i < n; i++) define(out, i, walk(v[i], i));
        } else {
          out = {};
          const ks = ObjectKeys(v);
          for (let i = 0; i < ks.length; i++) define(out, ks[i], walk(v[ks[i]], ks[i]));
        }
        path.length = path.length - 1;
        return out;
      };
      return walk(value, null);
    },

    replace(text, rules) {
      needString(text, "tw.replace", "the text");
      return replaceWhole(compile(rules, "tw.replace"), text);
    },

    // 逐段模式的回答钩子：`export const { onReplyText, onReplyTextEnd } = tw.streamReplace(rules)`。
    // rules 可以是一个函数 (ctx) => 规则，每块文字开始时调一次（读设置用）
    streamReplace(rules, options) {
      const who = "tw.streamReplace";
      let max = 256;
      if (options !== undefined) {
        if (options === null || typeof options !== "object") {
          throw new TypeErrorCtor(who + ": the options must be an object, not " + typeName(options));
        }
        const v = options.maxMatch;
        if (v !== undefined) {
          if (typeof v !== "number" || !NumberIsInteger(v) || v < 1) {
            throw new TypeErrorCtor(who + ": maxMatch must be a positive integer, not " + (typeof v === "number" ? StringCtor(v) : typeName(v)));
          }
          max = v;
        }
      }
      const dynamic = typeof rules === "function";
      // 固定的规则现在就核对：写错了，插件加载时就报
      const fixed = dynamic ? null : withPrefixes(compile(rules, who));
      const rulesFor = (ctx) => (dynamic ? compile(ReflectApply(rules, undefined, [ctx]), who) : fixed);
      // lane → 这一块的状态
      const lanes = new MapCtor();
      return {
        onReplyText(text, ctx) {
          if (typeof text !== "string") throw new TypeErrorCtor(who + ": onReplyText got " + typeName(text) + ", not a string");
          // 整块模式：一次就是一整块，整段换
          if (!stream) return replaceWhole(rulesFor(ctx), text);
          if (!ends) {
            throw new ErrorCtor(who + ": export its onReplyTextEnd as well; without it the text held back at the end of a block is lost");
          }
          let st = mapGet(lanes, lane);
          if (st === undefined) {
            const c = rulesFor(ctx);
            st = { __proto__: null, c: c.prefixes === null ? withPrefixes(c) : c, pending: "", ctx: "", base: 0 };
            mapSet(lanes, lane, st);
          }
          st.pending += text;
          return drain(st, max, false);
        },
        onReplyTextEnd() {
          const st = mapGet(lanes, lane);
          if (st === undefined) return "";
          mapDelete(lanes, lane);
          return drain(st, max, true);
        },
      };
    },

    // 按行拆（\r\n 也算一个换行），每行去掉首尾空白，略去空行
    lines(text) {
      needString(text, "tw.lines", "the text");
      const out = list();
      let from = 0;
      for (;;) {
        const i = indexOf(text, "\n", from);
        const line = trim(slice(text, from, i === -1 ? text.length : i));
        if (line !== "") out[out.length] = line;
        if (i === -1) return array(out);
        from = i + 1;
      }
    },

    // 一行一对：每个非空行在第一个分隔符处拆成 [左, 右]，两边去掉空白。缺分隔符、左边是空
    // 的行抛错，说出是第几行（从 1 数）—— 忽略它的话，设置里的笔误就无从察觉
    pairs(text, separator) {
      const who = "tw.pairs";
      needString(text, who, "the text");
      const sep = separator === undefined ? "=>" : separator;
      needString(sep, who, "the separator");
      if (sep === "") throw new TypeErrorCtor(who + ": the separator must not be empty");
      const out = list();
      let from = 0;
      for (let n = 1; ; n++) {
        const i = indexOf(text, "\n", from);
        const line = slice(text, from, i === -1 ? text.length : i);
        if (trim(line) !== "") {
          const at = indexOf(line, sep, 0);
          if (at === -1) throw new ErrorCtor(who + ": line " + n + ' has no "' + sep + '"');
          const left = trim(slice(line, 0, at));
          if (left === "") throw new ErrorCtor(who + ": line " + n + ' has nothing before "' + sep + '"');
          out[out.length] = [left, trim(slice(line, at + sep.length))];
        }
        if (i === -1) return array(out);
        from = i + 1;
      }
    },
  };
  ObjectFreeze(tw);

  ObjectDefineProperty(G, "tw", { value: tw, writable: true, configurable: true, enumerable: false });
  ObjectDefineProperty(G, "__tw_helpers", {
    value: ObjectFreeze({
      __proto__: null,
      lane(n) {
        lane = n;
      },
      mode(isStream, hasEnd) {
        stream = isStream;
        ends = hasEnd;
      },
    }),
    writable: true,
    configurable: true,
    enumerable: false,
  });
})();
