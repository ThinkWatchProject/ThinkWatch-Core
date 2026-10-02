// 插件桥：在沙箱里、插件之前求值一次，然后连同整个 QuickJS 一起做进快照
// （见 build.rs）。整个脚本的值是给 Rust 胶水（guest/src/lib.rs）的桥对象 ——
// 它不挂在任何全局上，插件拿不到。
//
// 它做四件事：
//   1. 只留标准 ECMAScript 的全局，外加 console 和 reject；
//   2. Math.random 换成每个实例重新播种的版本（快照把原来的状态冻住了）；
//   3. 调钩子，把返回值按钩子的类型核对、转成 JSON；
//   4. 把异常整理成 {"message","stack"}。
//
// **宿主不信这里的任何结论。**插件和桥在同一个领域里，插件能改原型、改内建
// 函数，所以这里的核对只是为了给出好懂的错误；输出交回宿主后，宿主照样按
// JSON 解析、按钩子的类型重新核对一遍。这里要防的只是「桥自己被插件弄崩」：
// 用到的内建函数在插件运行之前就拿住，之后不再从全局或原型上取。
"use strict";
(function () {
  const G = globalThis;

  const JSONparse = JSON.parse;
  const JSONstringify = JSON.stringify;
  const ObjectFreeze = Object.freeze;
  const ObjectKeys = Object.keys;
  const ObjectDefineProperty = Object.defineProperty;
  const ArrayIsArray = Array.isArray;
  const ReflectApply = Reflect.apply;
  const ReflectOwnKeys = Reflect.ownKeys;
  const ReflectDeleteProperty = Reflect.deleteProperty;
  const MathImul = Math.imul;
  const StringCtor = String;
  const StringSlice = String.prototype.slice;
  const StringSplit = String.prototype.split;
  const StringIndexOf = String.prototype.indexOf;
  const StringToWellFormed = String.prototype.toWellFormed;
  const ArrayJoin = Array.prototype.join;
  const PromiseThen = Promise.prototype.then;
  const ErrorCtor = Error;
  const TypeErrorCtor = TypeError;
  const hostLog = G.__tw_log;

  // 状态码：输出的第一个字符（Rust 那边按它分派）
  const VALUE = "0";
  const UNCHANGED = "1";
  const REJECTED = "2";
  const THREW = "3";
  const BAD = "4";
  const DROP = "5";

  const HOOKS = ["onRequest", "onReplyText", "onReplyTextEnd", "onToolCall"];
  const MAX_MESSAGE = 4096;
  const MAX_STACK = 16384;

  let hooks = { __proto__: null };
  let ctxValue;
  let current = null; // 正在跑的钩子名（reject 只认 onRequest）
  let rejected = null; // reject() 给的理由
  let outcome = null; // { done, ok, value }

  function clip(s, n) {
    return s.length > n ? ReflectApply(StringSlice, s, [0, n]) : s;
  }

  function wellFormed(s) {
    return ReflectApply(StringToWellFormed, s, []);
  }

  // 任何值转成一行可读的文字。绝不抛出：插件给的对象可能带会抛的 getter、
  // 会抛的 toJSON、Proxy、循环引用
  function show(v) {
    try {
      switch (typeof v) {
        case "string":
          return v;
        case "undefined":
          return "undefined";
        case "bigint":
          return StringCtor(v) + "n";
        case "symbol":
        case "number":
        case "boolean":
          return StringCtor(v);
        case "function":
          return "[Function]";
      }
      if (v === null) return "null";
      if (v instanceof ErrorCtor) return errorLine(v);
      const j = JSONstringify(v);
      return typeof j === "string" ? j : StringCtor(v);
    } catch (_) {
      try {
        return StringCtor(v);
      } catch (_) {
        return "[object]";
      }
    }
  }

  function errorLine(e) {
    let name = "Error";
    let message = "";
    try {
      name = StringCtor(e.name);
    } catch (_) {}
    try {
      message = StringCtor(e.message);
    } catch (_) {}
    return message === "" ? name : name + ": " + message;
  }

  // 栈里桥自己的那几帧对插件作者没有意义，去掉
  function cleanStack(s) {
    const lines = ReflectApply(StringSplit, s, ["\n"]);
    const kept = [];
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      if (line === "" || ReflectApply(StringIndexOf, line, ["bridge.js"]) !== -1) continue;
      kept[kept.length] = line;
    }
    return ReflectApply(ArrayJoin, kept, ["\n"]);
  }

  // 异常 → {"message","stack"}。JSON 用拼接写：对象字面量交给 JSON.stringify
  // 的话，插件在 Object.prototype 上挂一个 toJSON 就能改掉它。字符串原值不会去
  // 查 toJSON，可以放心交给它
  function describe(e) {
    let message;
    let stack = null;
    try {
      if (e !== null && typeof e === "object" && e instanceof ErrorCtor) {
        message = errorLine(e);
        try {
          const s = e.stack;
          if (typeof s === "string") stack = cleanStack(s);
        } catch (_) {}
      } else {
        message = "Uncaught " + show(e);
      }
    } catch (_) {
      message = "Uncaught exception";
    }
    message = clip(wellFormed(StringCtor(message)), MAX_MESSAGE);
    return (
      '{"message":' +
      JSONstringify(message) +
      ',"stack":' +
      (stack === null || stack === "" ? "null" : JSONstringify(clip(wellFormed(stack), MAX_STACK))) +
      "}"
    );
  }

  function typeName(v) {
    if (v === null) return "null";
    if (ArrayIsArray(v)) return "an array";
    return typeof v === "object" ? "an object" : "a " + typeof v;
  }

  function json(v, hook) {
    let s;
    try {
      s = JSONstringify(v);
    } catch (e) {
      return BAD + hook + " returned a value that cannot be turned into JSON: " + clip(show(e), 500);
    }
    if (typeof s !== "string") return BAD + hook + " returned a value that cannot be turned into JSON";
    return VALUE + s;
  }

  function finish(kind, v) {
    const hook = HOOKS[kind];
    switch (kind) {
      case 0:
        if (v === undefined) return UNCHANGED;
        if (v === null || typeof v !== "object" || ArrayIsArray(v)) {
          return BAD + "onRequest must return the request object or undefined, not " + typeName(v);
        }
        return json(v, hook);
      case 1:
      case 2:
        if (v === undefined) return UNCHANGED;
        if (typeof v !== "string") {
          return BAD + hook + " must return a string or undefined, not " + typeName(v);
        }
        // 切半个代理对（按 UTF-16 下标截文字时常见）在这里换成 U+FFFD，
        // 和 TextEncoder 的做法一样，不让一个截断的表情把整个回答弄挂
        return VALUE + wellFormed(v);
      case 3:
        if (v === undefined) return UNCHANGED;
        if (v === null) return DROP;
        if (typeof v !== "object") {
          return BAD + "onToolCall must return a tool call, an array of them, null or undefined, not " + typeName(v);
        }
        if (ArrayIsArray(v)) {
          if (v.length === 0) return DROP;
          for (let i = 0; i < v.length; i++) {
            const c = v[i];
            if (c === null || typeof c !== "object" || ArrayIsArray(c)) {
              return BAD + "onToolCall returned an array whose item " + i + " is " + typeName(c) + ", not a tool call";
            }
          }
          return json(v, hook);
        }
        return json([v], hook);
    }
    return BAD + "unknown hook";
  }

  // ── 插件看得见的全局 ─────────────────────────────────────────

  function emit(level, args) {
    let line = "";
    for (let i = 0; i < args.length; i++) {
      if (i > 0) line += " ";
      line += show(args[i]);
      if (line.length > 8192) break;
    }
    hostLog(level, wellFormed(clip(line, 8192)));
  }

  const consoleObject = {
    log(...args) {
      emit(0, args);
    },
    info(...args) {
      emit(1, args);
    },
    warn(...args) {
      emit(2, args);
    },
    error(...args) {
      emit(3, args);
    },
    debug(...args) {
      emit(0, args);
    },
  };

  function reject(reason) {
    if (current !== "onRequest") {
      throw new TypeErrorCtor("reject() can only be called inside onRequest");
    }
    // 理由记下就算数：插件自己 catch 住这个异常也照样拒绝
    if (rejected === null) {
      rejected = clip(wellFormed(reason === undefined ? "" : show(reason)), MAX_MESSAGE);
    }
    throw new ErrorCtor("the request was rejected by the plugin");
  }

  // xoshiro128**：32 位运算就够，种子由宿主每个实例给一次
  let s0 = 1;
  let s1 = 2;
  let s2 = 3;
  let s3 = 4;
  function rotl(x, k) {
    return (x << k) | (x >>> (32 - k));
  }
  function next32() {
    const result = MathImul(rotl(MathImul(s1, 5), 7), 9);
    const t = s1 << 9;
    s2 ^= s0;
    s3 ^= s1;
    s1 ^= s2;
    s0 ^= s3;
    s2 ^= t;
    s3 = rotl(s3, 11);
    return result >>> 0;
  }
  const random = {
    random() {
      return ((next32() >>> 5) * 67108864 + (next32() >>> 6)) / 9007199254740992;
    },
  }.random;

  ObjectDefineProperty(Math, "random", { value: random, writable: true, configurable: true, enumerable: false });
  ObjectDefineProperty(G, "console", { value: consoleObject, writable: true, configurable: true, enumerable: false });
  ObjectDefineProperty(G, "reject", { value: reject, writable: true, configurable: true, enumerable: false });

  // 全局只留 ECMAScript 标准里的（含附录 B 的 escape/unescape）和上面两个。
  // 引擎哪天多给了什么（queueMicrotask、performance、navigator……），在这里统一去掉
  const KEEP = [
    "globalThis", "Infinity", "NaN", "undefined",
    "eval", "isFinite", "isNaN", "parseFloat", "parseInt",
    "decodeURI", "decodeURIComponent", "encodeURI", "encodeURIComponent", "escape", "unescape",
    "AggregateError", "Array", "ArrayBuffer", "AsyncDisposableStack", "BigInt", "BigInt64Array",
    "BigUint64Array", "Boolean", "DataView", "Date", "DisposableStack", "Error", "EvalError",
    "FinalizationRegistry", "Float16Array", "Float32Array", "Float64Array", "Function",
    "Int8Array", "Int16Array", "Int32Array", "Iterator", "Map", "Number", "Object", "Promise",
    "Proxy", "RangeError", "ReferenceError", "RegExp", "Set", "SharedArrayBuffer", "String",
    "SuppressedError", "Symbol", "SyntaxError", "TypeError", "Uint8Array", "Uint8ClampedArray",
    "Uint16Array", "Uint32Array", "URIError", "WeakMap", "WeakRef", "WeakSet",
    "Atomics", "JSON", "Math", "Reflect",
    "console", "reject",
  ];
  const keep = { __proto__: null };
  for (let i = 0; i < KEEP.length; i++) keep[KEEP[i]] = true;
  const names = ReflectOwnKeys(G);
  for (let i = 0; i < names.length; i++) {
    const k = names[i];
    if (typeof k === "string" && keep[k] !== true) ReflectDeleteProperty(G, k);
  }

  // ── 给 Rust 胶水的桥 ─────────────────────────────────────────

  return {
    // 模块求值完之后：记下钩子，把清单和导出交给宿主核对
    load(ns) {
      const h = { __proto__: null };
      let found = "{";
      for (let i = 0; i < HOOKS.length; i++) {
        const name = HOOKS[i];
        const v = ns[name];
        h[name] = typeof v === "function" ? v : undefined;
        found += (i > 0 ? "," : "") + JSONstringify(name) + ":" + JSONstringify(v === undefined ? "missing" : typeof v);
      }
      found += "}";
      hooks = h;

      const m = ns.manifest;
      const kind = m === undefined ? "missing" : m === null ? "null" : ArrayIsArray(m) ? "array" : typeof m;
      let manifest = "null";
      let error = "null";
      let order = "null";
      if (kind === "object") {
        try {
          const s = JSONstringify(m);
          if (typeof s === "string") manifest = s;
        } catch (e) {
          error = JSONstringify(clip(show(e), 500));
        }
        // 设置项的先后就是界面上的先后，而宿主那边的 JSON 对象不保序
        try {
          const st = m.settings;
          if (st !== null && typeof st === "object" && !ArrayIsArray(st)) {
            const ks = ObjectKeys(st);
            let list = "[";
            for (let i = 0; i < ks.length; i++) list += (i > 0 ? "," : "") + JSONstringify(ks[i]);
            order = list + "]";
          }
        } catch (_) {}
      }
      const def = ns.default;
      return (
        '{"hooks":' + found +
        ',"manifest_kind":' + JSONstringify(kind) +
        ',"manifest":' + manifest +
        ',"manifest_error":' + error +
        ',"settings_order":' + order +
        ',"has_default":' + (def !== undefined ? "true" : "false") +
        "}"
      );
    },

    seed(a, b, c, d) {
      s0 = a | 0;
      s1 = b | 0;
      s2 = c | 0;
      s3 = d | 0;
      if ((s0 | s1 | s2 | s3) === 0) s0 = 1;
    },

    setCtx(text) {
      ctxValue = deepFreeze(JSONparse(text));
    },

    call(kind, input) {
      rejected = null;
      outcome = null;
      current = HOOKS[kind];
      const f = hooks[current];
      if (typeof f !== "function") {
        outcome = { __proto__: null, done: true, ok: false, value: new TypeErrorCtor(current + " is not exported") };
        return;
      }
      let r;
      try {
        if (kind === 0) r = ReflectApply(f, undefined, [JSONparse(input), ctxValue]);
        else if (kind === 1) r = ReflectApply(f, undefined, [input, ctxValue]);
        else if (kind === 2) r = ReflectApply(f, undefined, [ctxValue]);
        else r = ReflectApply(f, undefined, [JSONparse(input), ctxValue]);
      } catch (e) {
        outcome = { __proto__: null, done: true, ok: false, value: e };
        return;
      }
      // async 钩子：结果等 Rust 那边把 Promise 任务跑完再取（settle）
      if (r !== null && typeof r === "object") {
        const o = { __proto__: null, done: false, ok: false, value: undefined };
        try {
          ReflectApply(PromiseThen, r, [
            (v) => {
              o.done = true;
              o.ok = true;
              o.value = v;
            },
            (e) => {
              o.done = true;
              o.value = e;
            },
          ]);
          outcome = o;
          return;
        } catch (_) {
          // 不是 Promise：就是一个普通的返回值
        }
      }
      outcome = { __proto__: null, done: true, ok: true, value: r };
    },

    settle(kind) {
      const o = outcome;
      outcome = null;
      try {
        if (rejected !== null) return REJECTED + rejected;
        if (o === null) return THREW + describe(new ErrorCtor("the hook did not run"));
        if (!o.done) return BAD + HOOKS[kind] + " returned a promise that never settled";
        if (!o.ok) return THREW + describe(o.value);
        return finish(kind, o.value);
      } finally {
        current = null;
      }
    },

    describe,

    describeThrown(e) {
      return THREW + describe(e);
    },
  };

  function deepFreeze(v) {
    if (v !== null && typeof v === "object") {
      ObjectFreeze(v);
      const ks = ObjectKeys(v);
      for (let i = 0; i < ks.length; i++) deepFreeze(v[ks[i]]);
    }
    return v;
  }
})();
