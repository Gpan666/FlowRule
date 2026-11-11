//! 引擎纯逻辑回归测试（不依赖 Windows）。
//!
//! 引擎库只在 Windows 上产出（Win32 声明是 windows-only 的），所以这里用 `#[path]`
//! 直接把源码当模块编进测试二进制：不链接任何 Win32 库、不需要驱动，
//! 任何机器 / CI 上都能跑「五元组解析 / 分桶优先级 / 连接表解析」这些关键逻辑。
#![allow(dead_code)] // 生产代码里的 Win32 入口在测试里用不到

#[path = "../src/lib.rs"]
mod engine;
