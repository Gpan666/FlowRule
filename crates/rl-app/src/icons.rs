//! Windows exe 图标提取。
//!
//! `SHGetFileInfoW` 拿 HICON → `GetIconInfo` 拿彩色位图 → `GetDIBits` 读 32bpp BGRA
//! → 转 RGBA → PNG → base64，前端 `<img>` 可直接显示。
//!
//! 全部是 Win32 原生 API，不依赖 COM、不需要管理员权限；取不到路径的系统进程
//! 直接返回 None，前端就不渲染图标位、只显示程序名。
//!
//! 已用 Python ctypes 原型验证过整条链路（notepad / explorer / cmd 图标均正确、
//! 透明背景干净），这里是等价的 Rust 实现。
//!
//! 非 Windows 目标（CI 上跑 rl-app 单测）只保留声明、不链接、不实现。

// Windows 的 Shell / GDI 类型与常量，非 Windows 上无人使用
#![allow(dead_code)]

#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::{mem, path::Path};

const SHGFI_ICON: u32 = 0x0000_0100;
const SHGFI_LARGEICON: u32 = 0x0000_0000;
const DIB_RGB_COLORS: u32 = 0;

#[repr(C)]
struct ShFileInfoW {
    h_icon: isize,
    i_icon: i32,
    dw_attributes: u32,
    sz_display_name: [u16; 260],
    sz_type_name: [u16; 80],
}

#[repr(C)]
struct IconInfo {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: isize,
    hbm_color: isize,
}

#[repr(C)]
struct Bitmap {
    bm_type: i32,
    bm_width: i32,
    bm_height: i32,
    bm_width_bytes: i32,
    bm_planes: u16,
    bm_bits_pixel: u16,
    bm_bits: *mut u8,
}

#[repr(C)]
struct BitmapInfoHeader {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    bmi_header: BitmapInfoHeader,
    bmi_colors: [u32; 3],
}

// `link(name = …)` 只在真正的 Windows 目标上生效。非 Windows（CI 上跑 rl-app 单测）
// 保留声明但不链接，编译器不会真去解析这些符号。
#[cfg_attr(windows, link(name = "shell32"))]
extern "system" {
    fn SHGetFileInfoW(
        psz_path: *const u16,
        dw_file_attributes: u32,
        psfi: *mut ShFileInfoW,
        cb_file_info: u32,
        u_flags: u32,
    ) -> isize;
}

#[cfg_attr(windows, link(name = "user32"))]
extern "system" {
    fn GetIconInfo(h_icon: isize, p_icon_info: *mut IconInfo) -> i32;
    fn DestroyIcon(h_icon: isize) -> i32;
}

#[cfg_attr(windows, link(name = "gdi32"))]
extern "system" {
    fn GetObjectW(handle: isize, count: i32, data: *mut u8) -> i32;
    fn CreateCompatibleDC(hdc: isize) -> isize;
    fn DeleteDC(hdc: isize) -> i32;
    fn GetDIBits(
        hdc: isize,
        hbm: isize,
        start: u32,
        lines: u32,
        bits: *mut u8,
        bmi: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteObject(handle: isize) -> i32;
}

/// 返回 PNG 的 base64（不含 `data:image/png;base64,` 前缀）；取不到图标返回 `None`。
#[cfg(not(windows))]
pub fn exe_icon_base64(path: &str) -> Option<String> {
    // 非 Windows（CI 上跑单测）没有 Shell/GDI，直接当作取不到图标
    let _ = path;
    None
}

#[cfg(windows)]
pub fn exe_icon_base64(path: &str) -> Option<String> {
    if path.is_empty() || !Path::new(path).exists() {
        return None;
    }
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(std::iter::once(0)).collect();
    let mut sfi: ShFileInfoW = unsafe { mem::zeroed() };
    let ret = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut sfi,
            mem::size_of::<ShFileInfoW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if ret == 0 || sfi.h_icon == 0 {
        return None;
    }
    let png = unsafe { icon_to_png(sfi.h_icon) };
    unsafe { DestroyIcon(sfi.h_icon) };
    png
}

#[cfg(windows)]
unsafe fn icon_to_png(h_icon: isize) -> Option<String> {
    let mut ii: IconInfo = mem::zeroed();
    if GetIconInfo(h_icon, &mut ii) == 0 {
        return None;
    }
    if ii.hbm_color == 0 {
        if ii.hbm_mask != 0 {
            DeleteObject(ii.hbm_mask);
        }
        return None;
    }

    // 位图实际尺寸不固定（DPI 缩放时 LARGEICON 可能不是 32），先问 GDI
    let mut bm: Bitmap = mem::zeroed();
    let got = GetObjectW(
        ii.hbm_color,
        mem::size_of::<Bitmap>() as i32,
        &mut bm as *mut Bitmap as *mut u8,
    );
    let w = bm.bm_width;
    let h = bm.bm_height.abs();
    if got == 0 || w <= 0 || h <= 0 || w > 256 || h > 256 {
        DeleteObject(ii.hbm_color);
        if ii.hbm_mask != 0 {
            DeleteObject(ii.hbm_mask);
        }
        return None;
    }

    let hdc = CreateCompatibleDC(0);
    let mut bmi: BitmapInfo = mem::zeroed();
    bmi.bmi_header.bi_size = mem::size_of::<BitmapInfoHeader>() as u32;
    bmi.bmi_header.bi_width = w;
    bmi.bmi_header.bi_height = -h; // top-down，省掉一次垂直翻转
    bmi.bmi_header.bi_planes = 1;
    bmi.bmi_header.bi_bit_count = 32;
    bmi.bmi_header.bi_compression = 0; // BI_RGB

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(
        hdc,
        ii.hbm_color,
        0,
        h as u32,
        pixels.as_mut_ptr(),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    DeleteDC(hdc);
    DeleteObject(ii.hbm_color);
    if ii.hbm_mask != 0 {
        DeleteObject(ii.hbm_mask);
    }
    if lines != h {
        return None;
    }

    // BGRA → RGBA。少数图标的 alpha 通道全 0（透明度只存在 1bpp mask 里），
    // 这时给有色像素补 255，避免整张图看起来是全透明。
    for px in pixels.chunks_exact_mut(4) {
        let b = px[0];
        let g = px[1];
        let r = px[2];
        let mut a = px[3];
        if a == 0 && (r | g | b) != 0 {
            a = 255;
        }
        px[0] = r;
        px[1] = g;
        px[2] = b;
        px[3] = a;
    }

    let mut buf: Vec<u8> = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&pixels).ok()?;
    }

    use base64::Engine;
    Some(base64::engine::general_purpose::STANDARD.encode(&buf))
}
