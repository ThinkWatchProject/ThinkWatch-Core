// DeepSeek：替换会被拒收的旗帜表情
//
// DeepSeek 接口会拒收含特定地区旗帜表情的请求：模型还没运行就回 400 Content Exists Risk。
// 这类表情一旦进入对话历史（例如工具抓回的网页、读到的文件），之后这个会话的每一次请求
// 都会被拒收，会话就无法继续（deepseek-ai/deepseek-harness 讨论 #7310，DeepSeek Harness
// 与 OpenCode 上都能复现）。
//
// - 请求：系统提示词和对话消息里（含工具结果、此前工具调用的参数）出现这些表情时，换成
//   一段 ASCII 占位文字。占位文字不会自然出现，经过 JSON 转义也保持原样。
// - 回答：回答文字和工具调用参数里出现占位文字时换回原来的表情，客户端写出的文件里仍是
//   原样。逐段模式下，末尾可能是占位文字开头的几个字先扣住，等下一段到了再判断。
//
// 换哪些字符、换成什么写死在这里，没有设置项：插件持有 reply.tool_calls，替换不能被设置
// 引向别处。同样的输入每次换出同样的结果，上游的提示词缓存照常命中；请求里没有这些表情时
// 原样发出，一个字节都不动。
//
// 适用范围默认是发往上游的模型名以 deepseek 开头的请求（路由改写模型名之后的那个名字），
// 客户端用别的名字、经路由转到 DeepSeek 的请求也在范围内。
//
// 思考内容只读，其中的表情换不掉。
//
// 权限：system、messages（请求一侧替换），reply.text、reply.tool_calls（回答一侧换回）。

export const manifest = {
  name: "DeepSeek：替换会被拒收的旗帜表情",
  api: 1,
  description:
    "DeepSeek 接口会拒收含特定地区旗帜表情的请求，含有它们的会话因此无法继续。请求发出前把这些表情换成 ASCII 占位文字，回答里再换回原样。默认对发往 deepseek 开头的模型的请求生效。",
  permissions: ["system", "messages", "reply.text", "reply.tool_calls"],
  match: { models: ["deepseek*"] },
  reply: "stream",
};

// 被拒收的字符序列，和各自的占位文字
const SEQUENCES = [
  // U+1F1F9 U+1F1FC
  { chars: String.fromCodePoint(0x1f1f9, 0x1f1fc), placeholder: "[[emoji:1F1F9-1F1FC]]" },
];

function hide(s) {
  let out = s;
  for (const { chars, placeholder } of SEQUENCES) out = out.replaceAll(chars, placeholder);
  return out;
}

function reveal(s) {
  let out = s;
  for (const { chars, placeholder } of SEQUENCES) out = out.replaceAll(placeholder, chars);
  return out;
}

// JSON 值里的每个字符串（连同对象的键）
function deep(value, f) {
  if (typeof value === "string") return f(value);
  if (Array.isArray(value)) return value.map((item) => deep(item, f));
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [f(k), deep(v, f)]));
  }
  return value;
}

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

export function onRequest(req) {
  let changed = false;
  const fix = (s) => {
    const next = hide(s);
    if (next !== s) changed = true;
    return next;
  };
  req.system = fix(req.system);
  for (const m of req.messages) {
    for (const p of m.parts) {
      if (p.type === "text" || p.type === "tool_result") {
        p.text = fix(p.text);
      } else if (p.type === "tool_call") {
        const input = deep(p.input, hide);
        if (!same(input, p.input)) {
          p.input = input;
          changed = true;
        }
      }
    }
  }
  // 没有要换的就不返回：请求原样发出
  return changed ? req : undefined;
}

// 同一个回答里的几次调用共用一个实例：held 是上一段末尾扣住的、可能是占位文字开头的那几个字
let held = "";

export function onReplyText(text) {
  const s = reveal(held + text);
  let keep = 0;
  for (const { placeholder } of SEQUENCES) {
    for (let k = Math.min(placeholder.length - 1, s.length); k > keep; k--) {
      if (placeholder.startsWith(s.slice(s.length - k))) {
        keep = k;
        break;
      }
    }
  }
  held = s.slice(s.length - keep);
  const out = s.slice(0, s.length - keep);
  return out === text ? undefined : out;
}

export function onReplyTextEnd() {
  const out = held;
  held = "";
  return out === "" ? undefined : out;
}

export function onToolCall(call) {
  const input = deep(call.input, reveal);
  if (same(input, call.input)) return undefined;
  return { id: call.id, name: call.name, input };
}
