#!/usr/bin/env python3
"""release_notes.py 的测试，外加 `release-notes/` 下的每一份说明。

    python3 scripts/release_notes_test.py

CI 在每个 PR 上跑它：发布页的正文只在推 tag 时才写，写坏了（链接 404、说明里
混进中文）要到那时才看得见，而那时要撤回的是一个已经推上去的 tag。
"""

import pathlib
import re
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
# 不在仓库里留 __pycache__
sys.dont_write_bytecode = True

import release_notes as rn  # noqa: E402

ROOT = HERE.parent
WORKFLOW = (ROOT / ".github/workflows/release.yml").read_text(encoding="utf-8")

CHANGES = """## What's Changed
* chore: v0.47.0 by @fylorn in https://github.com/ThinkWatchProject/ThinkWatch-Core/pull/190


**Full Changelog**: https://github.com/ThinkWatchProject/ThinkWatch-Core/compare/v0.46.0...v0.47.0"""

BASE = "https://github.com/ThinkWatchProject/ThinkWatch-Core/releases/download/v0.47.0/"


def published() -> list[str]:
    """release.yml 里 FILES 那一段：发布出去的每一个文件。"""
    block = re.search(r"\n      FILES: \|\n((?:        dist/\S+\n)+)", WORKFLOW)
    assert block, "release.yml has no FILES block"
    return [line.strip().removeprefix("dist/") for line in block.group(1).splitlines()]


def notes_dir(case: unittest.TestCase, files: dict[str, str]) -> pathlib.Path:
    """一个临时的 `release-notes/`，测试结束就删。"""
    tmp = tempfile.TemporaryDirectory()
    case.addCleanup(tmp.cleanup)
    d = pathlib.Path(tmp.name)
    for name, text in files.items():
        (d / name).write_text(text, encoding="utf-8")
    return d


class Body(unittest.TestCase):
    def test_sections_come_in_order(self):
        body = rn.render("0.47.0", CHANGES, "A remote control port.")
        order = [
            body.index("A remote control port."),
            body.index("## Downloads"),
            body.index("## Server installation"),
            body.index("## Verifying a download"),
            body.index("## What's Changed"),
        ]
        self.assertEqual(order, sorted(order))
        self.assertTrue(body.startswith("A remote control port.\n\n## Downloads"))
        self.assertTrue(body.endswith("v0.46.0...v0.47.0\n"))

    def test_without_a_summary_it_starts_with_the_downloads(self):
        self.assertTrue(rn.render("0.47.0", CHANGES, None).startswith("## Downloads\n"))

    def test_without_changes_there_is_no_empty_section(self):
        body = rn.render("0.47.0", "\n", None)
        self.assertNotIn("What's Changed", body)
        self.assertTrue(body.endswith("themselves.\n"))

    def test_it_links_exactly_the_files_the_workflow_publishes(self):
        # 多一个是 404，少一个是发了没人找得到
        body = rn.render("0.47.0", CHANGES, None)
        links = [u.removeprefix(BASE) for u in re.findall(r"\]\((https://[^)]+)\)", body) if u.startswith(BASE)]
        files = [f for f in published() if not f.endswith(".sha256")]
        self.assertEqual(len(files), 7, files)
        self.assertEqual(sorted(links), sorted(files))
        # 每个文件都有它的校验文件
        for f in files:
            self.assertIn(f"{f}.sha256", published())

    def test_install_and_upgrade_pin_this_version(self):
        body = rn.render("0.47.0", CHANGES, None)
        self.assertIn(
            "\ncurl -fsSL https://raw.githubusercontent.com/ThinkWatchProject/ThinkWatch-Core/main/scripts/install.sh"
            " | sudo sh -s -- --version 0.47.0\n",
            body,
        )
        self.assertIn("\nsudo twcore upgrade --version 0.47.0 --restart\n", body)

    def test_the_commands_use_options_that_exist(self):
        # install.sh 和 twcore upgrade 各自认的选项；改了名字这里就对不上
        install = (ROOT / "scripts/install.sh").read_text(encoding="utf-8")
        self.assertIn("--version) ", install)
        main = (ROOT / "bin/twcore/src/main.rs").read_text(encoding="utf-8")
        upgrade = main[main.index("    Upgrade {") : main.index("\n    },", main.index("    Upgrade {"))]
        for field in ["restart: bool", "version: Option<String>"]:
            self.assertIn(field, upgrade)

    def test_the_checksum_commands_name_real_files(self):
        body = rn.render("0.47.0", CHANGES, None)
        for f in ["twcore-x86_64-unknown-linux-gnu.tar.gz", "twcore-aarch64-apple-darwin", "twcore-x86_64-pc-windows-msvc.exe"]:
            self.assertIn(f"{f}.sha256", published())
        self.assertIn("sha256sum -c twcore-x86_64-unknown-linux-gnu.tar.gz.sha256\n", body)
        self.assertIn("shasum -a 256 -c twcore-aarch64-apple-darwin.sha256\n", body)
        self.assertIn(
            "(Get-FileHash .\\twcore-x86_64-pc-windows-msvc.exe).Hash -eq "
            "(Get-Content .\\twcore-x86_64-pc-windows-msvc.exe.sha256).Split()[0]\n",
            body,
        )

    def test_the_body_itself_is_english(self):
        self.assertIsNone(rn.CJK.search(rn.render("0.47.0", CHANGES, None)))

    def test_a_malformed_version_is_refused(self):
        for bad in ["v0.47.0", "0.47", "0.47.0-rc1", "2026.9.16.1", ""]:
            with self.assertRaises(rn.NotesError, msg=bad):
                rn.render(bad, CHANGES, None)


class Summary(unittest.TestCase):
    def test_absent_is_none(self):
        self.assertIsNone(rn.summary("0.47.0", notes_dir(self, {})))

    def test_read_and_trimmed(self):
        d = notes_dir(self, {"0.47.0.md": "\nA remote control port.\n\n"})
        self.assertEqual(rn.summary("0.47.0", d), "A remote control port.")

    def test_chinese_is_refused(self):
        for text in ["远程控制端口。", "Remote port（远程）", "Remote port，"]:
            d = notes_dir(self, {"0.47.0.md": f"Upgrade notes.\n\n{text}\n"})
            with self.assertRaises(rn.NotesError, msg=text) as e:
                rn.summary("0.47.0", d)
            self.assertIn("line 3", str(e.exception))

    def test_a_top_level_heading_is_refused(self):
        d = notes_dir(self, {"0.47.0.md": "# ThinkWatch Core 0.47.0\n\nText.\n"})
        with self.assertRaises(rn.NotesError):
            rn.summary("0.47.0", d)
        # 二级标题可以
        d = notes_dir(self, {"0.47.0.md": "## Upgrade notes\n\nText.\n"})
        self.assertEqual(rn.summary("0.47.0", d), "## Upgrade notes\n\nText.")

    def test_an_empty_file_is_refused(self):
        with self.assertRaises(rn.NotesError):
            rn.summary("0.47.0", notes_dir(self, {"0.47.0.md": " \n\n"}))


class Committed(unittest.TestCase):
    def test_every_file_in_release_notes_renders(self):
        for path in sorted(rn.NOTES_DIR.glob("*")):
            with self.subTest(path.name):
                self.assertEqual(path.suffix, ".md")
                version = path.name.removesuffix(".md")
                self.assertRegex(version, rn.VERSION)
                text = rn.summary(version)
                self.assertTrue(rn.render(version, CHANGES, text).startswith(text))


class CommandLine(unittest.TestCase):
    def run_script(self, version: str) -> subprocess.CompletedProcess:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        changes = pathlib.Path(tmp.name) / "changes.md"
        changes.write_text(CHANGES, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(HERE / "release_notes.py"), version, str(changes)],
            capture_output=True,
            text=True,
        )

    def test_writes_the_body_to_stdout(self):
        r = self.run_script("9.9.9")
        self.assertEqual(r.returncode, 0, r.stderr)
        out = r.stdout
        self.assertTrue(out.startswith("## Downloads\n"))
        self.assertIn("releases/download/v9.9.9/twcore-aarch64-apple-darwin)", out)

    def test_a_bad_version_fails(self):
        r = self.run_script("v9.9.9")
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(r.stdout, "")


if __name__ == "__main__":
    unittest.main()
