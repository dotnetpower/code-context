"""Validate the installable artifact using only the Python standard library."""

import json
from pathlib import Path
import struct
import sys
import zipfile


def verify(path: Path) -> dict:
    with zipfile.ZipFile(path) as archive:
        names = set(archive.namelist())
        for name in names:
            parts = Path(name).parts
            if name.startswith("/") or ".." in parts:
                raise ValueError(f"Unsafe package path: {name}")
            if any(part in {"node_modules", "test", "tests", ".git", "__pycache__"} for part in parts):
                raise ValueError(f"Development-only package entry: {name}")
            if name.endswith((".ts", ".map", ".svg", ".pyc")):
                raise ValueError(f"Unexpected source/build entry: {name}")
        required = {
            "extension/package.json", "extension/readme.md", "extension/LICENSE.txt",
            "extension/changelog.md", "extension/docs/SETUP.md",
            "extension/docs/PUBLISHING.md", "extension/resources/icon.png",
            "extension/resources/banner.png", "extension/out/src/extension.js",
            "extension/prompts/code-context.instructions.md",
        }
        missing = required - names
        if missing:
            raise ValueError(f"Missing entries: {sorted(missing)}")
        manifest = json.loads(archive.read("extension/package.json"))
        if manifest.get("enabledApiProposals") or manifest["contributes"].get("languageModelToolSets"):
            raise ValueError("Distributable must use stable tool contributions only")
        expected = {
            "cctxNativeFiles", "cctxNativeGrep", "cctxNativeContext", "cctxNativeRead",
            "cctxNativeStatus", "cctxNativeSync", "cctxNativeWorkspaces",
        }
        tools = manifest["contributes"]["languageModelTools"]
        if len(tools) != 7 or {tool["toolReferenceName"] for tool in tools} != expected:
            raise ValueError("Native tool references changed")
        if manifest["publisher"] != "moonchoi" or manifest["name"] != "code-context-native":
            raise ValueError("Extension identity changed")
        if manifest["repository"]["url"] != "https://github.com/dotnetpower/code-context.git":
            raise ValueError("Unexpected source repository")
        for walkthrough in manifest["contributes"]["walkthroughs"]:
            for step in walkthrough["steps"]:
                media = step.get("media", {}).get("markdown")
                if not media or not media.startswith("docs/walkthrough/") or f"extension/{media}" not in names:
                    raise ValueError(f"Missing packaged walkthrough media: {step['id']}")
        for name, expected_size in [("icon", (256, 256)), ("banner", (1440, 480))]:
            image = archive.read(f"extension/resources/{name}.png")
            if image[:8] != b"\x89PNG\r\n\x1a\n" or struct.unpack(">II", image[16:24]) != expected_size:
                raise ValueError(f"Invalid {name} dimensions")
        return {
            "artifact": path.name, "version": manifest["version"], "entries": len(names),
            "native_tools": len(tools), "stable_api_only": True,
            "icon": manifest["icon"], "bytes": path.stat().st_size,
        }


if __name__ == "__main__":
    print(json.dumps(verify(Path(sys.argv[1])), indent=2))
