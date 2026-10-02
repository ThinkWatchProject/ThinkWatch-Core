#!/usr/bin/env bash
# 给 CI 和发版的 runner 备好编插件沙箱要的 clang 和 llvm-ar。
#
# 插件沙箱里的 QuickJS 在构建时编成 WebAssembly（crates/tw-plugin/build.rs），
# 要一个能出 wasm32 的 clang 和配套的 llvm-ar；链接用 Rust 自带的 rust-lld。
#
# - macOS：Homebrew 的 llvm（Apple 的 clang 编不了 wasm）。不写环境变量 ——
#   build.rs 自己找 Homebrew 的位置，顺便测了这条路。
# - Linux：Ubuntu 22.04 镜像（x64 和 arm64 两种）都预装的 clang-15，显式指定，
#   两个架构用同一个版本。没有就从 apt 装。
# - Windows：镜像预装的 LLVM（C:\Program Files\LLVM），没有就装官方发行版。
#   同样交给 build.rs 自己找。
#
# 用的是哪个 clang、编出的 wasm 是什么哈希，构建之后由 `wasm-toolchain.sh record`
# 打印出来（写进 crates/tw-plugin 的 OUT_DIR/guest-build.txt，也编进二进制）。
#
# 用法：scripts/wasm-toolchain.sh                    准备工具链
#       scripts/wasm-toolchain.sh record DIR [FILE]  打印 DIR 下找到的 guest-build.txt，
#                                                    给了 FILE 就再拷一份过去
set -euo pipefail

if [ "${1:-}" = record ]; then
  dir="${2:-target}"
  copy="${3:-}"
  found=0
  while IFS= read -r f; do
    found=1
    if [ -n "$copy" ]; then
      mkdir -p "$(dirname "$copy")"
      cp "$f" "$copy"
    fi
    echo "== $f"
    cat "$f"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
      {
        echo '```'
        cat "$f"
        echo '```'
      } >> "$GITHUB_STEP_SUMMARY"
    fi
  done < <(find "$dir" -path '*/tw-plugin-*/out/guest-build.txt' 2>/dev/null | sort)
  if [ "$found" = 0 ]; then
    echo "no guest-build.txt under $dir: tw-plugin was not built" >&2
    exit 1
  fi
  exit 0
fi

case "$(uname -s)" in
  Darwin)
    # brew update 要几分钟，而且换不来什么：镜像里的 Homebrew 本来就够新
    HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK=1 \
      brew install llvm
    "$(brew --prefix llvm)/bin/clang" --version
    ;;
  Linux)
    v=15
    if ! command -v "clang-$v" > /dev/null || ! command -v "llvm-ar-$v" > /dev/null; then
      sudo apt-get update -q
      sudo apt-get install -y -q "clang-$v" "llvm-$v"
    fi
    clang=$(command -v "clang-$v")
    ar=$(command -v "llvm-ar-$v")
    "$clang" --version
    if [ -n "${GITHUB_ENV:-}" ]; then
      echo "TW_WASM_CLANG=$clang" >> "$GITHUB_ENV"
      echo "TW_WASM_AR=$ar" >> "$GITHUB_ENV"
    fi
    ;;
  MINGW* | MSYS* | CYGWIN*)
    dir="/c/Program Files/LLVM/bin"
    if [ ! -x "$dir/clang.exe" ] || [ ! -x "$dir/llvm-ar.exe" ]; then
      choco install llvm -y --no-progress
    fi
    "$dir/clang.exe" --version
    ;;
  *)
    echo "unsupported runner: $(uname -s)" >&2
    exit 1
    ;;
esac
