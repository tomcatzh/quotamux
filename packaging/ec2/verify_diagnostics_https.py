#!/usr/bin/env python3
"""以明确的假凭据验证最终诊断包的 HTTPS 与八项失败报告；不验证模型推理。"""
import argparse
import json
from pathlib import Path
import stat
import subprocess
import tempfile
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[2]
FAKE = {
    "AWS_ACCESS_KEY_ID": "AKIA0000000000000000",
    "AWS_SECRET_ACCESS_KEY": "quotamux-diagnostic-fake-secret-00000000000",
    "AWS_SESSION_TOKEN": "quotamux-diagnostic-fake-session-token",
    "AWS_BEARER_TOKEN_BEDROCK": "quotamux-diagnostic-fake-bearer-token",
}


def docker(*arguments, timeout=120):
    result = subprocess.run(["docker", *arguments], text=True, capture_output=True, timeout=timeout)
    assert result.returncode == 0, f"Docker 操作失败：{arguments[0]}"
    return result.stdout.strip()


def verify(releases, directory, arch, platform):
    package = directory / arch
    package.mkdir(mode=0o700)
    with zipfile.ZipFile(releases / f"quotamux-0.3.0-bedrock-linux-{arch}.zip") as archive:
        for name in ("quotamux", "quotamux.toml"):
            path = package / name
            path.write_bytes(archive.read(name))
            path.chmod(stat.S_IMODE(archive.getinfo(name).external_attr >> 16))
    (package / "Dockerfile").write_text('FROM scratch\nWORKDIR /reports\nWORKDIR /app\nCOPY quotamux quotamux.toml /app/\nENTRYPOINT ["/app/quotamux"]\n')
    image = "quotamux-final-network-diag:" + arch
    docker("build", "--platform", platform, "--tag", image, str(package))
    reports = releases / "evidence" / arch
    reports.mkdir(mode=0o700, parents=True, exist_ok=True)
    report_name = "https-fake-credentials-" + uuid.uuid4().hex[:12] + ".json"
    command = ["docker", "run", "--read-only", "--platform", platform,
               "--mount", f"type=bind,source={reports},target=/reports"]
    for key, value in {
        **FAKE,
        "RUST_LOG": "trace",
        "AWS_EC2_METADATA_DISABLED": "true",
        "AWS_CONFIG_FILE": "/absent/config",
        "AWS_SHARED_CREDENTIALS_FILE": "/absent/credentials",
        "AWS_ENDPOINT_URL": "http://127.0.0.1:9/ignored",
        "AWS_ENDPOINT_URL_BEDROCK_RUNTIME": "http://127.0.0.1:9/ignored",
        "AWS_ENDPOINT_URL_STS": "http://127.0.0.1:9/ignored",
        "SSL_CERT_FILE": "/absent/ca.pem",
        "SSL_CERT_DIR": "/absent/certs",
    }.items():
        command.extend(["--env", f"{key}={value}"])
    name = "quotamux-final-diag-" + uuid.uuid4().hex[:12]
    command.extend(["--name", name, image, "--config", "/app/quotamux.toml",
                    "--diagnose", "--report", "/reports/" + report_name])
    try:
        result = subprocess.run(command, text=True, capture_output=True, timeout=420)
        assert result.returncode == 2, f"假凭据诊断应返回 2，实际 {result.returncode}"
        path = reports / report_name
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
        report_text = path.read_text()
        for secret in FAKE.values():
            assert secret not in report_text + result.stdout + result.stderr, "假凭据值未脱敏"
        assert "TRACE" not in result.stdout + result.stderr, "诊断模式输出了 SDK 调试日志"
        report = json.loads(report_text)
        assert report["result"] == "failed"
        assert report["identity"]["credentials_status"] == "passed"
        assert report["identity"]["status"] == "failed"
        assert report["identity"]["category"] in {
            "InvalidClientTokenId", "UnrecognizedClientException", "InvalidSignatureException",
            "SignatureDoesNotMatch", "ExpiredToken", "AccessDenied", "AccessDeniedException",
        }, "STS 未返回 AWS 鉴权错误"
        probes = report["model_probes"]
        assert len(probes) == 8
        assert len({(row["model"], row["streaming"]) for row in probes}) == 8
        assert all(row["status"] == "failed" for row in probes)
        assert all(row["http_status"] in (400, 401, 403) for row in probes), "未收到全部 AWS 鉴权响应"
        assert all(row["category"] == "provider_auth" for row in probes)
        assert all(row["upstream_type"] for row in probes), "丢失 AWS 错误类型"
        assert all(row["message"] for row in probes), "丢失 AWS 错误说明"
        assert docker("inspect", "--format", "{{.HostConfig.ReadonlyRootfs}}", name) == "true"
        assert not docker("diff", name), "只读根文件系统发生变化"
        print(f"通过：{arch} 内置 TLS、STS 拒绝后八项继续、AWS 鉴权错误留存、脱敏及端点覆盖隔离", flush=True)
        return {"architecture": arch, "embedded_tls": True, "system_ca_files": False,
                "credentials": "explicitly_fake", "sts_failure_did_not_block_probes": True,
                "probes": 8, "aws_statuses": [row["http_status"] for row in probes],
                "secret_redaction": True, "environment_endpoint_overrides_ignored": True,
                "read_only_root": True, "real_inference": False, "report": str(path.relative_to(releases))}
    finally:
        docker("rm", "--force", name, timeout=20)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--releases", type=Path, default=ROOT / "data/releases/diagnostics")
    args = parser.parse_args()
    releases = args.releases.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix="quotamux-diagnostics-https-", dir=ROOT / "target") as temp:
        for arch, platform in (("arm64", "linux/arm64"), ("x86_64", "linux/amd64")):
            results.append(verify(releases, Path(temp).resolve(), arch, platform))
    path = releases / "evidence" / "diagnostics-https-results.json"
    path.write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
    path.chmod(0o600)


if __name__ == "__main__":
    main()
