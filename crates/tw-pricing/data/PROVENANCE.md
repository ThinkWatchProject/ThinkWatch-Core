# 内置价目表的来历

| | |
|---|---|
| 来源 | https://github.com/BerriAI/litellm — `model_prices_and_context_window.json`（仓库根目录那份） |
| commit | `ee7c7e14f3dd7c4c3930a423440ec26427e2c554` |
| 快照日期 | 2026-09-09 |
| 许可 | MIT（**根目录那份**。`enterprise/` 下的例外不适用于它）|
| 原始大小 | 2343859 字节，gzip 后 110309 字节 |
| 模型数 | 3860 |

## 为什么是它

DESIGN.md §4.3.0 里那个决定性的字段：

```
cache_creation_input_token_cost_above_1hr
```

**它是公开数据集里唯一编码了 Anthropic「1 小时缓存写入」价格的。**其他
候选都只有 5 分钟档 —— Sonnet 4.5 是 5 分钟 $3.75、1 小时 $6.00，用户
开 1 小时 TTL 就会被**系统性低估 60%**。而低估比不知道更糟：它看起来是
个确定的数字。

## 怎么更新

**pin 到具体的 commit SHA，不要用 `main`。**`main` 意味着两次构建
可能拿到不同的价格，而「昨天算出来的钱和今天不一样」是没法解释的。

```bash
# 找到最新的 commit
curl -s "https://api.github.com/repos/BerriAI/litellm/commits?path=model_prices_and_context_window.json&per_page=1"
# 拉那一版并压缩
curl -sL "https://raw.githubusercontent.com/BerriAI/litellm/<SHA>/model_prices_and_context_window.json" \
  | gzip -9 > crates/tw-pricing/data/model_prices.json.gz
```

然后**更新这个文件里的 SHA 和日期**，并跑发版前的双源交叉校验（§4.3.0）：
对十几个主力模型，逐条比对**厂商官方定价页** —— 不是和另一个第三方数据集
对，那只是把赌注换个地方押。

一个实测到的例子：OpenAI 在 2026-08-22 下调了某个模型的价格，某个广泛
使用的数据集**三天后才修正**，中间一直返回偏高 25% 的价格。根因是那家的
同步脚本只自动同步模型清单，价格靠人工 PR。
