#!/usr/bin/env python3
"""写出一版在 GitHub Release 页面上的正文。

用法：release_notes.py <版本> <更新列表文件>

正文是英文的，依次是：

1. `release-notes/<版本>.md`：这一版的说明。**可以没有**，没有就从下载表开始。
2. 下载表：每个平台一行，文件名和 release.yml 挂上去的一字不差。
3. 服务器上安装、升级到这一版的命令。
4. 怎样用 `.sha256` 核对下载的文件。
5. 第二个参数的内容：GitHub 按上一版以来合并的 PR 生成的「What's Changed」，由
   调用方用 `gh api repos/<仓库>/releases/generate-notes` 取来。拆成参数传进来，
   这个脚本就不碰网络，测试可以直接喂它。

说明文件写坏了（有中文、有一级标题、是空的）就不写正文 —— 发版流水线在这一步
停下，比发出去再改好。CI 在每个 PR 上把 `release-notes/` 下的每一份都过一遍
（release_notes_test.py），所以真到打 tag 时不该再撞上。
"""

from __future__ import annotations

import pathlib
import re
import sys

REPO = "ThinkWatchProject/ThinkWatch-Core"
ROOT = pathlib.Path(__file__).resolve().parent.parent
NOTES_DIR = ROOT / "release-notes"

# 每个平台发的文件：(平台, 二进制, 服务器安装用的压缩包)。**和 release.yml 的
# FILES 一字不差** —— release_notes_test.py 拿那份清单核对这张表，多一个少一个
# 都不行，对不上的话发布页上的链接就是 404。
DOWNLOADS = [
    ("Linux, x86_64", "twcore-x86_64-unknown-linux-gnu", "twcore-x86_64-unknown-linux-gnu.tar.gz"),
    ("Linux, aarch64", "twcore-aarch64-unknown-linux-gnu", "twcore-aarch64-unknown-linux-gnu.tar.gz"),
    ("macOS, Apple silicon", "twcore-aarch64-apple-darwin", None),
    ("Windows, x64", "twcore-x86_64-pc-windows-msvc.exe", None),
    ("Windows, ARM64", "twcore-aarch64-pc-windows-msvc.exe", None),
]

INSTALL_SH = f"https://raw.githubusercontent.com/{REPO}/main/scripts/install.sh"

# 三段数字，和 `twcore upgrade` 认的一样：0.47.0
VERSION = re.compile(r"\d+\.\d+\.\d+")
# 中日韩文字和全角标点（码位区间）。发布页写英文
CJK_RANGES = [(0x3000, 0x30FF), (0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xF900, 0xFAFF), (0xFF00, 0xFFEF)]
CJK = re.compile("[" + "".join(f"{chr(a)}-{chr(b)}" for a, b in CJK_RANGES) + "]")


class NotesError(Exception):
    pass


def summary(version: str, notes_dir: pathlib.Path = NOTES_DIR) -> str | None:
    """`release-notes/<版本>.md` 的内容；没有这个文件是 None。"""
    path = notes_dir / f"{version}.md"
    if not path.exists():
        return None
    text = path.read_text(encoding="utf-8").strip()
    name = f"release-notes/{path.name}"
    if not text:
        raise NotesError(f"{name} is empty; delete it if there is nothing to say")
    for n, line in enumerate(text.splitlines(), 1):
        if CJK.search(line):
            raise NotesError(f"{name}, line {n}: the release page is written in English\n  {line}")
        # 发布页的标题由流水线定（ThinkWatch Core <版本>），正文里再来一个一级标题就重了
        if line.startswith("# "):
            raise NotesError(
                f"{name}, line {n}: the workflow sets the title; start with ## or with text\n  {line}"
            )
    return text


def render(version: str, changes: str, summary_text: str | None) -> str:
    """整份正文。`changes` 是 GitHub 生成的那段，原样接在最后。"""
    if not VERSION.fullmatch(version):
        raise NotesError(f"`{version}` is not a version such as 0.47.0")
    base = f"https://github.com/{REPO}/releases/download/v{version}"

    def link(name: str | None) -> str:
        return f"[`{name}`]({base}/{name})" if name else "—"

    parts = []
    if summary_text:
        parts.append(summary_text)

    rows = "\n".join(f"| {platform} | {link(binary)} | {link(archive)} |" for platform, binary, archive in DOWNLOADS)
    parts.append(
        f"""## Downloads

| Platform | Binary | Archive for server installation |
|---|---|---|
{rows}

Each file is published with a `.sha256` file beside it. A Linux archive contains `twcore`, the systemd unit `twcore.service` and `LICENSE`. ThinkWatch Lite includes its own copy of `twcore`; the files here are for running core separately, such as on a server."""
    )

    parts.append(
        f"""## Server installation

On Linux (x86_64 or aarch64), the install script sets up `twcore` as a systemd service. This installs {version}:

```sh
curl -fsSL {INSTALL_SH} | sudo sh -s -- --version {version}
```

An installation made with the script switches to {version} with:

```sh
sudo twcore upgrade --version {version} --restart
```

Configuration, the remote control port and connecting ThinkWatch Lite are described in [docs/server.md](https://github.com/{REPO}/blob/main/docs/server.md)."""
    )

    parts.append(
        """## Verifying a download

A `.sha256` file holds the SHA-256 of the file followed by its name. With both files in the current directory, on Linux:

```sh
sha256sum -c twcore-x86_64-unknown-linux-gnu.tar.gz.sha256
```

On macOS:

```sh
shasum -a 256 -c twcore-aarch64-apple-darwin.sha256
```

On Windows, in PowerShell, the following prints `True` when the binary matches:

```powershell
(Get-FileHash .\\twcore-x86_64-pc-windows-msvc.exe).Hash -eq (Get-Content .\\twcore-x86_64-pc-windows-msvc.exe.sha256).Split()[0]
```

The install script and `twcore upgrade` check the SHA-256 themselves."""
    )

    if changes.strip():
        parts.append(changes.strip())
    return "\n\n".join(parts) + "\n"


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: release_notes.py <version> <file with the generated list of changes>")
    version, changes_file = sys.argv[1], sys.argv[2]
    changes = pathlib.Path(changes_file).read_text(encoding="utf-8")
    try:
        sys.stdout.write(render(version, changes, summary(version)))
    except NotesError as e:
        sys.exit(str(e))


if __name__ == "__main__":
    main()
