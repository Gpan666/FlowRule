"""filter 回归校验：确保引擎实际使用的 filter 在 WinDivert 2.2 NETWORK 层可编译。

背景：WinDivert 2.2 的 WINDIVERT_LAYER_NETWORK **不支持 processId 字段**。
原实现给每个程序拼 `processId == N` 的 filter，导致 WinDivertOpen 必然返回
ERROR_INVALID_PARAMETER(87)，表现为「管理员运行后一启用限速就报错」。
本工具用 dist/WinDivert.dll 的 WinDivertHelperCompileFilter 做纯用户态编译校验，
不需要驱动、不需要管理员，可在任何机器上跑，防止再次引入非法字段。

真实签名（注意有 objLen 参数，少传会崩进程）：
  BOOL WinDivertHelperCompileFilter(LPCSTR filter, UINT32 layer,
      char *object, UINT objLen, const char **errorStr, UINT *errorPos);

用法：python tools/check_filter.py   （退出码 0 = 全部通过）
"""
import ctypes
import os
import re
import sys
from ctypes import wintypes
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DLL = ROOT / "dist" / "WinDivert.dll"
ENGINE_SRC = ROOT / "crates" / "rl-engine-win" / "src" / "lib.rs"
CORE_SRC = ROOT / "crates" / "rl-core" / "src" / "lib.rs"
LAYER_NETWORK = 0

os.add_dll_directory(str(DLL.parent))
dll = ctypes.WinDLL(str(DLL))
fn = dll.WinDivertHelperCompileFilter
fn.argtypes = [wintypes.LPCSTR, wintypes.UINT, ctypes.c_void_p, wintypes.UINT,
               ctypes.POINTER(ctypes.c_char_p), ctypes.POINTER(wintypes.UINT)]
fn.restype = wintypes.BOOL


def compile_ok(f: str, layer: int = LAYER_NETWORK):
    err = ctypes.c_char_p()
    pos = wintypes.UINT()
    ok = fn(f.encode("ascii"), layer, None, 0, ctypes.byref(err), ctypes.byref(pos))
    msg = err.value.decode(errors="replace") if err.value else ""
    return bool(ok), msg, pos.value


def main():
    failures = []

    # 1) 从 rl-core 源码里抽出 base_filter 的两个真实字符串，逐个编译
    core = CORE_SRC.read_text(encoding="utf-8")
    base_filters = [
        (name, f) for name, f in re.findall(r'Direction::(Down|Up)\s*=>\s*"([^"]+)"', core)
        if f.startswith(("inbound", "outbound"))
    ]
    if len(base_filters) != 2:
        failures.append(f"未能从 rl-core 抽出 2 个 base_filter（实际 {len(base_filters)}）")
    for name, f in base_filters:
        ok, msg, pos = compile_ok(f)
        print(f"[{'PASS' if ok else 'FAIL'}] base_filter({name}) len={len(f)} {f!r}"
              + ("" if ok else f" -> {msg} pos={pos}"))
        if not ok:
            failures.append(f"base_filter({name}) 编译失败：{msg} @ {pos}")
        if "processId" in f:
            failures.append(f"base_filter({name}) 含 NETWORK 层不支持的 processId")

    # 2) 静态防线：引擎源码里不得再出现 processId 过滤表达式
    engine = ENGINE_SRC.read_text(encoding="utf-8")
    # 去掉注释行后再查，避免文档里说明性的 processId 触发误报
    code_lines = [ln for ln in engine.splitlines() if not ln.lstrip().startswith("//")]
    code = "\n".join(code_lines)
    if re.search(r'processId\s*(==|!=)', code):
        failures.append("引擎代码里仍存在 processId 过滤表达式")
    else:
        print("[PASS] 引擎代码不含 processId 过滤表达式")

    # 3) 验证 WINDIVERT_ADDRESS 是 80 字节（防止缓冲区溢出回归）
    m = re.search(r"struct Address\(\[u8;\s*(\d+)\]\)", engine)
    if not m or int(m.group(1)) != 80:
        failures.append(f"WINDIVERT_ADDRESS 大小应为 80 字节，实际 {m.group(1) if m else '未知'}")
    else:
        print("[PASS] WINDIVERT_ADDRESS = 80 字节")

    # 3.5) 静态防线：base_filter 不得按网卡过滤
    for name, f in base_filters:
        for tok in ("ifIdx", "loopback"):
            if tok in f:
                failures.append(f"base_filter({name}) 含网卡条件 {tok}：{f}")
    if not any("ifIdx" in f or "loopback" in f for _, f in base_filters):
        print("[PASS] base_filter 不含任何网卡条件（跨网卡去重改由用户态保证）")

    # 4) 反例自检：确认 DLL 确实会拒绝 processId（保证这个校验有判别力）
    #    注意：这里的「合法部分」必须用引擎当前真实的 filter 语义，否则反例可能
    #    因为别的语法问题被拒绝，看不出 processId 本身非法。
    #    ⚠️ filter 里**不许再出现任何网卡条件**（ifIdx / loopback）：按网卡猜一次，
    #       猜错的方向就是「流量根本不进句柄」，比限多限少严重得多。
    #       同一个包跨网卡只限一次，改由引擎在用户态按包内 IP 去重（见 Dedupe）。
    bad = "inbound and (ip or ipv6) and (tcp or udp) and not impostor and (processId == 4242)"
    ok, msg, pos = compile_ok(bad)
    if ok:
        failures.append("反例自检失败：含 processId 的 filter 竟然编译通过，校验失去意义")
    else:
        print(f"[PASS] 反例被正确拒绝：{msg} @ pos={pos}")

    if failures:
        print("\nRESULT: FAIL")
        for f in failures:
            print("  -", f)
        return 1
    print("\nRESULT: PASS — 所有引擎 filter 均可在 NETWORK 层编译")
    return 0


if __name__ == "__main__":
    sys.exit(main())
