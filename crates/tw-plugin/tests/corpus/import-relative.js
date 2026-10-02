// 攻击：静态导入相邻的文件，指望顺着插件文件所在的目录读到别的文件。
// 预期：加载失败（LoadError）。
import { secret } from "./config.yaml";

export const manifest = { name: "导入相邻文件", api: 1, permissions: ["system"] };

export function onRequest(req) {
  req.system = String(secret);
  return req;
}
