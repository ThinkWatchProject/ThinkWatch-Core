#!/usr/bin/env bash
# 从零起，把每条路都走一遍。
#
# 单元测试证明不了这些，因为它们的失败模式全在**接缝上**：文件权限、
# socket 路径长度、配置里写错一个字段名被静默吞掉、某个端点在真二进制
# 上根本没注册。这个项目的前四个真 bug 就是这么被逮到的。
#
# 用法：scripts/smoke.sh
#
# **它不碰你自己的任何东西**：HOME 和 THINKWATCH_HOME 都指向一个临时
# 目录，跑完就删。
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
FAKE_HOME="$TMP/home"
export HOME="$FAKE_HOME"
export THINKWATCH_HOME="$FAKE_HOME/.thinkwatch"
mkdir -p "$THINKWATCH_HOME"
SOCK="$THINKWATCH_HOME/twcore.sock"
# 控制面的每条连接先握手（钥匙在 config.yaml 的 listen.control.key），curl
# 敲不开它。`twcore call` 读同一份配置里的钥匙、走和桌面端同一条握手。
# 带 --out 时正文写进文件、只打印状态码，和原来 curl -o -w 的用法一样
ctl() { "$BIN" --config "$CFG" call "$@"; }
PORT=18999
UPPORT=18998
PASS=0; FAIL=0

WARN=0
ok()   { PASS=$((PASS+1)); printf '  ✓ %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  ✗ %s\n' "$1"; [ $# -gt 1 ] && printf '      %s\n' "$2"; }
# **超出目标但不算回归**的那一档。
#
# 只有 ✓ 和 ✗ 两档时，一个「35MB，而目标是 30MB」只能二选一：打勾等于
# 给一个没达标的数字盖章，打叉等于让 CI 为一个没变坏的事实一直红着。
# 两个都会让人停止看这一行。
warn() { WARN=$((WARN+1)); printf '  ⚠ %s\n' "$1"; [ $# -gt 1 ] && printf '      %s\n' "$2"; }
step() { printf '\n== %s\n' "$1"; }
# 权限位、修改时间 + 大小。**GNU 先问**：BSD 的 `stat -f` 在 GNU 上是「查文件
# 系统」，同一串参数照样返回 0、吐出一行不相干的东西，而不是失败退到下一种。
mode_of()  { stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1" 2>/dev/null; }
mtime_size() { stat -c '%Y %s' "$1" 2>/dev/null || stat -f '%m %z' "$1" 2>/dev/null; }

cleanup() {
  [ -n "${CORE_PID:-}" ] && kill "$CORE_PID" 2>/dev/null
  wait "${CORE_PID:-0}" 2>/dev/null
  [ -n "${UP_PID:-}" ] && kill "$UP_PID" 2>/dev/null
  rm -rf "$TMP"
}
trap cleanup EXIT

# ---------------------------------------------------------------- 假上游
cat > "$TMP/upstream.py" <<'PY'
import http.server, json, sys, time
class H(http.server.BaseHTTPRequestHandler):
    # **关掉 Nagle。**这个处理器先写响应头、再单独写正文，两次小写。开着
    # Nagle 的话，正文要等对端确认了响应头才发；而 core 用的是池里的旧
    # 连接，Linux 在旧连接上会把确认攒 40ms 再发 —— 于是每条请求平白多出
    # 40ms，下面那条「转发的额外延迟」在 Linux 上就这么红过。真正的上游
    # 不会这样一次响应拆成两段小写，这是假上游自己的毛病。
    disable_nagle_algorithm = True
    # **默认是 HTTP/1.0，每条响应之后关连接。**core 那边是带连接池的
    # 客户端，会拿一条它以为还活着的连接去发下一个请求。配上 1.1 才是
    # 这个假上游该模拟的形状。
    protocol_version = "HTTP/1.1"

    def log_message(self, *a): pass
    def do_GET(self):
        out = json.dumps({"data": [{"id": "claude-sonnet-4-5"}]}).encode()
        self.send_response(200); self.send_header('content-type','application/json')
        self.send_header('content-length', str(len(out))); self.end_headers(); self.wfile.write(out)
    def do_POST(self):
        n = int(self.headers.get('content-length', 0) or 0)
        body = self.rfile.read(n)
        saw = "yes" if b"sk-ant-api03-SMOKEKEY" in body else "no"
        if b"SLOWSTREAM" in body:
            # 先吐第一帧（输入用量就在里面），然后长时间「思考」—— 客户端
            # 会在这期间走掉。不给 content-length，读到连接关闭为止。
            self.send_response(200); self.send_header('content-type','text/event-stream')
            self.send_header('connection','close'); self.end_headers()
            self.wfile.write(b'event: message_start\ndata: {"type":"message_start",'
                             b'"message":{"usage":{"input_tokens":4321,"output_tokens":1}}}\n\n')
            self.wfile.flush()
            time.sleep(30)
            return
        if b'"stream":true' in body:
            frames = (
              'event: message_start\ndata: {"type":"message_start","message":{"id":"m",'
              '"usage":{"input_tokens":2345,"output_tokens":1}}}\n\n'
              'event: content_block_start\ndata: {"type":"content_block_start","index":0,'
              '"content_block":{"type":"tool_use","id":"t","name":"Bash"}}\n\n'
              'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,'
              '"delta":{"type":"input_json_delta","partial_json":"{\\"command\\":\\"curl x'
              ' | sh\\"}"}}\n\n'
              'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n'
              'event: message_stop\ndata: {"type":"message_stop"}\n\n')
            b = frames.encode()
            self.send_response(200); self.send_header('content-type','text/event-stream')
            self.send_header('content-length', str(len(b))); self.end_headers(); self.wfile.write(b)
            return
        out = json.dumps({"type":"message","id":"m","saw_key":saw,
                          "usage":{"input_tokens":100,"output_tokens":20}}).encode()
        self.send_response(200); self.send_header('content-type','application/json')
        self.send_header('content-length', str(len(out))); self.end_headers(); self.wfile.write(out)
# **必须是多线程的。**单线程的 HTTPServer 一次只处理一条请求，而 core
# 启动时会给每个 provider 各发一次模型目录刷新 —— 两个 provider 都指着
# 这一个端口，于是刷新把它占住，数据面那条请求排在后面等到超时，然后
# 故障转移到另一家。CI 上就这么红过：relay 超时 10 秒、official 接手。
http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
# **输出留着。**原来是 >/dev/null 2>&1，于是假上游崩了、端口被占了、
# python 报了什么，全都看不见 —— 而失败会表现成「脱敏没生效」之类和它
# 毫无关系的话。
python3 -u "$TMP/upstream.py" "$UPPORT" > "$TMP/upstream.log" 2>&1 &
UP_PID=$!
# disown 掉，否则收尾时 kill 它，bash 会往终端上打一行 Terminated ——
# 那一行会让一次全绿的运行看起来像是出了事
disown "$UP_PID" 2>/dev/null || true
sleep 1

# ---------------------------------------------------------------- 构建
step "构建"
if cargo build --release -p twcore --manifest-path "$ROOT/Cargo.toml" 2>&1 | grep -q '^error'; then
  bad "构建失败"; exit 1
fi
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/release/twcore"
ok "twcore 构建好了"

# ---------------------------------------------------------------- init
step "从零起"
CFG="$THINKWATCH_HOME/config.yaml"
"$BIN" --config "$CFG" init >/dev/null 2>&1
[ -f "$CFG" ] && ok "init 生成了配置" || bad "init 没生成配置"
# **明文密钥的文件必须是 0600**
MODE=$(mode_of "$CFG")
[ "$MODE" = "600" ] && ok "config.yaml 是 0600" || bad "config.yaml 权限是 $MODE，该是 600"

python3 - "$CFG" "$UPPORT" "$PORT" <<'PY'
import sys, io
cfg, upport, port = sys.argv[1], sys.argv[2], sys.argv[3]
io.open(cfg, 'w', encoding='utf-8').write(f"""version: 1
listen:
  gateway:
    port: {port}
clients:
  - name: claude-code
    key: tw-smoketestkey0123456789
providers:
  - name: relay
    base_url: http://127.0.0.1:{upport}
    key: sk-upstream-smoke
    protocol: anthropic
  - name: official
    base_url: http://127.0.0.1:{upport}
    key: sk-upstream-smoke
    protocol: anthropic
# **把流量钉在 relay 上。**脱敏是全局的，走哪家都会换；钉住是为了让
# 「这一条走了哪条路由」也有一个确定的答案，失败时少猜一件事。
#
# official 留着不是摆设：模型目录刷新、/overview 的上游计数都要它，而
# 那两处正是「多上游」才盖得到的路径。
groups:
  - name: 只走中转
    type: fallback
    providers: [relay]
routes:
  - name: 默认
    rules:
      - name: 冒烟：这条必须走 relay，不许转移
        to: 只走中转
security:
  redact:
    mode: enforce
  inspect_tools:
    mode: enforce
""")
PY
# 这份配置故意没写控制面的钥匙：老用户的配置就是这样。check 要认得它（钥匙
# 由 serve 补上），serve 起来之后要只多出钥匙那几行
"$BIN" --config "$CFG" check >/dev/null 2>&1 && ok "check 认得这份配置" || bad "check 不认这份配置"
cp "$CFG" "$TMP/config.before"

# ---------------------------------------------------------------- 起服务
step "起服务"
# **先确认假上游在应答，再起 core。**不确认的话，上游没起来会表现成
# 数据面那几条断言失败 —— 而那些话说的是脱敏、是路由，没有一句指向真
# 正的原因。一个不检查自己前提的测试，会把失败报在错的地方。
UP_OK=""
for _ in $(seq 1 40); do
  if curl -sf -m 2 "http://127.0.0.1:$UPPORT/v1/models" >/dev/null 2>&1; then UP_OK=1; break; fi
  sleep 0.25
done
if [ -n "$UP_OK" ]; then
  ok "假上游在应答"
else
  bad "假上游没应答（端口 $UPPORT）" "$(tail -5 "$TMP/upstream.log" 2>/dev/null)"
  exit 1
fi
"$BIN" --config "$CFG" serve > "$TMP/core.log" 2>&1 &
CORE_PID=$!
for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
[ -S "$SOCK" ] && ok "控制面 socket 起来了" || { bad "socket 没出现" "$(tail -3 "$TMP/core.log")"; exit 1; }
# **钥匙在监听之前就写好了**：socket 出现的那一刻，配置里已经有它
KEY=$(python3 - "$CFG" <<'PY'
import re, sys
m = re.search(r'^    key: ([0-9a-f]{64})$', open(sys.argv[1], encoding='utf-8').read(), re.M)
print(m.group(1) if m else "")
PY
)
[ -n "$KEY" ] && ok "serve 给没有钥匙的配置补上了钥匙" || bad "配置里没有钥匙" "$(head -12 "$CFG")"
ADDED=$(diff "$TMP/config.before" "$CFG" | grep -c '^>')
REMOVED=$(diff "$TMP/config.before" "$CFG" | grep -c '^<')
[ "$ADDED" = 2 ] && [ "$REMOVED" = 0 ] && ok "只多出钥匙那两行，别的一个字节没动" \
  || bad "补钥匙改动了别的地方" "$(diff "$TMP/config.before" "$CFG" | head -8)"
[ "$("$BIN" --config "$CFG" control-key)" = "$KEY" ] && ok "control-key 打印的就是这把" || bad "control-key 打印的不是配置里那把"

MODE=$(mode_of "$THINKWATCH_HOME/data.db" || echo -)
[ "$MODE" = "600" ] && ok "data.db 是 0600" || bad "data.db 权限是 $MODE"

# ---------------------------------------------------------------- 数据面
step "数据面"
BODY='{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{"role":"user","content":"我的 key 是 sk-ant-api03-SMOKEKEYAAAAAAAAAAAAAAAA"}]}'
R=$(curl -s -XPOST "http://127.0.0.1:$PORT/v1/messages" -H 'x-api-key: tw-smoketestkey0123456789' \
      -H 'content-type: application/json' -d "$BODY")
if echo "$R" | grep -q '"saw_key": *"no"'; then
  ok "拦截档下密钥被换成了占位符"
else
  # **一条安全检查失败时必须说出它为什么失败。**「上游看见了真 key」
  # 只说了结果，而下一步取决于原因：脱敏没配上、规则没认出来、还是体
  # 根本没被当成 UTF-8。所以把这一次的路由决策和 core 日志一起交出来 ——
  # 少了这些，CI 上的一次失败在本机复现不出来就只能靠猜。
  sleep 0.5
  CHAIN=$(ctl "/history?limit=1" 2>/dev/null \
            | python3 -c 'import sys, json
# /history 是个顶层数组。字段名在演化，所以打印整行而不是挑几个 ——
# 挑错了名字就什么都看不到，而这段代码只在出事那一次跑。
try:
    rows = json.load(sys.stdin)
    if not rows:
        print("这次请求没落到 /history 里（观测通道可能还没写完）")
    else:
        print(json.dumps(rows[0], ensure_ascii=False, sort_keys=True))
except Exception as e:
    print("取不到路由信息：%s" % e)' 2>/dev/null)
  bad "上游看见了真 key" "$R"
  printf '      %s\n' "${CHAIN:-（控制面没给出路由信息）}"
  printf '      core.log 末尾：\n'
  sed 's/^/        /' <<<"$(tail -8 "$TMP/core.log")"
  printf '      假上游日志末尾：\n'
  sed 's/^/        /' <<<"$(tail -5 "$TMP/upstream.log" 2>/dev/null)"
fi

R=$(curl -s -XPOST "http://127.0.0.1:$PORT/v1/messages" -H 'x-api-key: tw-smoketestkey0123456789' \
      -H 'content-type: application/json' -H 'x-thinkwatch-client: smoke' \
      -d "${BODY/relay/relay}")
echo "$R" | grep -q 'saw_key' && ok "第二条请求也通了" || bad "第二条请求失败" "$R"

# 没带密钥要被挡住
CODE=$(curl -s -o /dev/null -w '%{http_code}' -XPOST "http://127.0.0.1:$PORT/v1/messages" \
        -H 'content-type: application/json' -d "$BODY")
[ "$CODE" = "401" ] && ok "没带密钥被挡住了（401）" || bad "没带密钥居然返回 $CODE"

# 流式 + 工具调用防火墙
S=$(curl -s -XPOST "http://127.0.0.1:$PORT/v1/messages" -H 'x-api-key: tw-smoketestkey0123456789' \
      -H 'content-type: application/json' \
      -d '{"model":"claude-sonnet-4-5","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}')
echo "$S" | grep -q 'event: error' && ok "高危工具调用被切断了" || bad "没切断" "$S"
echo "$S" | grep -q 'content_block_stop' && bad "切断之后还发了 content_block_stop" || ok "客户端拿到的工具调用是残的"
# **切断之前上游已经为输入计了费。**这一行要带着那笔钱落库 —— 以前失败的
# 行一律没有用量，这笔钱就不在账上。
GOT=""
for _ in $(seq 1 20); do
  GOT=$(ctl "/history?limit=1" 2>/dev/null \
          | python3 -c 'import sys, json
rows = json.load(sys.stdin)
r = rows[0] if rows else {}
good = ("was cut off" in ((r.get("error") or {}).get("text") or "") and r.get("input_tokens") == 2345
        and r.get("cost_micros") is not None and r.get("cost_estimated") is True)
print("ok" if good else json.dumps(r, ensure_ascii=False, sort_keys=True))' 2>/dev/null)
  [ "$GOT" = "ok" ] && break
  sleep 0.25
done
[ "$GOT" = "ok" ] && ok "被切断的请求落了库：带着切断之前的输入用量和估算金额" \
  || bad "被切断的请求没有带着用量落库" "$GOT"

# 客户端中途走掉（Claude Code 里按 Esc）。
#
# **这一行以前不落库。**流末尾报结局的代码在这条路径上一行都不执行，而
# 上游已经为输入计了费。网关的测试里那个服务是测试自己起的；这里验的是
# 真二进制上「客户端断开 → 事件 → 落库 → /history」整条链通不通。
curl -s -N -m 2 -XPOST "http://127.0.0.1:$PORT/v1/messages" -H 'x-api-key: tw-smoketestkey0123456789' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-4-5","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"SLOWSTREAM"}]}' \
  > "$TMP/cancelled.out" 2>/dev/null
if ! grep -q 'message_start' "$TMP/cancelled.out"; then
  bad "断开之前连第一帧都没收到 —— 这一条测的不是「中途」走掉" "$(head -c 300 "$TMP/cancelled.out")"
else
  GOT=""
  # 落库是异步的：事件先过广播，再由存储层的任务写进去
  for _ in $(seq 1 20); do
    GOT=$(ctl "/history?limit=1" 2>/dev/null \
            | python3 -c 'import sys, json
rows = json.load(sys.stdin)
r = rows[0] if rows else {}
good = (r.get("cancelled") is True and r.get("error") is None
        and r.get("input_tokens") == 4321
        and r.get("cost_micros") is not None and r.get("cost_estimated") is True)
print("ok" if good else json.dumps(r, ensure_ascii=False, sort_keys=True))' 2>/dev/null)
    [ "$GOT" = "ok" ] && break
    sleep 0.25
  done
  [ "$GOT" = "ok" ] && ok "客户端中途走掉的请求落了库：标着取消，带着输入用量和估算金额" \
    || bad "客户端中途走掉的请求没有按取消落库" "$GOT"
fi

# ---------------------------------------------------------------- 控制面
step "控制面要握手"
sleep 1
# **只有这里能证明门是真的。**单元测试测的是握手本身，而「它到底有没有挡在
# 真正对外的那个 socket 前面」只有真二进制加真 socket 答得出来 —— 漏挂的
# 样子是所有测试照常通过，而控制面对整台机器敞着。
PLAIN=$(curl -s -m 5 -o "$TMP/plain" -w '%{http_code}' --unix-socket "$SOCK" http://localhost/status)
if [ "$PLAIN" != 200 ] && ! grep -q api_version "$TMP/plain" 2>/dev/null; then
  ok "不握手的 HTTP 进不来"
else
  bad "不握手的 HTTP 竟然进去了" "$PLAIN $(head -c 200 "$TMP/plain")"
fi
# 钥匙不对：另一份配置、同一个 socket
mkdir -p "$TMP/wrong"
printf 'listen:\n  control:\n    key: %s\n' "$(printf '0%.0s' $(seq 1 64))" > "$TMP/wrong/config.yaml"
ln -s "$SOCK" "$TMP/wrong/twcore.sock"
if "$BIN" --config "$TMP/wrong/config.yaml" call /status > "$TMP/wrong/out" 2> "$TMP/wrong/err"; then
  bad "钥匙不对竟然进去了" "$(head -c 200 "$TMP/wrong/out")"
else
  grep -q "does not match" "$TMP/wrong/err" && ok "钥匙不对被拒，而且说的是钥匙不对" \
    || bad "钥匙不对被拒，但说法不对" "$(cat "$TMP/wrong/err")"
fi
ctl /status | grep -q '"api_version"' && ok "钥匙对了就进得来" || bad "拿着对的钥匙也进不来"

step "控制面（每个端点）"
get() { ctl --out "$TMP/out" "$1"; }
post() { ctl --out "$TMP/out" -X POST -d "$2" "$1"; }

# **带上时间窗再打一次。**不带参数时一切正常、带上 `from_ms` 就 400，
# 是这两个端点真实发生过的形态：`#[serde(flatten)]` 让 serde 走
# deserialize_any，而 query string 里一切都是字符串，于是 i64 永远解析
# 失败。单元测试看不见 —— 它只在真的经过一次 query string 解析时发生，
# 而界面恰恰总是带着时间窗调。
NOW=$(python3 -c 'import time;print(int(time.time()*1000))')
DAY=$((NOW - 86400000))
for ep in "/summary?from_ms=$DAY" "/summary/buckets?from_ms=$DAY&bucket_ms=3600000" \
          "/summary/by?dim=model&from_ms=$DAY" "/history?limit=5&from_ms=$DAY"; do
  C=$(get "$ep")
  [ "$C" = "200" ] && ok "GET ${ep%%\?*}（带时间窗）" || bad "GET $ep 返回 $C" "$(cat "$TMP/out" 2>/dev/null | head -c 200)"
done

for ep in /status /overview /summary /history /latency /latency/provider /storage /quota /security \
          /security/events /sessions /diagnostics /config /config/history /models; do
  C=$(get "$ep")
  [ "$C" = "200" ] && ok "GET $ep" || bad "GET $ep 返回 $C"
done

# 试算要说清按哪条路由算：不指定密钥或路由时是 400，不再悄悄取第一把密钥
C=$(post /dryrun '{"model":"claude-sonnet-4-5","route":"默认"}'); [ "$C" = "200" ] && ok "POST /dryrun" || bad "POST /dryrun 返回 $C"
# 页面打开时补问模型清单：立刻返回开始问的那几家，不等上游回话
C=$(post /models/refresh '{}'); [ "$C" = "200" ] && ok "POST /models/refresh" || bad "POST /models/refresh 返回 $C"
C=$(ctl /overview | python3 -c 'import json,sys;p=json.load(sys.stdin)["providers"][0];print(p["model_status"] in ("pending","listed","no_list","failed") and isinstance(p["model_fetching"],bool))')
[ "$C" = "True" ] && ok "/overview 带模型获取状态" || bad "/overview 的模型状态字段不对：$C"

ID=$(ctl "/history?limit=1" \
      | python3 -c 'import json,sys;d=json.load(sys.stdin);print(d[0]["id"] if d else 0)')
if [ "$ID" != "0" ]; then
  C=$(get "/request/$ID"); [ "$C" = "200" ] && ok "GET /request/{id}" || bad "返回 $C"
  C=$(post /replay/quote "{\"id\":$ID,\"provider\":\"official\"}")
  [ "$C" = "200" ] && ok "POST /replay/quote" || bad "返回 $C"
else
  bad "历史里一条记录都没有"
fi

# ---------------------------------------------------------------- 诊断包
step "诊断包不带密钥出门"
get /diagnostics >/dev/null
if grep -qE "sk-upstream-smoke|tw-smoketestkey0123456789|$KEY" "$TMP/out"; then
  bad "诊断包里有真密钥"
else
  ok "诊断包里没有真密钥"
fi
# 控制面的钥匙不从控制面出去：编辑器拿到的原文里是打码
get /config >/dev/null
if grep -q "$KEY" "$TMP/out"; then
  bad "GET /config 带出了控制面的钥匙"
else
  ok "GET /config 里的钥匙是打码的"
fi

# ---------------------------------------------------------------- 只有一份配置
step "配置目录里只有一份配置文件"
# **策略类的东西一律进 config.yaml。**这一条会被慢慢侵蚀 —— 每加一个
# 功能都有一个「顺手开个文件放它的设置」的诱惑，而每一个单独看都很合理。
STRAY=$(find "$THINKWATCH_HOME" -maxdepth 1 -name '*.yaml' ! -name 'config.yaml' 2>/dev/null)
if [ -z "$STRAY" ]; then
  ok "除了 config.yaml 没有别的配置文件"
else
  bad "冒出了别的配置文件" "$STRAY"
fi

# ---------------------------------------------------------------- 客户端的专用密钥
# 接管本身在桌面端做；core 这边只发钥匙。**再要一次给的是同一把**
step "客户端的专用密钥"
C=$(post /clients/claude-code/key '{}')
[ "$C" = "200" ] && ok "发了一把专用密钥" || bad "返回 $C" "$(cat "$TMP/out")"
C=$(post /clients/claude-code/key '{}')
grep -q '"created":false' "$TMP/out" && ok "再要一次给的是同一把" || bad "又新建了一把" "$(cat "$TMP/out")"

# ---------------------------------------------------------------- 资源目标
# 资源目标有四个数字，而在此之前**没有任何东西在守它们** —— 一个写在
# 文档里、没人验的目标，和没有目标的区别只在于它让人以为验过。
step "资源目标"

RSS_KB=$(ps -o rss= -p "$CORE_PID" | tr -d ' ')
RSS_MB=$((RSS_KB / 1024))
# 目标 < 30 MB。**留一点余量但不留太多**：卡死在 30 会让一次无关的
# 依赖升级把 CI 弄红，而放到 100 就等于没有这个检查
if [ "$RSS_MB" -lt 30 ]; then
  ok "内存 ${RSS_MB} MB（目标 < 30）"
elif [ "$RSS_MB" -lt 45 ]; then
  # **不给超标的数字打勾。**那等于盖章说它达标了
  warn "内存 ${RSS_MB} MB，超出 30 MB 目标" "跑过一轮请求之后量的，不是纯冷启动；40 以内不算回归"
else
  bad "内存 ${RSS_MB} MB，比目标高出一截"
fi

# 空闲时不写盘（最后一条）。**没有请求就不该有任何写入** ——
# 一个常驻进程每秒摸一次磁盘，在笔记本上就是电量
DB_BEFORE=$(mtime_size "$THINKWATCH_HOME/data.db" || echo "0 0")
sleep 3
DB_AFTER=$(mtime_size "$THINKWATCH_HOME/data.db" || echo "1 1")
[ "$DB_BEFORE" = "$DB_AFTER" ] && ok "空闲 3 秒没有写盘" \
  || bad "空闲时还在写盘" "before=$DB_BEFORE after=$DB_AFTER"

# 转发的额外延迟（入站解析加路由 < 2ms、中继 < 1ms）。
# **和直连同一个假上游比** —— 差出来的就是我们这一层的成本。
# 两边各打 20 次取总时间，单次的噪声比我们要量的东西还大。
direct_ms() {
  local t0 t1
  t0=$(python3 -c 'import time;print(int(time.time()*1000))')
  for _ in $(seq 1 20); do
    curl -s -o /dev/null -XPOST "http://127.0.0.1:$UPPORT/v1/messages" \
      -H 'content-type: application/json' -d "$BODY"
  done
  t1=$(python3 -c 'import time;print(int(time.time()*1000))')
  echo $(( (t1 - t0) / 20 ))
}
through_ms() {
  local t0 t1
  t0=$(python3 -c 'import time;print(int(time.time()*1000))')
  for _ in $(seq 1 20); do
    curl -s -o /dev/null -XPOST "http://127.0.0.1:$PORT/v1/messages" \
      -H 'x-api-key: tw-smoketestkey0123456789' \
      -H 'content-type: application/json' -d "$BODY"
  done
  t1=$(python3 -c 'import time;print(int(time.time()*1000))')
  echo $(( (t1 - t0) / 20 ))
}
D=$(direct_ms); T=$(through_ms); OVER=$((T - D))
# 门槛放在 15ms：curl 每次起一个进程，那个噪声本来就有几毫秒，而这个
# 检查要抓的是「某次改动让每个请求多花了几十毫秒」那种量级的回归
if [ "$OVER" -lt 15 ]; then
  ok "经过网关比直连多 ${OVER}ms（直连 ${D}ms、经过 ${T}ms）"
else
  bad "经过网关多花了 ${OVER}ms，目标是解析加路由 < 2ms、中继 < 1ms"
fi

# ---------------------------------------------------------------- 请它退出
#
# **放在最后**：这一条真的会把 core 停掉，后面就没有控制面可用了。
#
# 这是整条链唯一被端到端验证的地方 —— 一条 HTTP 请求扳开关、主循环的
# select 醒过来、进程自己退出。单元测试能证明开关会被扳动，证明不了它
# 接在主循环上；而接错的样子是请求返回 202、进程稳稳地继续跑。
# **换钥匙**：跑着的 core 从文件监听拿到新钥匙，旧的那把立刻进不来
step "换钥匙"
NEW=$("$BIN" --config "$CFG" control-key --rotate 2>/dev/null)
[ -n "$NEW" ] && [ "$NEW" != "$KEY" ] && ok "control-key --rotate 换了一把" || bad "没换成" "$NEW"
sleep 1.5
ctl /status | grep -q '"api_version"' && ok "新钥匙进得来" || bad "换完钥匙进不来了" "$(tail -3 "$TMP/core.log")"
printf 'listen:\n  control:\n    key: %s\n' "$KEY" > "$TMP/wrong/config.yaml"
if "$BIN" --config "$TMP/wrong/config.yaml" call /status >/dev/null 2>&1; then
  bad "旧钥匙换掉之后还进得来"
else
  ok "旧钥匙进不来了"
fi

step "请它退出，它就退"
CODE=$(ctl --out "$TMP/out" -X POST /shutdown)
if [ "$CODE" = 202 ]; then
  ok "控制面收下了这条请求"
else
  bad "请它退出返回 $CODE" "$(cat "$TMP/out")"
fi
# 给它几秒真正退出。**等的是进程没了，不是 socket 没了** —— 后者在
# 一个卡住的进程上也可能发生。
GONE=0
for _ in $(seq 1 50); do
  kill -0 "$CORE_PID" 2>/dev/null || { GONE=1; break; }
  sleep 0.2
done
if [ "$GONE" = 1 ]; then
  ok "进程在几秒内自己退了"
else
  bad "请过它退出之后，进程还在跑（pid $CORE_PID）"
fi

# ---------------------------------------------------------------- 收尾
step "结果"
printf '通过 %d，失败 %d，超标但没回归 %d\n' "$PASS" "$FAIL" "$WARN"
[ "$FAIL" -eq 0 ] || exit 1
