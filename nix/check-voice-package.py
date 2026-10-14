"""Check the installed voice closure and initialize it without opening devices."""

import hashlib
import json
import os
from pathlib import Path
import selectors
import struct
import subprocess
import sys
import time


def receive(process, expected):
    deadline = time.monotonic() + 20
    data = bytearray()
    size = 4
    with selectors.DefaultSelector() as selector:
        selector.register(process.stdout, selectors.EVENT_READ)
        while len(data) < size:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not selector.select(remaining):
                raise RuntimeError("voice helper reply timed out")
            chunk = os.read(process.stdout.fileno(), size - len(data))
            if not chunk:
                raise RuntimeError("voice helper closed before replying")
            data.extend(chunk)
            if size == 4 and len(data) == 4:
                length = struct.unpack(">I", data)[0]
                if not 0 < length <= 128 * 1024:
                    raise RuntimeError("invalid voice helper reply length")
                size += length
    if json.loads(data[4:]) != {"type": expected}:
        raise RuntimeError("unexpected voice helper reply")


def check(package):
    voice = package / "codex-resources/voice"
    manifest = json.loads((voice / "manifest.json").read_text())
    commit = manifest["buildCommit"]
    for relative, expected in manifest["sha256"].items():
        path = package / relative
        if path.is_symlink() or not path.is_file():
            raise RuntimeError(f"voice package file is not physical: {relative}")
        with path.open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest() != expected:
                raise RuntimeError(f"voice package hash differs: {relative}")
    runtime = json.loads((voice / "runtime.json").read_text())
    if runtime["sourceCommit"] != commit:
        raise RuntimeError("voice runtime build identity differs")
    helper = voice / "bin/codex-voice-host"
    actual = subprocess.check_output(
        [helper, "--build-commit"], text=True, timeout=20
    ).strip()
    if actual != commit:
        raise RuntimeError("voice helper build identity differs")
    environment = {
        "HOME": os.environ.get("HOME", "/tmp"),
        "GST_PLUGIN_PATH": "",
        "GST_PLUGIN_PATH_1_0": "",
        "GST_PLUGIN_SYSTEM_PATH": "",
        "GST_PLUGIN_SYSTEM_PATH_1_0": "",
        "GST_REGISTRY": "/dev/null",
        "GST_REGISTRY_UPDATE": "no",
        "GST_REGISTRY_FORK": "no",
    }
    process = subprocess.Popen(
        [helper],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env=environment,
    )
    try:
        for message, expected in [
            ({"type": "hello", "protocol": 1, "buildCommit": commit}, "ready"),
            ({"type": "initializeRuntime"}, "runtimeReady"),
            ({"type": "close"}, "closed"),
        ]:
            payload = json.dumps(message).encode()
            process.stdin.write(struct.pack(">I", len(payload)) + payload)
            process.stdin.flush()
            receive(process, expected)
        if process.wait(timeout=20) != 0:
            raise RuntimeError("voice helper did not close successfully")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
    print(
        "Voice package hashes, build identity and native initialization passed; no devices opened."
    )


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve(strict=True))
