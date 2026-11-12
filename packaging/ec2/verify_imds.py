#!/usr/bin/env python3
"""验证最终 Linux 静态程序的默认 IMDSv2 凭据链；不使用真实凭据或模型推理。"""
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import subprocess
import threading
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
import uuid

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "data/releases"
TOKEN = "quotamux-imds-only-fake-token"
ACCESS = "AKIAQUOTAMUXTEST00000"
SECRET = "quotamux-imds-fake-secret-not-real-000000000"
SESSION = "quotamux-imds-fake-session"
ROLE_PATH = "/latest/meta-data/iam/security-credentials/"


def docker(*args, timeout=30):
    result = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError("Docker 测试操作失败：" + args[0])
    return result.stdout.strip()


class MetadataServer:
    def __init__(self, missing_role=False):
        self.events = []
        self.invalid = []
        self.lock = threading.Lock()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            timeout = 5

            def log_message(self, *args):
                pass

            def answer(self, status, value):
                data = value.encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json" if value.startswith("{") else "text/plain")
                self.send_header("Content-Length", str(len(data)))
                if self.command == "PUT" and status == 200:
                    self.send_header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
                self.end_headers()
                self.wfile.write(data)

            def do_PUT(self):
                with owner.lock:
                    owner.events.append(("PUT", self.path))
                try:
                    ttl = int(self.headers.get("X-aws-ec2-metadata-token-ttl-seconds", "0"))
                except ValueError:
                    ttl = 0
                if self.path != "/latest/api/token" or not 1 <= ttl <= 21600:
                    with owner.lock:
                        owner.invalid.append("token 请求路径或 TTL 无效")
                    self.answer(400, "invalid")
                    return
                self.answer(200, TOKEN)

            def do_GET(self):
                with owner.lock:
                    owner.events.append(("GET", self.path))
                if self.headers.get("X-aws-ec2-metadata-token") != TOKEN:
                    with owner.lock:
                        owner.invalid.append("GET 缺少有效 IMDSv2 token")
                    self.answer(401, "invalid")
                    return
                if self.path == ROLE_PATH:
                    self.answer(404 if missing_role else 200, "" if missing_role else "fake-role")
                elif self.path == ROLE_PATH + "fake-role" and not missing_role:
                    expiration = (datetime.now(timezone.utc) + timedelta(hours=1)).strftime("%Y-%m-%dT%H:%M:%SZ")
                    self.answer(200, json.dumps({"Code":"Success", "LastUpdated":datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                        "Type":"AWS-HMAC", "AccessKeyId":ACCESS, "SecretAccessKey":SECRET, "Token":SESSION, "Expiration":expiration}))
                else:
                    with owner.lock:
                        owner.invalid.append("未预期的 metadata 路径")
                    self.answer(404, "")

        self.server = ThreadingHTTPServer(("0.0.0.0", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def http(port, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    request = Request(f"http://127.0.0.1:{port}" + path, data=data, headers={"Content-Type":"application/json"})
    try:
        with urlopen(request, timeout=40) as response:
            return response.status, json.load(response)
    except HTTPError as error:
        return error.code, json.load(error)


def run_case(arch, platform, mode):
    metadata = MetadataServer(missing_role=mode == "missing-role")
    name = "quotamux-imds-" + uuid.uuid4().hex[:12]
    try:
        endpoint = f"http://host.docker.internal:{metadata.server.server_port}"
        args = ["run", "--detach", "--name", name, "--platform", platform, "--read-only",
            "--tmpfs", "/app/data:rw,nosuid,nodev,size=64m,mode=0700",
            "--publish", "127.0.0.1::8081", "--env", "RUST_LOG=trace",
            "--env", "AWS_EC2_METADATA_SERVICE_ENDPOINT=" + endpoint,
            "--env", "AWS_EC2_METADATA_DISABLED=" + ("true" if mode == "disabled" else "false"),
            "--env", "AWS_SHARED_CREDENTIALS_FILE=/nonexistent/credentials",
            "--env", "AWS_CONFIG_FILE=/nonexistent/config"]
        if mode == "environment-priority":
            args.extend(["--env", "AWS_ACCESS_KEY_ID=" + ACCESS, "--env", "AWS_SECRET_ACCESS_KEY=" + SECRET,
                         "--env", "AWS_SESSION_TOKEN=" + SESSION])
        args.append("quotamux-ec2-check:" + arch)
        docker(*args)
        address = docker("port", name, "8081/tcp")
        port = int(address.splitlines()[0].rsplit(":", 1)[1])
        deadline = time.monotonic() + 20
        while True:
            try:
                if http(port, "/healthz")[0] == 200:
                    break
            except (URLError, TimeoutError, ConnectionError):
                pass
            if time.monotonic() > deadline:
                raise RuntimeError("隔离网关未及时就绪")
            time.sleep(0.1)
        status, _ = http(port, "/v1/messages", {"model":"claude-haiku-5-5", "max_tokens":1,
            "messages":[{"role":"user", "content":"quotamux-fake-imds-auth-only"}]})
        assert status >= 400, "假身份不应成功推理"
        _, attempts = http(port, "/api/attempts?limit=10")
        row = attempts["attempts"][0]
        with metadata.lock:
            events = list(metadata.events)
            invalid = list(metadata.invalid)
        assert not invalid, "IMDSv2 请求验证失败"
        assert row["error_class"] == "provider_auth", "凭据故障应归类为鉴权故障"
        if mode == "role":
            assert ("PUT", "/latest/api/token") in events, "默认链没有请求 IMDSv2 token"
            assert ("GET", ROLE_PATH) in events, f"默认链没有查询 Role；请求序列={events}"
            assert ("GET", ROLE_PATH + "fake-role") in events, "默认链没有获取 Role 凭据"
            assert row["status"] in (400, 401, 403), "AWS 未返回预期鉴权拒绝"
        elif mode == "missing-role":
            assert ("GET", ROLE_PATH) in events
            assert not any(path == ROLE_PATH + "fake-role" for _, path in events)
            assert row["status"] is None, "缺少 Role 不应发送已签名调用"
        elif mode in ("disabled", "environment-priority"):
            assert not events, "此场景不应访问 IMDS"
            if mode == "disabled":
                assert row["status"] is None
            else:
                assert row["status"] in (400, 401, 403)
        result = {"architecture":arch, "scenario":mode, "gateway_status":status,
            "upstream_status":row["status"], "classification":row["error_class"],
            "imds_requests":[{"method":method, "path":path} for method,path in events],
            "imds_v2_token_validation":not invalid, "real_credentials":False, "model_inference":False}
        print(f"通过：{arch} {mode}；鉴权分类={row['error_class']}，IMDS 请求数={len(events)}，上游状态={row['status']}", flush=True)
        return result
    finally:
        subprocess.run(["docker", "rm", "--force", name], capture_output=True, timeout=20)
        metadata.close()


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    results = []
    try:
        for arch, platform, mode in [
            ("arm64", "linux/arm64", "role"),
            ("x86_64", "linux/amd64", "role"),
            ("arm64", "linux/arm64", "missing-role"),
            ("arm64", "linux/arm64", "disabled"),
            ("arm64", "linux/arm64", "environment-priority"),
        ]:
            results.append(run_case(arch, platform, mode))
    finally:
        path = OUT / "imds-mock-results.json"
        path.write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
        path.chmod(0o600)


if __name__ == "__main__":
    main()
