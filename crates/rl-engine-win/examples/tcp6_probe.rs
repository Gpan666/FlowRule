use libloading::{Library, Symbol};
use std::ffi::c_void;
use std::collections::HashSet;

type FnGetTable = unsafe extern "system" fn(*mut c_void, *mut u32, i32, u32, u32, u32) -> u32;
type FnEnumProc = unsafe extern "system" fn(*mut u32, *mut u32) -> i32;

fn main() {
    unsafe {
        // 1) 收集系统中真实存在的 PID
        let k32 = Library::new("kernel32.dll").unwrap();
        let enum_proc: Symbol<FnEnumProc> = k32.get(b"K32EnumProcesses\0").unwrap();
        let mut ids = [0u32; 8192];
        let mut needed = 0u32;
        (*enum_proc)(ids.as_mut_ptr(), &mut needed);
        let count = (needed as usize / 4).min(8192);
        let pids: HashSet<u32> = ids[..count].iter().copied().filter(|&p| p > 4).collect();
        println!("active pids={}", pids.len());
        drop(k32);

        // 2) 拿 IPv6 TCP 表
        let dll = Library::new("iphlpapi.dll").unwrap();
        let tcp: Symbol<FnGetTable> = dll.get(b"GetExtendedTcpTable\0").unwrap();
        let f = *tcp;
        let mut size: u32 = 0;
        f(std::ptr::null_mut(), &mut size, 0, 23, 5, 0);
        let mut buf = vec![0u8; size as usize];
        let r2 = f(buf.as_mut_ptr() as *mut c_void, &mut size, 0, 23, 5, 0);
        if r2 != 0 { println!("ret={r2}"); return; }
        let buf = &buf[..size as usize];
        let n = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        println!("entries={n} bytes={}", buf.len());
        let _ = std::fs::write(r"C:\Users\MOSS\rl-target\tcp6.bin", buf);

        // 3) 扫描:只认真实 PID
        println!("--- pid scan (value in active pid set, ratio>0.7) ---");
        for head in [4usize, 8, 12, 16] {
            for stride in [52usize, 56, 60, 64] {
                if head + (n.max(1) - 1) * stride + 56 > buf.len() { continue; }
                for x in (0..56usize).step_by(4) {
                    let mut good = 0usize;
                    for i in 0..n {
                        let off = head + i * stride + x;
                        if off + 4 > buf.len() { break; }
                        let v = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
                        if pids.contains(&v) { good += 1; }
                    }
                    if good * 10 > n * 7 {
                        println!("head={head} stride={stride} pid@+{x} ratio={good}/{n}");
                    }
                }
            }
        }

        // 4) 端口字段扫描:值满足 mib_port(v) in 1..=65535 且不高位脏
        println!("--- port scan (low16 non-zero after swap sanity, ratio>0.7) ---");
        for head in [4usize, 8, 12, 16] {
            for stride in [52usize, 56usize] {
                if head + (n.max(1) - 1) * stride + 56 > buf.len() { continue; }
                for x in (0..56usize).step_by(4) {
                    let mut good = 0usize;
                    for i in 0..n {
                        let off = head + i * stride + x;
                        if off + 4 > buf.len() { break; }
                        let v = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
                        if v >> 16 == 0 && (v & 0xffff) != 0 { good += 1; }
                    }
                    if good * 10 > n * 7 {
                        println!("head={head} stride={stride} port@+{x} ratio={good}/{n}");
                    }
                }
            }
        }
        std::mem::forget(dll);
    }
}
