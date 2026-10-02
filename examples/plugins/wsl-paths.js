// WSL 与 Windows 路径互换
//
// 在 Windows 上运行的客户端收到 /mnt/c/Users/... 这样的 WSL 路径时找不到文件；
// 在 WSL 里运行的客户端收到 C:\Users\... 时同样找不到。这个插件把回答里工具调用
// 参数中的路径改成客户端那一侧的写法。
//
// 只改整个值就是一个路径的参数，例如 file_path、path；命令行里夹带的路径不改：
// 反斜杠在 shell 里是转义符，改了反而出错。
//
// 权限：reply.tool_calls，读写回答里的工具调用。这是高风险权限，插件能改动模型
// 要执行的操作；改过的工具调用照样经过 Lite 的工具调用审查。
// 设置：改成哪一侧的写法，windows 或 wsl。

export const manifest = {
  name: "WSL 与 Windows 路径互换",
  api: 1,
  description: "把回答里工具调用参数中的路径改成客户端那一侧的写法。",
  permissions: ["reply.tool_calls"],
  settings: {
    to: { type: "string", label: "改成（windows 或 wsl）", default: "windows" },
  },
};

// /mnt/c/Users/me/a.txt → C:\Users\me\a.txt
const WSL_PATH = /^\/mnt\/([a-zA-Z])(\/.*)?$/s;
// C:\Users\me\a.txt、C:/Users/me/a.txt → /mnt/c/Users/me/a.txt
const WINDOWS_PATH = /^([a-zA-Z]):([\\/].*)?$/s;

function toWindows(value) {
  const m = WSL_PATH.exec(value);
  if (!m) return value;
  return `${m[1].toUpperCase()}:${(m[2] ?? "\\").replaceAll("/", "\\")}`;
}

function toWsl(value) {
  const m = WINDOWS_PATH.exec(value);
  if (!m) return value;
  return `/mnt/${m[1].toLowerCase()}${(m[2] ?? "/").replaceAll("\\", "/")}`;
}

function rewrite(value, convert) {
  if (typeof value === "string") return convert(value);
  if (Array.isArray(value)) return value.map((item) => rewrite(item, convert));
  if (value !== null && typeof value === "object") {
    // fromEntries 按原样建出每个键，参数里有 __proto__ 这样的键也不会出错
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, rewrite(item, convert)]),
    );
  }
  return value;
}

export function onToolCall(call, ctx) {
  const to = ctx.settings.to ?? "windows";
  if (to !== "windows" && to !== "wsl") {
    throw new Error(`设置「改成」只能是 windows 或 wsl，当前是 ${to}`);
  }
  const input = rewrite(call.input, to === "windows" ? toWindows : toWsl);
  if (JSON.stringify(input) === JSON.stringify(call.input)) return undefined;
  return { id: call.id, name: call.name, input };
}
