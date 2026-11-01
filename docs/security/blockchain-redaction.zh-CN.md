# UUID、区块链凭证与账户隐私过滤

共享过滤引擎按编码结构和字段位置识别机密。UUID、裸十六进制和链上账户同时作为隐私数据处理：默认启用、默认替换，包括普通订单和追踪 UUID。整体模式为 `observe` 时只记录，`enforce` 时执行规则配置的动作；单条规则仍可关闭、设为仅记录或添加值例外。

规则名称、说明和开关由 Core 的双语目录提供，桌面版和企业版直接读取该目录。

## 钱包与标识符

| 规则 | 范围与校验 |
|---|---|
| `uuid` | 标准 `8-4-4-4-12` 和紧凑 32 位十六进制；大小写均可，不限制版本或 variant；同形 MD5 也替换，保留花括号、URN、URL 路径和文件扩展名 |
| `crypto-hex-key` | 64 位十六进制，可带 `0x`；覆盖 Ethereum、BNB、Polygon、Base、TRON 等裸私钥，以及无法按外观区分的 32 字节交易哈希和摘要 |
| `crypto-mnemonic` | 既有 BIP39 英文、简体中文词表及校验和检测 |
| `crypto-wif-key` | Bitcoin 主网／测试网 WIF；Base58Check、版本、长度、压缩标记和 secp256k1 标量校验 |
| `crypto-extended-key` | BIP32／SLIP-0132 的 xprv、tprv、yprv、Yprv、zprv、Zprv、uprv、Uprv、vprv、Vprv；校验和、私钥标记和标量校验 |
| `crypto-solana-key` | 64 字节 seed＋公钥的 Base58 和作为文本粘贴的 JSON 数组；派生 Ed25519 公钥核对后一半，避免将同长度签名识别为私钥 |
| `crypto-sui-key` | suiprivkey Bech32；HRP、校验和、签名方案 0／1／2 和 32 字节密钥 |
| `crypto-stellar-key` | S 开头 StrKey 种子；版本、32 字节载荷及 CRC16-XModem |
| `crypto-xrp-seed` | XRP 字母表 Base58Check；secp256k1 family seed 和 Ed25519 seed 版本 |
| `crypto-address` | EVM、Bitcoin、TRON、XRP、Stellar 地址，Bitcoin 隔离见证地址，BIP32／SLIP-0132 扩展公钥；另覆盖无校验和的 32 字节 Base58 账户格式（如 Solana） |

`crypto-secret-field` 补充没有稳定编码的机密：privateKey／private_key、secretKey、pk、seed、mnemonic、seedPhrase、recoveryPhrase、passphrase、walletSecret、keySecret、恢复码、TOTP secret、MPC／备份份额及相应中文标签。支持环境变量前缀，例如 WALLET_PRIVATE_KEY、SIGNER_PRIVATE_KEY 和 PRIVY_WALLET_PRIVATE_KEY。按字段识别时不依赖随机度或固定 key 长度，带引号的短口令、纯字母口令、部分／拼错的助记词也保护。

`crypto-keystore` 识别 keystore 和 ciphertext 字段，包括 Ethereum V3 加密密钥材料。作为聊天文本粘贴的密钥数组和 keystore 对象可整体替换。

完整钱包密钥和新增凭证字段的安全记录完全打码，不保留私钥片段。UUID、哈希和公开地址本身不会触发工具审查中的“密钥发往陌生主机”；有明确私钥编码的材料保留该保护。

## 交易平台与签名鉴权

不透明 API key 通常没有可通用验证的前缀、长度或字符集。`exchange-credential` 与 `crypto-secret-field` 按 SDK／配置字段、请求头和环境变量识别，只替换值，保留名称和周围结构。

| 平台 | 典型字段／请求头 |
|---|---|
| Binance | X-MBX-APIKEY、apiKey、secretKey；RSA／Ed25519 PEM 由既有私钥块规则覆盖 |
| OKX | OK-ACCESS-KEY、OK-ACCESS-PASSPHRASE、OK-ACCESS-SIGN、SecretKey |
| Bybit | X-BAPI-API-KEY、X-BAPI-SIGN、apiSecret |
| Coinbase Exchange／CDP | CB-ACCESS-KEY、CB-ACCESS-PASSPHRASE、CB-ACCESS-SIGN、KEY_SECRET、WALLET_SECRET、X-Wallet-Auth |
| Kraken | API-Key、API-Sign、secret、OTP |
| KuCoin | KC-API-KEY、KC-API-PASSPHRASE、KC-API-SIGN |
| Bitget | ACCESS-KEY、ACCESS-PASSPHRASE、ACCESS-SIGN |
| Gate／Bitfinex／HTX | KEY、SIGN、API-KEY、API-SECRET、AccessKeyId、SecretKey |
| MEXC／Gemini | X-MEXC-APIKEY、X-GEMINI-APIKEY、X-GEMINI-PAYLOAD、X-GEMINI-SIGNATURE |
| BitMart／Phemex／Bitstamp | X-BM-KEY、X-BM-SIGN、API_MEMO、x-phemex-access-token、X-Auth 的 BITSTAMP key、X-Auth-Signature |
| Polymarket CLOB | apiKey、secret、passphrase；POLY_API_KEY、POLY_PASSPHRASE、POLY_SIGNATURE |
| Polymarket Builder／Relayer | key、secret、passphrase；POLY_BUILDER_API_KEY、POLY_BUILDER_PASSPHRASE、POLY_BUILDER_SIGNATURE、POLYMARKET_BUILDER_SECRET、RELAYER_API_KEY |
| predict.fun | x-api-key、PREDICT_API_KEY、WALLET_PRIVATE_KEY、PRIVY_WALLET_PRIVATE_KEY、signature；Bearer JWT 使用既有 JWT 规则 |
| 其他签名交易服务 | Kalshi 的 ACCESS-KEY／SIGNATURE 与 PEM；Hyperliquid、Lighter、dYdX 等使用明确的 secret_key／privateKey／apiKey 字段时由共享字段规则覆盖 |

Polymarket CLOB 的 UUID key 同时命中 UUID 规则，secret 和 passphrase 分别按字段保护。predict.fun 的 API key 不假定为 UUID。签名材料按字段保护；格式检测不会验证交易是否仍可执行。

泛字段 key、secret、password、memo、sign、OTP 中的字面量也会替换，因此公开充值 memo 或代码中的普通 key 字面量可能被隐藏。变量引用、模板、调用和类型声明尽量保留；值例外可用于明确需要发送的公开数据。

`crypto-rpc-key` 保护 Infura `/v3/`、Alchemy `/v2/`、QuickNode 令牌路径和 Helius `api-key` URL 中的凭据部分；其他 RPC 服务的命名 API key、Authorization 和 URL token 继续使用通用规则。

## 识别边界

- 扫描协议定义的文本、工具参数和工具结果范围；协议结构中的模型名、调用 ID、签名字段和媒体载荷沿用既有范围约束。
- JSON 数值和结构分隔符不直接替换。作为实际工具参数传递的数值型密钥数组不在文本替换范围；作为消息文本粘贴的数组可保护。此约束保证请求仍是合法 JSON。
- 无标签的短口令、32 字节 Base64 seed、未知格式的密钥、加密备份及未支持语言的助记词，不能保证仅按外观识别。使用明确字段名或保存凭证可补充精确匹配。截图不做 OCR。
- 过滤是可逆占位符替换；回答回到当前调用方时按账本还原。审计正文落盘保护另受对应产品的存储配置控制。

## 格式依据

- [BIP39](https://github.com/bitcoin/bips/blob/master/bip-0039.mediawiki)、[BIP32](https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki)、[SLIP-0132](https://github.com/satoshilabs/slips/blob/master/slip-0132.md)
- [Solana keypair](https://github.com/anza-xyz/solana-sdk/blob/master/keypair/src/lib.rs)、[Sui SIP-15](https://github.com/sui-foundation/sips/blob/main/sips/sip-15.md)
- [Stellar StrKey](https://github.com/stellar/rs-stellar-strkey/blob/main/src/strkey.rs)、[XRP cryptographic keys](https://xrpl.org/docs/concepts/accounts/cryptographic-keys)
- [Ethereum V3 keystore](https://ethereum.org/en/developers/docs/data-structures-and-encoding/web3-secret-storage/)
- [Polymarket CLOB OpenAPI](https://docs.polymarket.com/api-spec/clob-openapi.yaml)、[predict.fun API](https://api.predict.fun/docs)、[predict.fun SDK](https://github.com/PredictDotFun/sdk/blob/main/README.md)
- [Binance authentication](https://developers.binance.com/docs/binance-spot-api-docs/rest-api/request-security)、[OKX API](https://www.okx.com/docs-v5/en/#overview-rest-authentication)、[Bybit authentication](https://bybit-exchange.github.io/docs/v5/guide)
- [Coinbase Exchange authentication](https://docs.cdp.coinbase.com/exchange/rest-api/authentication)、[Kraken authentication](https://docs.kraken.com/api/docs/guides/spot-rest-auth/)、[KuCoin authentication](https://www.kucoin.com/docs-new/authentication)
