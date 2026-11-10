"""按窗口标题找到应用窗口，置前后截图（支持副屏 / 多显示器）。

用法: python tools/capture_window.py "流律" out.png
原因: 本机窗口常开在第二块屏，CopyFromScreen(0,0,1920,1080) 拍不到；
      PowerShell 的 Add-Type 与 COM 被环境策略禁用，故用 ctypes + Pillow。
"""
import ctypes
import sys
import time
from ctypes import wintypes

from PIL import ImageGrab

user32 = ctypes.windll.user32
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)  # PER_MONITOR_AWARE
except Exception:
    user32.SetProcessDPIAware()

WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)


def find_windows(keyword):
    hits = []

    def cb(hwnd, _):
        if not user32.IsWindowVisible(hwnd):
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n <= 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if keyword.lower() in buf.value.lower():
            hits.append(hwnd)
        return True

    user32.EnumWindows(WNDENUMPROC(cb), 0)
    return hits


def main():
    keyword = sys.argv[1] if len(sys.argv) > 1 else "流律"
    out = sys.argv[2] if len(sys.argv) > 2 else "window.png"

    # WebView2 冷启动需要几秒，轮询等窗口出现（不是重试命令，是等外部进程就绪）
    hits = []
    for _ in range(30):
        hits = find_windows(keyword)
        if hits:
            break
        time.sleep(0.5)
    if not hits:
        print(f"未找到标题含 {keyword!r} 的窗口")
        return 1
    hwnd = hits[0]

    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    try:
        user32.SwitchToThisWindow(hwnd, True)
    except Exception:
        pass
    user32.SetForegroundWindow(hwnd)
    user32.SetWindowPos(hwnd, -1, 0, 0, 0, 0, 0x0001 | 0x0002)  # TOPMOST, NOMOVE|NOSIZE
    time.sleep(0.8)

    r = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(r))
    print(f"rect={r.left},{r.top},{r.right},{r.bottom}")
    img = ImageGrab.grab(bbox=(r.left, r.top, r.right, r.bottom), all_screens=True)
    img.save(out)
    print(f"saved={out} size={img.size}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
