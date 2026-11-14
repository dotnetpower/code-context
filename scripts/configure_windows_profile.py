#!/usr/bin/env python3
"""Configure Windows VS Code base and WSL-associated profiles for Code Context Native."""

from __future__ import annotations

import argparse
import glob
import json
import os
import shutil
import sqlite3
import tempfile
from pathlib import Path

EXT_ID = "dotnetpower.code-context-native"
LEGACY_EXT_ID = "moonchoi.code-context-native"


def atomic_write_json(path: Path, data: object, *, indent: int | None = 4) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=f"{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as temporary:
            if indent is None:
                json.dump(data, temporary, ensure_ascii=False, separators=(",", ":"))
            else:
                json.dump(data, temporary, indent=indent, ensure_ascii=False)
                temporary.write("\n")
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, path)
    except BaseException:
        Path(temporary_name).unlink(missing_ok=True)
        raise


def discover_associated_profiles(user_data: Path, distribution: str) -> tuple[list[str], list[str]]:
    """Return (profile_ids, profile_names) associated with `wsl+<distribution>`."""
    profile_ids: list[str] = []
    database = user_data / "globalStorage" / "state.vscdb"
    if database.is_file():
        try:
            with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
                row = connection.execute(
                    "SELECT value FROM ItemTable WHERE key = 'associatedRemoteProfiles'"
                ).fetchone()
            if row and row[0]:
                associations = json.loads(row[0])
                raw_ids = associations.get(f"wsl+{distribution}", [])
                if isinstance(raw_ids, list):
                    profile_ids = [
                        str(pid)
                        for pid in raw_ids
                        if isinstance(pid, str) and pid and not pid.startswith("builtin/")
                    ]
        except (OSError, sqlite3.Error, json.JSONDecodeError):
            profile_ids = []

    location_to_name: dict[str, str] = {}
    storage_json = user_data / "globalStorage" / "storage.json"
    if storage_json.is_file():
        try:
            storage = json.loads(storage_json.read_text(encoding="utf-8"))
            for entry in storage.get("userDataProfiles", []):
                if isinstance(entry, dict):
                    loc = entry.get("location")
                    name = entry.get("name")
                    if (
                        isinstance(loc, str)
                        and isinstance(name, str)
                        and loc
                        and not loc.startswith("builtin/")
                    ):
                        location_to_name[loc] = name
                        if loc not in profile_ids and (user_data / "profiles" / loc).is_dir():
                            profile_ids.append(loc)
        except (OSError, json.JSONDecodeError):
            pass

    profile_names = [location_to_name[pid] for pid in profile_ids if pid in location_to_name]
    return profile_ids, profile_names


def configure_settings(settings_path: Path, distribution: str, executable: str) -> bool:
    if settings_path.is_file():
        try:
            settings = json.loads(settings_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            return False
    else:
        settings = {}
    if not isinstance(settings, dict):
        return False

    extension_kinds = settings.setdefault("remote.extensionKind", {})
    if not isinstance(extension_kinds, dict):
        extension_kinds = {}
        settings["remote.extensionKind"] = extension_kinds
    extension_kinds.pop(LEGACY_EXT_ID, None)
    extension_kinds[EXT_ID] = ["ui"]
    settings["codeContextNative.wslDistribution"] = distribution
    settings["codeContextNative.wslExecutablePath"] = executable
    atomic_write_json(settings_path, settings, indent=4)
    return True


def update_extensions_manifest(manifest_path: Path, win_ext_dir: Path, version: str) -> bool:
    if not manifest_path.is_file():
        return False
    try:
        entries = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False
    if not isinstance(entries, list):
        return False

    target_rel = f"{EXT_ID}-{version}"
    win_abs = str(win_ext_dir)
    if win_abs.startswith("/mnt/") and len(win_abs) > 6 and win_abs[6] == "/":
        drive = win_abs[5]
        rest = win_abs[6:]
        uri_path = f"/{drive}:{rest}/{target_rel}"
        fs_path = f"{drive}:\\{rest.lstrip('/').replace('/', '\\')}\\{target_rel}"
        external = f"file:///{drive}%3A{rest}/{target_rel}"
    else:
        uri_path = f"{win_abs}/{target_rel}"
        fs_path = os.path.join(win_abs, target_rel)
        external = f"file://{uri_path}"

    changed = False
    filtered: list[dict] = []
    for item in entries:
        if not isinstance(item, dict):
            continue
        ident = (item.get("identifier") or {}).get("id")
        if ident == LEGACY_EXT_ID:
            changed = True
            continue
        if ident == EXT_ID and (
            item.get("version") != version or item.get("relativeLocation") != target_rel
        ):
            item["version"] = version
            item["relativeLocation"] = target_rel
            loc = item.get("location")
            if isinstance(loc, dict):
                loc["path"] = uri_path
                if "fsPath" in loc:
                    loc["fsPath"] = fs_path
                if "external" in loc:
                    loc["external"] = external
            changed = True
        filtered.append(item)

    if changed:
        atomic_write_json(manifest_path, filtered, indent=None)
    return changed


def sync_extension_directories(
    win_ext_dir: Path,
    user_data: Path,
    wsl_ext_src: Path,
    version: str,
) -> dict[str, int]:
    target_rel = f"{EXT_ID}-{version}"
    dst_dir = win_ext_dir / target_rel
    copied = 0
    if wsl_ext_src.is_dir() and wsl_ext_src.resolve() != dst_dir.resolve():
        if dst_dir.exists():
            shutil.rmtree(dst_dir)
        shutil.copytree(wsl_ext_src, dst_dir)
        copied = 1

    obsolete_path = win_ext_dir / ".obsolete"
    try:
        obsolete = (
            json.loads(obsolete_path.read_text(encoding="utf-8"))
            if obsolete_path.is_file()
            else {}
        )
    except (OSError, json.JSONDecodeError):
        obsolete = {}
    if not isinstance(obsolete, dict):
        obsolete = {}
    obsolete.pop(target_rel, None)

    removed_stale = 0
    synced_locked = 0
    for pattern in (f"{EXT_ID}-*", f"{LEGACY_EXT_ID}-*"):
        for raw_old in glob.glob(str(win_ext_dir / pattern)):
            old_dir = Path(raw_old)
            if old_dir.name == target_rel or not old_dir.is_dir():
                continue
            try:
                shutil.rmtree(old_dir)
                obsolete.pop(old_dir.name, None)
                removed_stale += 1
            except OSError:
                obsolete[old_dir.name] = True
                if wsl_ext_src.is_dir() and old_dir.name.startswith(f"{EXT_ID}-"):
                    for sub in ("out", "prompts"):
                        src_sub = wsl_ext_src / sub
                        dst_sub = old_dir / sub
                        if src_sub.is_dir():
                            shutil.rmtree(dst_sub, ignore_errors=True)
                            shutil.copytree(src_sub, dst_sub)
                    src_pkg = wsl_ext_src / "package.json"
                    if src_pkg.is_file():
                        shutil.copy2(src_pkg, old_dir / "package.json")
                    synced_locked += 1

    atomic_write_json(obsolete_path, obsolete, indent=None)

    manifests_updated = 0
    if update_extensions_manifest(win_ext_dir / "extensions.json", win_ext_dir, version):
        manifests_updated += 1
    profiles_root = user_data / "profiles"
    if profiles_root.is_dir():
        for prof_manifest in profiles_root.glob("*/extensions.json"):
            if update_extensions_manifest(prof_manifest, win_ext_dir, version):
                manifests_updated += 1

    return {
        "copied": copied,
        "removed_stale": removed_stale,
        "synced_locked": synced_locked,
        "manifests_updated": manifests_updated,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--user-data", type=Path, required=True)
    parser.add_argument("--distribution", required=True)
    parser.add_argument("--executable", default="")
    parser.add_argument("--extensions-dir", type=Path)
    parser.add_argument("--wsl-ext-src", type=Path)
    parser.add_argument("--version", default="")
    parser.add_argument("--print-profile-names", action="store_true")
    args = parser.parse_args()

    profile_ids, profile_names = discover_associated_profiles(args.user_data, args.distribution)
    if args.print_profile_names:
        for name in profile_names:
            print(name)
        return 0

    if not args.executable:
        raise SystemExit("--executable is required unless --print-profile-names is set")

    configured_paths: list[str] = []
    base_settings = args.user_data / "settings.json"
    if configure_settings(base_settings, args.distribution, args.executable):
        configured_paths.append(str(base_settings))

    for pid in profile_ids:
        profile_settings = args.user_data / "profiles" / pid / "settings.json"
        if profile_settings.parent.is_dir() and configure_settings(
            profile_settings, args.distribution, args.executable
        ):
            configured_paths.append(str(profile_settings))

    ext_stats: dict[str, int] = {}
    if args.extensions_dir and args.wsl_ext_src and args.version:
        ext_stats = sync_extension_directories(
            args.extensions_dir,
            args.user_data,
            args.wsl_ext_src,
            args.version,
        )

    print(
        json.dumps(
            {
                "configured_settings": configured_paths,
                "profile_ids": profile_ids,
                "profile_names": profile_names,
                **ext_stats,
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
