// WSL 路径转换
//
// 在 WSL 里运行的客户端拿到 C:\Users\... 这样的 Windows 路径时找不到文件；在 Windows
// 上运行的客户端拿到 /mnt/c/Users/... 时同样找不到。这个插件把工具调用参数里的路径统一成
// 客户端那一侧的写法：
//
// - 回答里的工具调用：客户端拿到的就是它能用的路径；
// - 对话历史里此前的工具调用：模型看到的始终是同一种写法，接着也按这种写法调用。
//
// 只改整个值就是一个盘符路径的参数（file_path、path 之类），在 /mnt/<盘符>/… 与
// <盘符>:\… 之间换写法，换法写死在这里。命令行里夹带的路径、对话里的文字、工具结果都
// 不改：反斜杠在 shell 里是转义符，文字和文件内容里的路径改了反而误导模型。
//
// 设置只有一个开关：客户端在哪一侧。设置改不出任何别的改写。
//
// 遍历和复制交给辅助函数：tw.toolCalls 逐个交出对话历史里的工具调用，tw.mapStrings 复制
// 参数、把其中每个字符串换一遍（参数里有 __proto__ 这样的键也照常复制）。改的是传入的
// 对象，不用返回；什么都没换时请求原样发出，一个字节都不动。
//
// 权限：messages（对话历史），reply.tool_calls（回答里的工具调用）。reply.tool_calls 是
// 高风险权限：插件能改动模型要执行的操作；改过的工具调用照样经过 Lite 的工具调用审查。
// 设置：客户端运行在 Windows 上（关闭时按客户端在 WSL 里处理）。出错时拒绝这个请求。
//
// manifest 是纯数据，写成 core 改写它时的样子：界面改设置时只换这一段，改出来的和原来
// 只差改了的那一行。
//
// 给人看的文字（名字、说明、设置项的标签）一律英文：界面按插件 id 和设置项的键换成用户
// 的语言。

export const manifest = {
  name: "Convert WSL and Windows paths",
  api: 1,
  description: "Rewrites drive paths in tool-call arguments to the form the client can open (WSL /mnt/c/… or Windows C:\\…), in answers and in the conversation history.",
  permissions: ["messages", "reply.tool_calls"],
  on_error: "reject",
  settings: {
    windows_client: {
      type: "boolean",
      label: "The client runs on Windows (otherwise WSL)",
      value: false,
    },
  },
};

// /mnt/c/Users/me/a.txt
const WSL_PATH = /^\/mnt\/([a-zA-Z])(\/.*)?$/s;
// C:\Users\me\a.txt、C:/Users/me/a.txt
const WINDOWS_PATH = /^([a-zA-Z]):([\\/].*)?$/s;

function convert(value, windows) {
  if (windows) {
    const m = WSL_PATH.exec(value);
    return m ? `${m[1].toUpperCase()}:${(m[2] ?? "\\").replaceAll("/", "\\")}` : value;
  }
  const m = WINDOWS_PATH.exec(value);
  return m ? `/mnt/${m[1].toLowerCase()}${(m[2] ?? "/").replaceAll("\\", "/")}` : value;
}

export function onRequest(req, ctx) {
  const windows = ctx.settings.windows_client === true;
  tw.toolCalls(req, (call) => {
    call.input = tw.mapStrings(call.input, (s) => convert(s, windows));
  });
}

export function onToolCall(call, ctx) {
  const windows = ctx.settings.windows_client === true;
  call.input = tw.mapStrings(call.input, (s) => convert(s, windows));
}
