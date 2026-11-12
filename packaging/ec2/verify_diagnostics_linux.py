#!/usr/bin/env python3
"""对三文件 ZIP 的真实 Linux 静态程序做离线诊断验收；不验证 AWS 推理。"""
import argparse
import json
from pathlib import Path
import stat
import subprocess
import tempfile
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[2]
SECRET = "synthetic-diagnostic-secret-must-not-leak"
MODELS = {"global.anthropic.claude-" + name for name in ("opus-5-5", "sonnet-5-5", "haiku-5-5", "fable-5-2")}


def docker(*args, timeout=120):
    result = subprocess.run(["docker", *args], text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError("Docker 测试准备失败：" + args[0])
    return result.stdout.strip()


def invocation(image, platform, args, mounts, expected, check_logs=None, allow_platform_ca=False):
    name = "quotamux-diag-" + uuid.uuid4().hex[:12]
    command = ["docker", "run", "--name", name, "--platform", platform, "--network", "none",
        "--read-only", "--env", "AWS_EC2_METADATA_DISABLED=true",
        "--env", "AWS_CONFIG_FILE=/absent/config", "--env", "AWS_SHARED_CREDENTIALS_FILE=/absent/credentials"]
    for source, destination, readonly in mounts:
        command.extend(["--mount", f"type=bind,source={source},target={destination}" + (",readonly" if readonly else "")])
    command.extend([image, *args])
    try:
        result = subprocess.run(command, text=True, capture_output=True, timeout=60)
        assert result.returncode == expected, f"诊断退出码 {result.returncode}，预期 {expected}"
        assert SECRET not in result.stdout + result.stderr, "命令输出泄漏合成秘密"
        changes = docker("diff", name)
        assert docker("inspect", "--format", "{{.HostConfig.ReadonlyRootfs}}", name) == "true"
        # OrbStack 在带系统证书目录的 Alpine 中注入自身 CA；scratch 不允许例外。
        ca_paths = {"/usr", "/usr/local", "/usr/local/share", "/usr/local/share/ca-certificates",
            "/usr/local/share/ca-certificates/orbstack-root.crt", "/etc", "/etc/ssl", "/etc/ssl/certs",
            "/etc/ssl/certs/orbstack-root.crt", "/etc/ssl/certs/ca-certificates.crt", "/etc/ca-certificates",
            "/etc/ca-certificates/trust-source", "/etc/ca-certificates/trust-source/anchors",
            "/etc/ca-certificates/trust-source/anchors/orbstack-root.crt"}
        unexpected = [line for line in changes.splitlines() if not allow_platform_ca or line[2:] not in ca_paths]
        assert not unexpected, "只读根文件系统发生变化：" + "\n".join(unexpected)
        if check_logs is not None:
            check_logs(result)
        return result
    finally:
        cleanup = subprocess.run(["docker", "rm", "--force", name], text=True, capture_output=True, timeout=20)
        assert cleanup.returncode == 0, "本次诊断容器清理失败"


def report(path):
    assert stat.S_IMODE(path.stat().st_mode) == 0o600, "报告权限不是 0600"
    text = path.read_text()
    assert SECRET not in text, "报告泄漏合成秘密"
    value = json.loads(text)
    assert value["schema_version"] == 1
    assert value["result"] == "failed"
    return value


def verify_archive(releases, arch, platform, directory):
    archive = releases / f"quotamux-0.3.0-bedrock-linux-{arch}.zip"
    package = directory / arch
    package.mkdir(mode=0o700)
    with zipfile.ZipFile(archive) as stream:
        assert stream.namelist() == ["quotamux", "quotamux.toml", "start.sh"]
        for info in stream.infolist():
            path = package / info.filename
            path.write_bytes(stream.read(info))
            path.chmod(stat.S_IMODE(info.external_attr >> 16))
    # 非法配置只含合成秘密，不带任何真实凭据。
    (package / "bad.toml").write_text('api_key = "' + SECRET + '"\nthis is invalid [\n')
    (package / "Dockerfile").write_text('FROM scratch\nWORKDIR /reports\nWORKDIR /app\nCOPY quotamux quotamux.toml bad.toml /app/\nENTRYPOINT ["/app/quotamux"]\n')
    image = "quotamux-diag-check:" + arch
    docker("build", "--platform", platform, "--tag", image, str(package))
    reports = package / "reports"
    reports.mkdir(mode=0o700)
    mount = [(reports, "/reports", False)]
    base = ["--config", "/app/quotamux.toml"]
    invocation(image, platform, base + ["--diagnose", "--report", "/reports/offline.json"], mount, 2)
    offline = report(reports / "offline.json")
    assert offline["os"] == "linux" and offline["standalone_tls"] is True
    assert offline["identity"]["status"] == "failed"
    probes = offline["model_probes"]
    assert len(probes) == 8 and all(row["status"] == "skipped" for row in probes)
    assert {(row["model"], row["streaming"]) for row in probes} == {(model, streaming) for model in MODELS for streaming in (False, True)}
    assert set(offline["config"]["bedrock"][0]["upstream_models"]) == MODELS
    aliases = {alias for model in offline["config"]["served_models"] for alias in model["aliases"]}
    assert MODELS <= aliases
    assert offline["environment"]["set"]["AWS_ACCESS_KEY_ID"] is False
    assert offline["environment"]["metadata_disabled"] is True
    initial = (reports / "offline.json").read_bytes()
    invocation(image, platform, base + ["--diagnose", "--report", "/reports/offline.json"], mount, 1)
    assert (reports / "offline.json").read_bytes() == initial
    (reports / "symlink.json").symlink_to("/reports/offline.json")
    invocation(image, platform, base + ["--diagnose", "--report", "/reports/symlink.json"], mount, 1)
    assert (reports / "offline.json").read_bytes() == initial
    invocation(image, platform, ["--config", "/app/bad.toml", "--diagnose", "--report", "/reports/invalid.json"], mount, 2)
    invalid = report(reports / "invalid.json")
    assert invalid["config"]["category"] == "config_parse"
    assert invalid["identity"]["status"] == "skipped"
    assert not invalid["model_probes"]
    invocation(image, platform, ["--help"], mount, 0)
    invocation(image, platform, base + ["--check"], mount, 0)
    invocation(image, platform, base + ["--check", "--diagnose"], mount, 1)
    invocation(image, platform, base + ["--report", "/reports/invalid-cli.json"], mount, 1)
    assert not (reports / "invalid-cli.json").exists()
    assert not (reports / "data").exists()

    # 复用已有带 Bash 的测试镜像，覆盖成新 ZIP 的三文件。
    (package / "Dockerfile").write_text(f'FROM quotamux-ec2-launcher-check:{arch}\nWORKDIR /reports\nWORKDIR /package/logs\nWORKDIR /package/data\nWORKDIR /package\nCOPY quotamux quotamux.toml start.sh /package/\nENTRYPOINT ["/bin/bash","/package/start.sh"]\n')
    launcher = "quotamux-diag-launcher-check:" + arch
    docker("build", "--platform", platform, "--tag", launcher, str(package))
    logs = package / "logs"
    data = package / "data"
    logs.mkdir(mode=0o700)
    data.mkdir(mode=0o700)
    invocation(launcher, platform, ["--diagnose", "--report", "/reports/launcher.json"],
        [*mount, (logs, "/package/logs", False), (data, "/package/data", True)], 2, allow_platform_ca=True)
    report(reports / "launcher.json")
    log_files = list(logs.glob("*.log"))
    assert log_files and all(stat.S_IMODE(path.stat().st_mode) == 0o600 for path in log_files)
    assert all(SECRET not in path.read_text() for path in log_files)
    assert not list(logs.glob(".pipe.*")), "启动脚本遗留日志管道"
    assert not list(data.iterdir()), "诊断创建了 Store 数据"
    print(f"通过：{arch} 静态离线诊断、0600 报告、八项 skipped、脱敏、拒绝覆盖、CLI 及启动脚本", flush=True)
    return {"architecture":arch, "archive":archive.name, "scratch_read_only":True,
        "network":"none", "credentials":"none", "report_schema":1, "report_mode":"0600",
        "global_models":4, "skipped_probes":8, "identity":"failed", "invalid_config_redacted":True,
        "existing_report_protected":True, "symlink_report_protected":True, "cli_checks":True,
        "launcher_exit":2, "launcher_log":True, "store_files_created":False, "aws_inference_verified":False}


def main():
    parser = argparse.ArgumentParser(description="对两架构 ZIP 做无网络诊断验收")
    parser.add_argument("--releases", type=Path, default=ROOT / "data/releases/diagnostics")
    args = parser.parse_args()
    releases = args.releases.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix="quotamux-diagnostics-", dir=ROOT / "target") as temp:
        directory = Path(temp).resolve()
        for arch, platform in (("arm64", "linux/arm64"), ("x86_64", "linux/amd64")):
            results.append(verify_archive(releases, arch, platform, directory))
    evidence = releases / "evidence"
    evidence.mkdir(mode=0o700, exist_ok=True)
    path = evidence / "diagnostics-linux-results.json"
    path.write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
    path.chmod(0o600)


if __name__ == "__main__":
    main()
