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


def strip_jsonc(raw: str) -> str:
    """Strip // line comments, /* */ block comments, and trailing commas outside JSON strings."""
    chars: list[str] = []
    in_string = False
    escaped = False
    i = 0
    n = len(raw)
    while i < n:
        ch = raw[i]
        if in_string:
            chars.append(ch)
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            i += 1
            continue
        if ch == '"':
            in_string = True
            chars.append(ch)
            i += 1
            continue
        if ch == "/" and i + 1 < n and raw[i + 1] == "/":
            i += 2
            while i < n and raw[i] not in ("\n", "\r"):
                i += 1
            continue
        if ch == "/" and i + 1 < n and raw[i + 1] == "*":
            i += 2
            while i + 1 < n and not (raw[i] == "*" and raw[i + 1] == "/"):
                i += 1
            i += 2
            continue
        chars.append(ch)
        i += 1
    no_comments = "".join(chars)
    out: list[str] = []
    in_string = False
    escaped = False
    m = len(no_comments)
    j = 0
    while j < m:
        ch = no_comments[j]
        if in_string:
            out.append(ch)
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            j += 1
            continue
        if ch == '"':
            in_string = True
            out.append(ch)
            j += 1
            continue
        if ch == ",":
            k = j + 1
            while k < m and no_comments[k] in (" ", "\t", "\r", "\n"):
                k += 1
            if k < m and no_comments[k] in ("}", "]"):
                j += 1
                continue
        out.append(ch)
        j += 1
    return "".join(out)


def load_jsonc(path: Path) -> object:
    text = path.read_text(encoding="utf-8-sig")
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return json.loads(strip_jsonc(text))


def has_jsonc_comments(raw: str) -> bool:
    in_string = False
    escaped = False
    i = 0
    n = len(raw)
    while i < n:
        ch = raw[i]
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            i += 1
            continue
        if ch == '"':
            in_string = True
            i += 1
            continue
        if ch == "/" and i + 1 < n and raw[i + 1] in ("/", "*"):
            return True
        i += 1
    return False


def _tokenize_jsonc(raw: str) -> list[tuple[str, int, int, str]]:
    """Return non-whitespace, non-comment tokens as (kind, start, end, text)."""
    tokens: list[tuple[str, int, int, str]] = []
    i = 0
    n = len(raw)
    while i < n:
        ch = raw[i]
        if ch in " \t\r\n":
            i += 1
            continue
        if ch == "/" and i + 1 < n and raw[i + 1] == "/":
            i += 2
            while i < n and raw[i] not in "\r\n":
                i += 1
            continue
        if ch == "/" and i + 1 < n and raw[i + 1] == "*":
            i += 2
            while i + 1 < n and not (raw[i] == "*" and raw[i + 1] == "/"):
                i += 1
            i += 2
            continue
        if ch == '"':
            start = i
            i += 1
            esc = False
            while i < n:
                c = raw[i]
                if esc:
                    esc = False
                elif c == "\\":
                    esc = True
                elif c == '"':
                    i += 1
                    break
                i += 1
            tokens.append(("str", start, i, raw[start:i]))
            continue
        if ch in "{}[]:,":
            tokens.append((ch, i, i + 1, ch))
            i += 1
            continue
        start = i
        while i < n and raw[i] not in " \t\r\n{}[]:,/\"":
            i += 1
        tokens.append(("lit", start, i, raw[start:i]))
    return tokens


def _skip_jsonc_value(tokens: list[tuple[str, int, int, str]], idx: int) -> int:
    kind = tokens[idx][0]
    if kind in ("{", "["):
        close_k = "}" if kind == "{" else "]"
        depth = 1
        idx += 1
        while idx < len(tokens) and depth > 0:
            if tokens[idx][0] == kind:
                depth += 1
            elif tokens[idx][0] == close_k:
                depth -= 1
            idx += 1
        return idx
    return idx + 1


def _parse_jsonc_object_fields(
    tokens: list[tuple[str, int, int, str]], open_brace_idx: int
) -> tuple[dict[str, tuple[int, int, int, int, int]], int]:
    """Map key -> (key_tok_idx, val_tok_start, val_tok_end_excl, char_key_start, char_val_end) and return close_brace_idx."""
    fields: dict[str, tuple[int, int, int, int, int]] = {}
    idx = open_brace_idx + 1
    while idx < len(tokens):
        kind, k_start, _k_end, text = tokens[idx]
        if kind == "}":
            return fields, idx
        if kind == ",":
            idx += 1
            continue
        if kind == "str" and idx + 1 < len(tokens) and tokens[idx + 1][0] == ":":
            key_name = json.loads(text)
            val_tok_start = idx + 2
            val_tok_end = _skip_jsonc_value(tokens, val_tok_start)
            char_val_end = tokens[val_tok_end - 1][2]
            fields[key_name] = (idx, val_tok_start, val_tok_end, k_start, char_val_end)
            idx = val_tok_end
        else:
            idx += 1
    return fields, len(tokens) - 1


def _extract_jsonc_comments(segment: str) -> list[str]:
    comments: list[str] = []
    i = 0
    n = len(segment)
    in_str = False
    esc = False
    while i < n:
        ch = segment[i]
        if in_str:
            if esc:
                esc = False
            elif ch == "\\":
                esc = True
            elif ch == '"':
                in_str = False
            i += 1
            continue
        if ch == '"':
            in_str = True
            i += 1
            continue
        if ch == "/" and i + 1 < n and segment[i + 1] == "/":
            start = i
            i += 2
            while i < n and segment[i] not in "\r\n":
                i += 1
            comments.append(segment[start:i])
            continue
        if ch == "/" and i + 1 < n and segment[i + 1] == "*":
            start = i
            i += 2
            while i + 1 < n and not (segment[i] == "*" and segment[i + 1] == "/"):
                i += 1
            i = min(n, i + 2)
            comments.append(segment[start:i])
            continue
        i += 1
    return comments


def _upsert_jsonc_object_property(raw: str, obj_open_tok_idx: int, key: str, value_json: str, indent: str = "    ") -> str:
    tokens = _tokenize_jsonc(raw)
    fields, close_tok_idx = _parse_jsonc_object_fields(tokens, obj_open_tok_idx)
    if key in fields:
        _, v_start_tok, v_end_tok, _, _ = fields[key]
        v_char_start = tokens[v_start_tok][1]
        v_char_end = tokens[v_end_tok - 1][2]
        inner_comments = _extract_jsonc_comments(raw[v_char_start:v_char_end])
        preserved = ("".join(f"{c}\n{indent}" for c in inner_comments)) if inner_comments else ""
        return raw[:v_char_start] + preserved + value_json + raw[v_char_end:]
    close_char = tokens[close_tok_idx][1]
    prev_tok = tokens[close_tok_idx - 1]
    if prev_tok[0] in ("{", ","):
        # Insert directly before `close_char` (`}`), which is guaranteed outside all block/line comments!
        return f'{raw[:close_char].rstrip()}\n{indent}"{key}": {value_json}\n{raw[close_char:]}'
    comma_pos = prev_tok[2]
    with_comma = raw[:comma_pos] + "," + raw[comma_pos:]
    adj_close = close_char + 1
    return f'{with_comma[:adj_close].rstrip()}\n{indent}"{key}": {value_json}\n{with_comma[adj_close:]}'


def _delete_jsonc_object_property(raw: str, obj_open_tok_idx: int, key: str) -> str:
    tokens = _tokenize_jsonc(raw)
    fields, _ = _parse_jsonc_object_fields(tokens, obj_open_tok_idx)
    if key not in fields:
        return raw
    k_tok_idx, _, v_end_tok, k_char_start, v_char_end = fields[key]
    inner_comments = _extract_jsonc_comments(raw[k_char_start:v_char_end])
    preserved = ("\n".join(inner_comments) + "\n") if inner_comments else ""
    if v_end_tok < len(tokens) and tokens[v_end_tok][0] == ",":
        comma_start = tokens[v_end_tok][1]
        comma_end = tokens[v_end_tok][2]
        between = raw[v_char_end:comma_start]
        return raw[:k_char_start] + preserved + between + raw[comma_end:]
    if k_tok_idx > obj_open_tok_idx + 1 and tokens[k_tok_idx - 1][0] == ",":
        comma_start = tokens[k_tok_idx - 1][1]
        comma_end = tokens[k_tok_idx - 1][2]
        between = raw[comma_end:k_char_start]
        return raw[:comma_start] + between + preserved + raw[v_char_end:]
    return raw[:k_char_start] + preserved + raw[v_char_end:]


def _write_preserving_jsonc_if_needed(path: Path, raw: str, updated_obj: dict) -> bool:
    import sys

    if not raw or not has_jsonc_comments(raw):
        atomic_write_json(path, updated_obj, indent=4)
        return True
    patched = raw
    tokens = _tokenize_jsonc(patched)
    if not tokens or tokens[0][0] != "{":
        print(f"Skipping malformed JSONC root in {path}", file=sys.stderr)
        return False
    for key, val in updated_obj.items():
        tokens = _tokenize_jsonc(patched)
        top_fields, _ = _parse_jsonc_object_fields(tokens, 0)
        if key in ("codeContextNative.wslDistribution", "codeContextNative.wslExecutablePath") and isinstance(val, str):
            patched = _upsert_jsonc_object_property(patched, 0, key, json.dumps(val), indent="    ")
        elif key == "remote.extensionKind" and isinstance(val, dict):
            if key in top_fields and tokens[top_fields[key][1]][0] == "{":
                rk_tok_idx = top_fields[key][1]
                patched = _delete_jsonc_object_property(patched, rk_tok_idx, LEGACY_EXT_ID)
                tokens = _tokenize_jsonc(patched)
                top_fields, _ = _parse_jsonc_object_fields(tokens, 0)
                rk_tok_idx = top_fields[key][1]
                patched = _upsert_jsonc_object_property(patched, rk_tok_idx, EXT_ID, '["ui"]', indent="        ")
            else:
                patched = _upsert_jsonc_object_property(patched, 0, key, json.dumps(val), indent="    ")
        elif key == "servers" and isinstance(val, dict) and "code-context" in val and key in top_fields:
            srv_tok_idx = top_fields[key][1]
            if tokens[srv_tok_idx][0] == "{":
                srv_fields, _ = _parse_jsonc_object_fields(tokens, srv_tok_idx)
                if "code-context" in srv_fields and tokens[srv_fields["code-context"][1]][0] == "{":
                    cc_tok_idx = srv_fields["code-context"][1]
                    patched = _delete_jsonc_object_property(patched, cc_tok_idx, "cwd")
                    tokens = _tokenize_jsonc(patched)
                    top_fields, _ = _parse_jsonc_object_fields(tokens, 0)
                    srv_fields, _ = _parse_jsonc_object_fields(tokens, top_fields["servers"][1])
                    cc_tok_idx = srv_fields["code-context"][1]
                    cc_val = val["code-context"]
                    if isinstance(cc_val, dict) and "args" in cc_val:
                        patched = _upsert_jsonc_object_property(
                            patched, cc_tok_idx, "args", json.dumps(cc_val["args"]), indent="            "
                        )
    try:
        reparsed = json.loads(strip_jsonc(patched))
    except json.JSONDecodeError as exc:
        print(f"Failed to preserve JSONC comments for {path}: {exc}", file=sys.stderr)
        return False
    if reparsed != updated_obj:
        print(f"JSONC update mismatch for {path}; skipping write to preserve file", file=sys.stderr)
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=f"{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as temporary:
            temporary.write(patched)
            temporary.flush()
            os.fsync(temporary.fileno())
        os.replace(temporary_name, path)
        return True
    except BaseException:
        Path(temporary_name).unlink(missing_ok=True)
        raise


def configure_mcp_json(mcp_path: Path, distribution: str, executable: str) -> bool:
    if not mcp_path.is_file():
        return False
    try:
        raw = mcp_path.read_text(encoding="utf-8-sig")
        mcp_cfg = load_jsonc(mcp_path)
    except (OSError, json.JSONDecodeError):
        return False
    if not isinstance(mcp_cfg, dict):
        return False
    servers = mcp_cfg.get("servers")
    if not isinstance(servers, dict):
        return False
    entry = servers.get("code-context")
    if not isinstance(entry, dict):
        return False
    cmd = str(entry.get("command", "")).strip().lower()
    if cmd not in ("wsl.exe", "wsl") and not cmd.endswith("\\wsl.exe"):
        return False
    desired_args = ["--distribution", distribution, "--exec", executable, "shared-mcp"]
    changed = False
    if entry.get("args") != desired_args:
        entry["args"] = desired_args
        changed = True
    if entry.pop("cwd", None) is not None:
        changed = True
    if changed:
        return _write_preserving_jsonc_if_needed(mcp_path, raw, mcp_cfg)
    return False


def configure_settings(settings_path: Path, distribution: str, executable: str) -> bool:
    raw = ""
    if settings_path.is_file():
        try:
            raw = settings_path.read_text(encoding="utf-8-sig")
            settings = load_jsonc(settings_path)
        except (OSError, json.JSONDecodeError):
            return False
    else:
        settings = {}
    if not isinstance(settings, dict):
        return False

    before = json.dumps(settings, sort_keys=True)
    extension_kinds = settings.setdefault("remote.extensionKind", {})
    if not isinstance(extension_kinds, dict):
        extension_kinds = {}
        settings["remote.extensionKind"] = extension_kinds
    extension_kinds.pop(LEGACY_EXT_ID, None)
    extension_kinds[EXT_ID] = ["ui"]
    settings["codeContextNative.wslDistribution"] = distribution
    settings["codeContextNative.wslExecutablePath"] = executable
    after = json.dumps(settings, sort_keys=True)
    if before != after or not settings_path.is_file():
        return _write_preserving_jsonc_if_needed(settings_path, raw, settings)
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
        rest_win = rest.lstrip("/").replace("/", "\\")
        uri_path = f"/{drive}:{rest}/{target_rel}"
        fs_path = f"{drive}:\\{rest_win}\\{target_rel}"
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
    configured_mcp: list[str] = []
    base_settings = args.user_data / "settings.json"
    if configure_settings(base_settings, args.distribution, args.executable):
        configured_paths.append(str(base_settings))
    base_mcp = args.user_data / "mcp.json"
    if configure_mcp_json(base_mcp, args.distribution, args.executable):
        configured_mcp.append(str(base_mcp))

    for pid in profile_ids:
        profile_settings = args.user_data / "profiles" / pid / "settings.json"
        if profile_settings.parent.is_dir() and configure_settings(
            profile_settings, args.distribution, args.executable
        ):
            configured_paths.append(str(profile_settings))
        profile_mcp = args.user_data / "profiles" / pid / "mcp.json"
        if configure_mcp_json(profile_mcp, args.distribution, args.executable):
            configured_mcp.append(str(profile_mcp))

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
                "configured_mcp": configured_mcp,
                "profile_ids": profile_ids,
                "profile_names": profile_names,
                **ext_stats,
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
