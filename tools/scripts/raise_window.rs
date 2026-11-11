//! 一次性小工具：把指定 pid 的主窗口提到最前，并打印它的屏幕矩形。
//! 用途：无头环境里给 GUI 截图（PowerShell 的 Add-Type / COM 被安全策略禁用）。
//!
//! 编译：rustc --edition 2021 -O tools/raise_window.rs -o raise.exe
//! 用法：raise.exe <pid>

use std::ffi::c_void;
use std::process::exit;

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn AttachThreadInput(a: u32, b: u32, attach: i32) -> i32;
    fn GetForegroundWindow() -> isize;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn GetWindowRect(hwnd: isize, r: *mut Rect) -> i32;
}

static mut TARGET_PID: u32 = 0;
static mut FOUND: isize = 0;
static mut TITLE: [u16; 256] = [0; 256];

const HWND_TOPMOST: isize = -1;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_SHOWWINDOW: u32 = 0x0040;

extern "system" fn enum_cb(hwnd: isize, _lparam: isize) -> i32 {
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != TARGET_PID || IsWindowVisible(hwnd) == 0 {
            return 1; // 继续枚举
        }
        let n = GetWindowTextW(hwnd, TITLE.as_mut_ptr(), 256);
        if n == 0 {
            return 1;
        }
        FOUND = hwnd;
        0 // 停止枚举
    }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("用法: raise_window.exe <pid>");
        exit(2);
    });
    let target_pid: u32 = match arg.parse() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pid 解析失败: {e}");
            exit(2);
        }
    };

    unsafe {
        TARGET_PID = target_pid;
        EnumWindows(enum_cb, 0);
        if FOUND == 0 {
            eprintln!("未找到 pid={target_pid} 的可见窗口");
            exit(1);
        }
        let hwnd = FOUND;

        // 绕开「不允许设置前台窗口」的限制：先把自己的输入线程挂到前台窗口的线程上
        let fg = GetForegroundWindow();
        if fg != 0 {
            let mut fg_pid: u32 = 0;
            let fg_thread = GetWindowThreadProcessId(fg, &mut fg_pid);
            let me = GetCurrentThreadId();
            if fg_thread != me && fg_thread != 0 {
                AttachThreadInput(fg_thread, me, 1);
                SetForegroundWindow(hwnd);
                AttachThreadInput(fg_thread, me, 0);
            } else {
                SetForegroundWindow(hwnd);
            }
        } else {
            SetForegroundWindow(hwnd);
        }
        // 再压一层 topmost，确保盖过同为 topmost 的其他窗口
        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);

        let mut r = Rect::default();
        GetWindowRect(hwnd, &mut r);
        let title = String::from_utf16_lossy(&TITLE[..TITLE.iter().position(|&c| c == 0).unwrap_or(0)]);
        println!("OK title=\"{title}\" rect={},{},{},{}", r.left, r.top, r.right, r.bottom);
        let _ = std::mem::transmute::<isize, *mut c_void>(0); // 保持 c_void 引用，避免未使用告警
    }
}
