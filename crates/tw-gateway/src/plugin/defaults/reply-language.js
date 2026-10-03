// 指定回答语言
//
// 在系统提示词末尾附上一句固定的要求：用设置里的语言回答。附上的这句话每次都一样，
// 上游的提示词缓存照常命中。
//
// 设置里只能写语言的名称（字母、空格、括号和连字符，最多 40 个字符），写不进句子：
// 这句要求是插件定好的，设置改不出别的指令。
//
// 权限：system，只读写系统提示词。
// 设置：回答语言，装上时是简体中文。出错时（比如语言名写得不对）拒绝这个请求。
//
// manifest 是纯数据，写成 core 改写它时的样子：界面改设置时只换这一段，改出来的和原来
// 只差改了的那一行。
//
// 给人看的文字（名字、说明、设置项的标签、抛出的错误）一律英文：界面按插件 id 和设置项
// 的键换成用户的语言，换不了的（抛出的错误）英文也看得懂。设置的值是语言自己的写法，
// 那是值，不是界面上的字。

export const manifest = {
  name: "Answer in a chosen language",
  api: 1,
  description: "Adds a fixed line to the end of the system prompt that asks the model to answer in the language set here.",
  permissions: ["system"],
  on_error: "reject",
  settings: {
    language: { type: "string", label: "Answer language", value: "简体中文" },
  },
};

const NAME = /^[\p{L}\p{M}][\p{L}\p{M} ()\-]{0,39}$/u;

export function onRequest(req, ctx) {
  const language = String(ctx.settings.language ?? "").trim();
  if (!NAME.test(language)) {
    throw new Error(
      "The answer language setting accepts only the name of a language, such as English or Deutsch.",
    );
  }
  const line = `Always respond in ${language}, unless the user explicitly asks for another language.`;
  if (req.system.includes(line)) return undefined;
  req.system = req.system ? `${req.system}\n\n${line}` : line;
  return req;
}
