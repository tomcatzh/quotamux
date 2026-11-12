#!/usr/bin/env bash
# 前台运行；终端与 logs/ 同时保留最大级别调试日志。
# 专用静态程序内置 Mozilla CA 根证书（MPL-2.0）；证书源码 https://curl.se/ca/cacert.pem 。
set -u
set -o pipefail
umask 077

root_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)" || exit 1
cd -- "$root_dir" || exit 1
binary="$root_dir/quotamux"
config="$root_dir/quotamux.toml"
if [[ ! -x "$binary" || ! -r "$config" ]]; then
    printf '启动失败：请将 quotamux、quotamux.toml 和 start.sh 解压在同一目录，并保留执行权限。\n' >&2
    exit 1
fi
export QUOTAMUX_DATA_DIR="${QUOTAMUX_DATA_DIR:-$root_dir/data}"
export RUST_LOG="${RUST_LOG:-trace}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-full}"
log_dir="$root_dir/logs"
diagnose=0
for argument in "$@"; do
    if [[ "$argument" == "--diagnose" ]]; then
        diagnose=1
        break
    fi
done
if [[ -L "$log_dir" ]]; then
    printf '启动失败：logs 必须为解压目录内的真实目录。\n' >&2
    exit 1
fi
mkdir -p -- "$log_dir" || exit 1
chmod 700 "$log_dir" || exit 1
if [[ "$diagnose" -eq 0 ]]; then
    mkdir -p -- "$QUOTAMUX_DATA_DIR" || exit 1
fi
log_file="$log_dir/quotamux-$(date -u +%Y%m%dT%H%M%SZ)-$$.log"
pipe_dir="$(mktemp -d "$log_dir/.pipe.XXXXXX")" || exit 1
fifo="$pipe_dir/output"
app_pid=""
tee_pid=""
wait_interrupted=0
cleanup() {
    rm -f -- "$fifo"
    rmdir -- "$pipe_dir" 2>/dev/null || :
}
forward_signal() {
    wait_interrupted=1
    if [[ -n "$app_pid" ]]; then
        kill -s "$1" "$app_pid" 2>/dev/null || :
    fi
}
trap cleanup EXIT
trap 'forward_signal TERM' TERM
trap 'forward_signal INT' INT
mkfifo -- "$fifo" || exit 1
printf '配置：%s\n数据：%s\n日志：%s\n日志级别：%s；按 Ctrl+C 停止。\n' "$config" "$QUOTAMUX_DATA_DIR" "$log_file" "$RUST_LOG" | tee -a "$log_file"
if [[ "$diagnose" -eq 1 ]]; then
    printf '完整诊断：自动汇总身份、原始失败及四款模型的普通/流式探测，可能产生少量模型用量。\n完成后回传终端显示的诊断 JSON 报告即可。\n' | tee -a "$log_file"
fi
tee -a "$log_file" < "$fifo" &
tee_pid=$!
"$binary" --config "$config" "$@" > "$fifo" 2>&1 &
app_pid=$!
while :; do
    wait_interrupted=0
    wait "$app_pid"
    app_status=$?
    # wait 可能被脚本收到的信号中断；子进程仍存活时继续等待。
    [[ "$wait_interrupted" -eq 1 ]] || break
done
wait "$tee_pid"
tee_status=$?
if [[ "$app_status" -eq 0 && "$tee_status" -ne 0 ]]; then
    printf '日志写入失败。\n' >&2
    exit "$tee_status"
fi
exit "$app_status"
