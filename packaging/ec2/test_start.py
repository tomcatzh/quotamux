#!/usr/bin/env python3
"""启动包装脚本的隔离测试，不调用真实程序或 AWS。"""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time


SOURCE = Path(__file__).resolve().parent
FAKE = '''#!/usr/bin/env python3
import json, os, signal, sys, time
from pathlib import Path
root = Path(__file__).resolve().parent
root.joinpath("captured.json").write_text(json.dumps({"args":sys.argv[1:], "cwd":os.getcwd(), "log":os.environ["RUST_LOG"], "backtrace":os.environ["RUST_BACKTRACE"], "data":os.environ["QUOTAMUX_DATA_DIR"]}))
print("假程序日志", flush=True)
if "--diagnose" in sys.argv:
    index = sys.argv.index("--report") if "--report" in sys.argv else None
    report = Path(sys.argv[index + 1]) if index is not None else root / "logs/diagnostics-fake.json"
    with report.open("x") as stream:
        json.dump({"schema_version":1,"result":"failed","category":"fake-credentials"}, stream)
    print("诊断发现问题；报告：" + str(report), flush=True)
    sys.exit(2)
if "--hold" in sys.argv:
    def stop(signum, frame):
        print("收到退出信号", flush=True)
        sys.exit(17)
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    root.joinpath("ready").touch()
    while True: time.sleep(0.05)
sys.exit(0 if "--check" in sys.argv else 23)
'''


def main():
    with tempfile.TemporaryDirectory(prefix="quotamux-launcher-") as temp:
        root = Path(temp).resolve() / "解压 路径"
        root.mkdir()
        for name in ["start.sh", "quotamux.toml"]:
            shutil.copy2(SOURCE / name, root / name)
        (root / "quotamux").write_text(FAKE)
        (root / "quotamux").chmod(0o755)
        env = dict(os.environ)
        for name in ["RUST_LOG", "RUST_BACKTRACE", "QUOTAMUX_DATA_DIR"]:
            env.pop(name, None)
        run = subprocess.run([str(root / "start.sh")], cwd="/", env=env, text=True, capture_output=True, timeout=5)
        assert run.returncode == 23, run
        capture = json.loads((root / "captured.json").read_text())
        assert capture == {"args":["--config", str(root / "quotamux.toml")], "cwd":str(root.resolve()), "log":"trace", "backtrace":"full", "data":str(root / "data")}, capture
        logs = list((root / "logs").glob("*.log"))
        assert len(logs) == 1
        assert "假程序日志" in logs[0].read_text()
        assert logs[0].stat().st_mode & 0o077 == 0
        assert not list((root / "logs").glob(".pipe.*"))
        run = subprocess.run([str(root / "start.sh"), "--check"], cwd="/", env=env, text=True, capture_output=True, timeout=5)
        assert run.returncode == 0, run
        assert json.loads((root / "captured.json").read_text())["args"][-1] == "--check"
        # 旧目录权限较宽时，脚本收紧项目日志目录；诊断失败仍完整保留报告。
        (root / "logs").chmod(0o755)
        shutil.rmtree(root / "data")
        report = root / "logs/diagnostics-explicit.json"
        run = subprocess.run([str(root / "start.sh"), "--diagnose", "--report", str(report)], cwd="/", env=env, text=True, capture_output=True, timeout=5)
        assert run.returncode == 2, run
        assert json.loads(report.read_text())["result"] == "failed"
        assert report.stat().st_mode & 0o077 == 0
        assert (root / "logs").stat().st_mode & 0o077 == 0
        assert not (root / "data").exists()
        assert json.loads((root / "captured.json").read_text())["args"][-3:] == ["--diagnose", "--report", str(report)]
        assert str(report) in run.stdout
        assert any("诊断发现问题" in path.read_text() for path in (root / "logs").glob("*.log"))
        assert not list((root / "logs").glob(".pipe.*"))
        process = subprocess.Popen([str(root / "start.sh"), "--hold"], cwd="/", env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 5
            while not (root / "ready").exists():
                assert time.monotonic() < deadline, "假程序未启动"
                time.sleep(0.01)
            process.send_signal(signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=5)
            assert process.returncode == 17, (process.returncode, stdout, stderr)
            assert "收到退出信号" in stdout
            assert not list((root / "logs").glob(".pipe.*"))
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()
        print("通过：异目录/空格路径、TRACE/backtrace、私有日志、退出码、诊断报告留存、无诊断数据写入、--check、TERM 转发及管道清理")


if __name__ == "__main__":
    main()
