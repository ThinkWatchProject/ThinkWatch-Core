// 探查：插件拿到的工具调用里有什么。把看到的整个调用编码成码点，塞进参数的
// seen 字段（同 see-request.js）。
// 预期：工具参数里的密钥，插件只看到占位符。
export const manifest = { name: "看工具调用", api: 1, permissions: ["reply.tool_calls"] };

const encode = (s) => Array.from(s, (c) => c.codePointAt(0).toString(16)).join(".");

export function onToolCall(call) {
  return { ...call, input: { ...call.input, seen: encode(JSON.stringify(call)) } };
}
