#!/usr/bin/env python3
"""将已验证的两架构静态程序打包成各含三个文件的 EC2 ZIP。"""
import argparse
import hashlib
import json
from pathlib import Path
import stat
import struct
import time
import tomllib
import zipfile


def validate_elf(path: Path, machine: int) -> bytes:
    data = path.read_bytes()
    if len(data) < 64 or data[:6] != b"\x7fELF\x02\x01":
        raise ValueError(f"不是 64 位小端 ELF：{path}")
    if struct.unpack_from("<H", data, 18)[0] != machine:
        raise ValueError(f"ELF 架构不符：{path}")
    table = struct.unpack_from("<Q", data, 32)[0]
    entry_size, count = struct.unpack_from("<HH", data, 54)
    if entry_size < 56 or count == 0 or table + entry_size * count > len(data):
        raise ValueError(f"ELF 程序头无效：{path}")
    for index in range(count):
        offset = table + entry_size * index
        kind = struct.unpack_from("<I", data, offset)[0]
        if kind == 3:
            raise ValueError(f"程序仍依赖动态加载器：{path}")
        if kind == 2:
            file_offset = struct.unpack_from("<Q", data, offset + 8)[0]
            file_size = struct.unpack_from("<Q", data, offset + 32)[0]
            if file_offset + file_size > len(data):
                raise ValueError(f"ELF 动态段无效：{path}")
            for position in range(file_offset, file_offset + file_size, 16):
                tag = struct.unpack_from("<q", data, position)[0]
                if tag == 0:
                    break
                if tag == 1:
                    raise ValueError(f"程序仍依赖共享库：{path}")
    return data


def package(binaries: Path, output: Path) -> None:
    source = Path(__file__).resolve().parent
    config = (source / "quotamux.toml").read_bytes()
    script = (source / "start.sh").read_bytes()
    document = tomllib.loads(config.decode())
    assert document["server"]["listen"] == "127.0.0.1:8081"
    assert document["backends"][0]["region"] == "us-east-1"
    assert len(document["backends"][0]["models"]) == 4
    for model in document["models"]:
        assert "global.anthropic." + model["name"] in model["aliases"]
    assert len(document["backends"][0]["credentials"]) == 1
    assert not document["backends"][0]["credentials"][0].get("api_key")
    assert b'RUST_LOG:-trace' in script
    output.mkdir(parents=True, exist_ok=True)
    timestamp = time.gmtime()[:6]
    manifest = []
    for platform, name, machine in [
        ("linux_arm64", "arm64", 183),
        ("linux_amd64", "x86_64", 62),
    ]:
        binary = validate_elf(binaries / platform / "quotamux", machine)
        archive = output / f"quotamux-0.3.0-bedrock-linux-{name}.zip"
        files = [("quotamux", binary, 0o755), ("quotamux.toml", config, 0o600), ("start.sh", script, 0o755)]
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as stream:
            for filename, data, mode in files:
                info = zipfile.ZipInfo(filename, timestamp)
                info.create_system = 3
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = (stat.S_IFREG | mode) << 16
                stream.writestr(info, data, compresslevel=9)
        with zipfile.ZipFile(archive) as stream:
            assert stream.namelist() == [item[0] for item in files]
            assert stream.testzip() is None
            for filename, data, mode in files:
                assert stream.read(filename) == data
                assert stat.S_IMODE(stream.getinfo(filename).external_attr >> 16) == mode
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        manifest.append({"file": archive.name, "bytes": archive.stat().st_size, "sha256": digest,
                         "binary_sha256": hashlib.sha256(binary).hexdigest(), "files": [item[0] for item in files]})
        print(f"通过：{archive.name}，{archive.stat().st_size:,} 字节，静态 ELF 与三个文件检查完成")
    (output / "package-manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    (output / "SHA256SUMS").write_text("".join(f"{item['sha256']}  {item['file']}\n" for item in manifest))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, default=Path("target/ec2-static"))
    parser.add_argument("--output", type=Path, default=Path("data/releases"))
    args = parser.parse_args()
    package(args.binaries, args.output)
