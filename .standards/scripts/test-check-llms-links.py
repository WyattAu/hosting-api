#!/usr/bin/env python3
"""Negative test: placeholders must FAIL, real URLs must pass."""
import pathlib
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).parent
CHECK = HERE / "check-llms-links.py"

BAD = """# T
> t

- [A](https://github.com/WyattAu/$1/blob/main/X.md)
- [B](https://github.com/WyattAu/${1}/blob/main/Y.md)
"""
GOOD = """# T
> t

- [A](https://github.com/WyattAu/OmniTS-template/blob/main/X.md)
"""


def run(text: str) -> int:
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as fh:
        fh.write(text)
        name = fh.name
    try:
        return subprocess.run(
            [sys.executable, str(CHECK), name], capture_output=True, text=True
        ).returncode
    finally:
        pathlib.Path(name).unlink(missing_ok=True)


def main() -> int:
    if run(BAD) == 0:
        print("FAIL: placeholders were accepted")
        return 1
    if run(GOOD) != 0:
        print("FAIL: a clean file was rejected")
        return 1
    if run("") != 0:
        print("FAIL: an empty file should be clean")
        return 1
    missing = subprocess.run(
        [sys.executable, str(CHECK), "definitely-not-here.txt"],
        capture_output=True,
        text=True,
    )
    if missing.returncode == 0:
        print("FAIL: a missing file should fail")
        return 1
    print("ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
