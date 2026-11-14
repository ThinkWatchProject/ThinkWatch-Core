"""The recorder follows the build manifest, not Cargo's private directory layout."""

import pathlib
import subprocess
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("wasm-toolchain.sh")
MANIFEST = "guest.wasm sha256 test-hash\nclang test-clang\n"


class RecorderTests(unittest.TestCase):
    def test_cargo_layout_does_not_change_recorded_provenance(self):
        for relative in [
            "debug/build/tw-plugin-abc/out/guest-build.txt",
            "debug/build/tw-plugin/abc/out/guest-build.txt",
        ]:
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                source = root / "target" / relative
                source.parent.mkdir(parents=True)
                source.write_text(MANIFEST)
                copy = root / "record" / "target.txt"
                result = subprocess.run(
                    ["bash", str(SCRIPT), "record", str(root / "target"), str(copy)],
                    capture_output=True, text=True, check=True,
                )
                self.assertIn(MANIFEST, result.stdout)
                self.assertEqual(copy.read_text(), MANIFEST)

    def test_symlinked_build_directory_is_supported(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            (root / "actual").mkdir()
            (root / "actual" / "guest-build.txt").write_text(MANIFEST)
            (root / "target").symlink_to(root / "actual", target_is_directory=True)
            result = subprocess.run(
                ["bash", str(SCRIPT), "record", str(root / "target")],
                capture_output=True, text=True, check=True,
            )
            self.assertIn(MANIFEST, result.stdout)

    def test_missing_manifest_remains_a_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                ["bash", str(SCRIPT), "record", tmp], capture_output=True, text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("no guest-build.txt", result.stderr)


if __name__ == "__main__":
    unittest.main()
