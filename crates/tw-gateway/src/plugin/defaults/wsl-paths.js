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
// 权限：messages（对话历史），reply.tool_calls（回答里的工具调用）。reply.tool_calls 是
// 高风险权限：插件能改动模型要执行的操作；改过的工具调用照样经过 Lite 的工具调用审查。
// 设置：客户端运行在 Windows 上（关闭时按客户端在 WSL 里处理）。

export const manifest = {
  name: "Convert WSL and Windows paths",
  api: 1,
  description:
    "Rewrites drive paths in tool-call arguments to the form the client can open (WSL /mnt/c/… or Windows C:\\…), in answers and in the conversation history.",
  permissions: ["messages", "reply.tool_calls"],
  settings: {
    windows_client: {
      type: "boolean",
      label: "客户端运行在 Windows 上（关闭时按 WSL 处理）",
      default: false,
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

function rewrite(value, windows) {
  if (typeof value === "string") return convert(value, windows);
  if (Array.isArray(value)) return value.map((item) => rewrite(item, windows));
  if (value !== null && typeof value === "object") {
    // fromEntries 按原样建出每个键，参数里有 __proto__ 这样的键也不会出错
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, rewrite(item, windows)]),
    );
  }
  return value;
}

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

export function onRequest(req, ctx) {
  const windows = ctx.settings.windows_client === true;
  let changed = false;
  for (const m of req.messages) {
    for (const p of m.parts) {
      if (p.type !== "tool_call") continue;
      const input = rewrite(p.input, windows);
      if (!same(input, p.input)) {
        p.input = input;
        changed = true;
      }
    }
  }
  // 没改就不返回：请求原样发出，一个字节都不动
  return changed ? req : undefined;
}

export function onToolCall(call, ctx) {
  const input = rewrite(call.input, ctx.settings.windows_client === true);
  if (same(input, call.input)) return undefined;
  return { id: call.id, name: call.name, input };
}
