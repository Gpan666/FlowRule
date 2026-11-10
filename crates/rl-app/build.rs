use std::{env, fs, path::PathBuf};

fn main() {
    // GNU 的 windres 打不开含非 ASCII 字符的路径，而本项目目录叫「限速软件」。
    // tauri-build 默认会把 config 里的 icons/icon.ico 绝对路径写进 .rc，
    // 直接编译会报 `windres: can't open icon file ... Invalid argument`。
    // 解法：先把 ico 复制到 OUT_DIR（纯英文路径），再显式告诉 tauri-build 用它。
    let out = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR"));
    let ascii_icon = out.join("app-icon.ico");
    fs::copy("icons/icon.ico", &ascii_icon).expect("复制窗口图标到 OUT_DIR 失败");
    println!("cargo:rerun-if-changed=icons/icon.ico");

    let attributes = tauri_build::Attributes::new().windows_attributes(
        tauri_build::WindowsAttributes::new().window_icon_path(&ascii_icon),
    );
    tauri_build::try_build(attributes).expect("tauri-build 失败");
}
