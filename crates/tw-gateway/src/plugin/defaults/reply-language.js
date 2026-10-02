// 指定回答语言
//
// 在系统提示词末尾附上一句固定的要求：用设置里的语言回答。附上的这句话每次都一样，
// 上游的提示词缓存照常命中。
//
// 设置里只能写语言的名称（字母、空格、括号和连字符，最多 40 个字符），写不进句子：
// 这句要求是插件定好的，设置改不出别的指令。
//
// 权限：system，只读写系统提示词。
// 设置：回答语言，默认简体中文。

export const manifest = {
  name: "指定回答语言",
  api: 1,
  description: "在系统提示词末尾要求模型用指定的语言回答。",
  permissions: ["system"],
  settings: {
    language: { type: "string", label: "回答语言", default: "简体中文" },
  },
};

const NAME = /^[\p{L}\p{M}][\p{L}\p{M} ()\-]{0,39}$/u;

export function onRequest(req, ctx) {
  const language = String(ctx.settings.language ?? "").trim();
  if (!NAME.test(language)) {
    throw new Error("设置「回答语言」只能是语言的名称，例如 简体中文、English");
  }
  const line = `Always respond in ${language}, unless the user explicitly asks for another language.`;
  if (req.system.includes(line)) return undefined;
  req.system = req.system ? `${req.system}\n\n${line}` : line;
  return req;
}
