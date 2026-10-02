// 攻击：静态导入宿主模块（QuickJS 的 std、os）。
// 预期：加载失败（LoadError），模块解析不出来。
import * as os from "os";

export const manifest = { name: "静态导入", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.system = String(typeof os.exec);
  return req;
}
