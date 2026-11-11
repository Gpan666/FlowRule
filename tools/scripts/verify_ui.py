"""启动 dist/rl-app.exe，把窗口提到前台并截图，最后结束进程（验证专用，无残留）。

注意两个坑：
1. WebView2 用 DirectComposition 硬件渲染，屏幕 BitBlt / ImageGrab 常抓到桌面壁纸，
   必须用 PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT) 直接抓窗口。
2. ctypes 默认把 int 当 32 位，HWND/HDC 在 64 位下必须显式声明 argtypes/restype。
"""
import ctypes
import subprocess
import sys
import time
from ctypes import wintypes
from pathlib import Path

from PIL import Image, ImageGrab

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
EXE = DIST / "rl-app.exe"

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
kernel32 = ctypes.windll.kernel32
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except Exception:
    user32.SetProcessDPIAware()

WNDENUMPROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)

user32.GetForegroundWindow.argtypes = []
user32.GetForegroundWindow.restype = wintypes.HWND
user32.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
user32.GetWindowThreadProcessId.restype = wintypes.DWORD
user32.AttachThreadInput.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.BOOL]
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.BringWindowToTop.argtypes = [wintypes.HWND]
user32.ShowWindow.argtypes = [wintypes.HWND, ctypes.c_int]
user32.SetWindowPos.argtypes = [wintypes.HWND, wintypes.HWND, ctypes.c_int,
                               ctypes.c_int, ctypes.c_int, ctypes.c_int, wintypes.UINT]
user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.GetDC.argtypes = [wintypes.HWND]
user32.GetDC.restype = wintypes.HDC
user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
user32.PrintWindow.argtypes = [wintypes.HWND, wintypes.HDC, wintypes.UINT]
user32.PrintWindow.restype = wintypes.BOOL
gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
gdi32.CreateCompatibleDC.restype = wintypes.HDC
gdi32.CreateCompatibleBitmap.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int]
gdi32.CreateCompatibleBitmap.restype = wintypes.HBITMAP
gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HGDIOBJ]
gdi32.SelectObject.restype = wintypes.HGDIOBJ
gdi32.DeleteObject.argtypes = [wintypes.HGDIOBJ]
gdi32.DeleteDC.argtypes = [wintypes.HDC]


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD), ("biWidth", wintypes.LONG),
        ("biHeight", wintypes.LONG), ("biPlanes", wintypes.WORD),
        ("biBitCount", wintypes.WORD), ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD), ("biXPelsPerMeter", wintypes.LONG),
        ("biYPelsPerMeter", wintypes.LONG), ("biClrUsed", wintypes.DWORD),
        ("biClrImportant", wintypes.DWORD),
    ]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD * 3)]


user32.ClientToScreen.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.POINT)]
user32.GetClientRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
user32.SetCursorPos.argtypes = [ctypes.c_int, ctypes.c_int]
user32.mouse_event.argtypes = [wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                               wintypes.DWORD, ctypes.c_void_p]
gdi32.GetDIBits.argtypes = [wintypes.HDC, wintypes.HBITMAP, wintypes.UINT,
                            wintypes.UINT, ctypes.c_void_p,
                            ctypes.POINTER(BITMAPINFO), wintypes.UINT]


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


def raise_window(hwnd):
    user32.ShowWindow(hwnd, 9)
    fg = user32.GetForegroundWindow()
    fg_tid = user32.GetWindowThreadProcessId(fg, None)
    me = kernel32.GetCurrentThreadId()
    if fg_tid and fg_tid != me:
        user32.AttachThreadInput(fg_tid, me, True)
        user32.SetForegroundWindow(hwnd)
        user32.BringWindowToTop(hwnd)
        user32.AttachThreadInput(fg_tid, me, False)
    else:
        user32.SetForegroundWindow(hwnd)
    # HWND_TOPMOST 必须是 64 位全 1，不能传 Python 的 -1（会被截成 0xFFFFFFFF）
    user32.SetWindowPos(hwnd, wintypes.HWND(-1), 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0040)


def print_window(hwnd, out):
    r = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(r))
    w, h = r.right - r.left, r.bottom - r.top
    hdc = user32.GetDC(0)
    mem = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    old = gdi32.SelectObject(mem, bmp)
    ok = user32.PrintWindow(hwnd, mem, 2)  # PW_RENDERFULLCONTENT

    bi = BITMAPINFO()
    bi.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bi.bmiHeader.biWidth = w
    bi.bmiHeader.biHeight = -h  # top-down
    bi.bmiHeader.biPlanes = 1
    bi.bmiHeader.biBitCount = 32
    bi.bmiHeader.biCompression = 0
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(mem, bmp, 0, h, buf, ctypes.byref(bi), 0)
    img = Image.frombuffer("RGB", (w, h), buf.raw, "raw", "BGRX", 0, 1)
    img.save(out)
    print(f"PrintWindow ok={ok} saved={out} size={img.size}")

    gdi32.SelectObject(mem, old)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(0, hdc)


def click_theme_button(hwnd):
    """标题栏右侧的主题切换按钮：用客户区坐标 + ClientToScreen 精确定位。"""
    cr = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(cr))
    pt = wintypes.POINT(cr.right - 164, 27)
    user32.ClientToScreen(hwnd, ctypes.byref(pt))
    user32.SetCursorPos(pt.x, pt.y)
    time.sleep(0.25)
    user32.mouse_event(0x0002, 0, 0, 0, None)  # LEFTDOWN
    time.sleep(0.08)
    user32.mouse_event(0x0004, 0, 0, 0, None)  # LEFTUP
    print(f"theme click at {pt.x},{pt.y}")


def click_program_row(hwnd):
    """点程序列表里第二行（有真实图标的 ADU.exe），验证详情卡片图标。"""
    cr = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(cr))
    pt = wintypes.POINT(160, 650)
    user32.ClientToScreen(hwnd, ctypes.byref(pt))
    user32.SetCursorPos(pt.x, pt.y)
    time.sleep(0.2)
    user32.mouse_event(0x0002, 0, 0, 0, None)
    time.sleep(0.08)
    user32.mouse_event(0x0004, 0, 0, 0, None)


def test_drag(hwnd):
    """在标题栏空白处按下鼠标、拖动一段距离、松开，返回窗口位移。"""
    before = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(before))
    x, y = before.left + 300, before.top + 27
    user32.SetCursorPos(x, y)
    time.sleep(0.25)
    user32.mouse_event(0x0002, 0, 0, 0, None)  # LEFTDOWN
    time.sleep(0.12)
    for i in range(1, 7):
        user32.SetCursorPos(x + 20 * i, y + 10 * i)
        time.sleep(0.05)
    user32.mouse_event(0x0004, 0, 0, 0, None)  # LEFTUP
    time.sleep(0.6)
    after = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(after))
    return after.left - before.left, after.top - before.top


def main():
    p = subprocess.Popen([str(EXE)], cwd=str(DIST))
    print(f"launched pid={p.pid}", flush=True)

    hwnd = None
    for _ in range(40):
        if p.poll() is not None:
            print(f"进程提前退出，returncode={p.returncode}")
            return 1
        hits = find_windows("流律")
        if hits:
            hwnd = hits[0]
            break
        time.sleep(0.5)
    if hwnd is None:
        print("窗口未出现")
        p.terminate()
        return 1

    raise_window(hwnd)
    time.sleep(3.0)  # 等 WebView2 渲染与首次进程扫描完成

    r = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(r))
    print(f"rect={r.left},{r.top},{r.right},{r.bottom}")

    # 一次进程内截两张：当前主题 + 点一次切换后的主题，再按亮度自动分辨明暗，
    # 这样不受 localStorage 里残留主题的影响。
    a = ROOT / "ui-theme-a.png"
    b = ROOT / "ui-theme-b.png"
    print_window(hwnd, a)
    click_theme_button(hwnd)
    time.sleep(1.0)
    print_window(hwnd, b)

    ia, ib = Image.open(a).convert("RGB"), Image.open(b).convert("RGB")
    from PIL import ImageStat
    la = sum(ImageStat.Stat(ia).mean) / 3
    lb = sum(ImageStat.Stat(ib).mean) / 3
    light, dark = (ia, ib) if la > lb else (ib, ia)
    light.save(ROOT / "ui-light.png")
    dark.save(ROOT / "ui-dark.png")
    print(f"brightness A={la:.0f} B={lb:.0f} -> light/dark saved")

    # 最后停在浅色：若第二次截图是深色，再点一次切回
    if lb < la:
        click_theme_button(hwnd)
        time.sleep(0.4)

    dx, dy = test_drag(hwnd)
    print(f"drag delta = ({dx}, {dy}) -> {'OK 窗口可拖拽' if abs(dx) > 20 or abs(dy) > 20 else 'FAIL 窗口未移动'}")

    click_program_row(hwnd)
    time.sleep(1.2)
    print_window(hwnd, ROOT / "ui-detail.png")

    p.terminate()
    try:
        p.wait(timeout=5)
    except Exception:
        p.kill()
    print("process stopped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
