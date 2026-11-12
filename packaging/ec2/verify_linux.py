#!/usr/bin/env python3
"""在隔离 Linux 容器中验证最终 ZIP，无真实 AWS 凭据或模型推理。"""
import json
from pathlib import Path
import stat
import subprocess
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "data/releases"
RUST_IMAGE = "rust:1.97.1-alpine3.23@sha256:c4a364ddbf684fe038e6fa6a4f25b30c8dc85247423e0e660676ece0d17be4a2"


def docker(*args, timeout=60):
    return subprocess.run(["docker", *args], capture_output=True, text=True, check=True, timeout=timeout).stdout.strip()


def http(path, body=None):
    data = None if body is None else json.dumps(body).encode()
    request = Request("http://127.0.0.1:8081" + path, data=data, headers={"Content-Type":"application/json"})
    try:
        with urlopen(request, timeout=30) as response:
            return response.status, json.load(response)
    except HTTPError as error:
        return error.code, json.load(error)


def wait_ready():
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        try:
            status, _ = http("/healthz")
            if status == 200:
                return
        except (URLError, ConnectionError, TimeoutError):
            pass
        time.sleep(0.2)
    raise RuntimeError("隔离 8081 未就绪")


def main():
    results = []
    for archive_arch, docker_arch in [("arm64", "arm64"), ("x86_64", "amd64")]:
        archive = OUT / f"quotamux-0.3.0-bedrock-linux-{archive_arch}.zip"
        directory = ROOT / "target/ec2-smoke" / archive_arch
        directory.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(archive) as stream:
            assert stream.namelist() == ["quotamux", "quotamux.toml", "start.sh"]
            for info in stream.infolist():
                path = directory / info.filename
                path.write_bytes(stream.read(info))
                path.chmod(stat.S_IMODE(info.external_attr >> 16))
        # 仅修改测试副本，让 Docker 的本地端口转发能访问容器；ZIP 保持 localhost。
        config = directory / "quotamux.toml"
        config.write_text(config.read_text().replace('listen = "127.0.0.1:8081"', 'listen = "0.0.0.0:8081"'))
        dockerfile = directory / "Dockerfile"
        dockerfile.write_text('FROM scratch\nWORKDIR /app\nCOPY quotamux quotamux.toml /app/\nENTRYPOINT ["/app/quotamux"]\nCMD ["--config","/app/quotamux.toml"]\n')
        image = f"quotamux-ec2-check:{archive_arch}"
        docker("build", "--platform", f"linux/{docker_arch}", "--tag", image, str(directory), timeout=120)
        check = docker("run", "--rm", "--network", "none", "--platform", f"linux/{docker_arch}", image, "--config", "/app/quotamux.toml", "--check")
        assert "configuration valid" in check
        print(f"通过：{archive_arch} 空文件系统、无网络配置检查", flush=True)
        name = "quotamux-ec2-smoke-" + uuid.uuid4().hex[:12]
        try:
            docker("run", "--detach", "--name", name, "--platform", f"linux/{docker_arch}",
                   "--read-only", "--tmpfs", "/app/data:rw,nosuid,nodev,size=64m,mode=0700",
                   "--publish", "127.0.0.1:8081:8081",
                   "--env", "RUST_LOG=trace", "--env", "RUST_BACKTRACE=full",
                   "--env", "SSL_CERT_FILE=/missing/ca.pem", "--env", "SSL_CERT_DIR=/missing/certs",
                   "--env", "AWS_EC2_METADATA_DISABLED=true", "--env", "AWS_ACCESS_KEY_ID=AKIAQUOTAMUXTEST00000",
                   "--env", "AWS_SECRET_ACCESS_KEY=quotamux-test-secret-not-real-000000000000",
                   "--env", "AWS_SESSION_TOKEN=quotamux-fake-session-token", image)
            wait_ready()
            status, models = http("/v1/models")
            assert status == 200 and len(models["data"]) == 4
            status, reply = http("/v1/messages", {"model":"claude-haiku-5-5", "max_tokens":1,
                                                   "messages":[{"role":"user","content":"quotamux-tls-smoke"}]})
            assert status >= 400, reply
            _, attempts = http("/api/attempts?limit=10")
            row = attempts["attempts"][0]
            assert row["status"] in (400, 401, 403), row
            assert row["error_class"] == "provider_auth", row
            # 收到真实 AWS 鉴权响应证明：在没有 CA 文件的 scratch 中，TLS 验证与请求签名完成。
            print(f"通过：{archive_arch} 健康检查、四模型目录、内置 TLS（AWS 拒绝假凭据 {row['status']}）", flush=True)
            docker("stop", "--time", "10", name, timeout=20)
            assert docker("inspect", "--format", "{{.State.ExitCode}}", name) == "0"
            logs = docker("logs", name)
            assert '"level":"TRACE"' in logs
            (OUT / f"smoke-{archive_arch}.log").write_text(logs + "\n")
        finally:
            subprocess.run(["docker", "rm", "--force", name], capture_output=True, timeout=20)

        dockerfile.write_text(f'FROM {RUST_IMAGE}\nRUN command -v bash || apk add --no-cache bash\nWORKDIR /package\nCOPY quotamux quotamux.toml start.sh /package/\nENTRYPOINT ["/bin/bash","/package/start.sh"]\n')
        launcher_image = f"quotamux-ec2-launcher-check:{archive_arch}"
        docker("build", "--platform", f"linux/{docker_arch}", "--tag", launcher_image, str(directory), timeout=120)
        name = "quotamux-ec2-launcher-" + uuid.uuid4().hex[:12]
        try:
            docker("run", "--detach", "--name", name, "--network", "none", "--platform", f"linux/{docker_arch}", launcher_image)
            deadline = time.monotonic() + 15
            while "QuotaMux listening" not in docker("logs", name):
                assert time.monotonic() < deadline
                time.sleep(0.2)
            docker("kill", "--signal", "INT", name)
            assert docker("wait", name, timeout=15) == "0"
            logs = docker("logs", name)
            assert "日志级别：trace" in logs
            check = docker("run", "--rm", "--network", "none", "--platform", f"linux/{docker_arch}", launcher_image, "--check")
            assert "configuration valid" in check
            (OUT / f"launcher-{archive_arch}.log").write_text(logs + "\n")
        finally:
            subprocess.run(["docker", "rm", "--force", name], capture_output=True, timeout=20)
        print(f"通过：{archive_arch} 真实启动脚本、TRACE 日志、--check 与 Ctrl+C 干净退出", flush=True)
        results.append({"architecture":archive_arch, "static_scratch_check":True, "models":4,
                        "embedded_tls_aws_fake_credentials_status":row["status"], "real_inference":False,
                        "trace_logging":True, "launcher_sigint_exit":0})
    (OUT / "linux-smoke-results.json").write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
