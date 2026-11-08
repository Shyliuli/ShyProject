//! 用户态进程模式：布置初始栈，并把系统调用直接透传给宿主。
//!
//! `shyemu-user` 只用这个模块；`shyemu` 的整机模式不安装 `UserMode`，所以
//! `syscall` 仍然走 ISA 定义的 trap。系统调用 ABI 见 `ABI.md`。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::cpu::Emu;

/// 初始栈顶：参数和环境变量字符串从这里的下方开始排布。
const STACK_TOP: u32 = 0x00ff_f000;
/// 初始栈按 16 字节对齐，和 RISC-V psABI 的进程入口一致。
const STACK_ALIGN: u32 = 16;
/// 一次读写最多搬多少字节，避免一个巨大的 count 直接吃掉宿主内存。
const MAX_IO: usize = 1 << 20;
/// 路径和名字的最大长度。
const PATH_MAX: usize = 4096;
/// 最多同时打开多少个 guest 文件描述符（3 号起）。
const FD_LIMIT: usize = 64;

// Linux asm-generic 编号，riscv64 直接使用这一套。
const SYS_GETCWD: u32 = 17;
const SYS_OPENAT: u32 = 56;
const SYS_CLOSE: u32 = 57;
const SYS_LSEEK: u32 = 62;
const SYS_READ: u32 = 63;
const SYS_WRITE: u32 = 64;
const SYS_EXIT: u32 = 93;
const SYS_EXIT_GROUP: u32 = 94;
const SYS_BRK: u32 = 214;

const AT_FDCWD: u32 = -100i32 as u32;

const O_WRONLY: u32 = 1;
const O_CREAT: u32 = 0o100;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;

const EIO: i32 = 5;
const EBADF: i32 = 9;
const EFAULT: i32 = 14;
const EINVAL: i32 = 22;
const ESPIPE: i32 = 29;
const ERANGE: i32 = 34;
const ENAMETOOLONG: i32 = 36;
const ENOSYS: i32 = 38;
const EMFILE: i32 = 24;

/// 一次系统调用的结果。
pub enum Outcome {
    /// 回到用户程序，值写进 `1x`。
    Return(u32),
    /// 以这个退出码结束进程。
    Exit(u32),
}

/// 进程初始栈：数据块在 `sp` 下方，`sp` 以上是空闲栈，
/// `4x`/`5x`/`6x` 是 argc/argv/envp。
pub struct Entry {
    pub sp: u32,
    pub argc: u32,
    pub argv: u32,
    pub envp: u32,
    /// 数据块最低地址，堆不能长到这里。
    pub data_start: u32,
}

/// 用户态进程的宿主状态：打开的文件和堆的边界。
pub struct UserMode {
    files: Vec<Option<File>>,
    heap_start: u32,
    brk: u32,
    brk_limit: u32,
}

impl UserMode {
    /// 堆从镜像末尾开始，向上最多长到初始栈块的下方。
    pub fn new(image_len: u32, brk_limit: u32) -> Self {
        let heap_start = image_len.next_multiple_of(16);
        Self {
            files: Vec::new(),
            heap_start,
            brk: heap_start,
            brk_limit,
        }
    }

    /// 执行一条系统调用：号码在 `0x`，参数在 `1x`..`6x`。
    pub fn syscall(&mut self, emu: &mut Emu, number: u32, args: [u32; 6]) -> Outcome {
        let value = match number {
            SYS_READ => self.read(emu, args[0], args[1], args[2]),
            SYS_WRITE => self.write(emu, args[0], args[1], args[2]),
            SYS_OPENAT => self.openat(emu, args[0], args[1], args[2]),
            SYS_CLOSE => self.close(args[0]),
            SYS_LSEEK => self.lseek(args[0], args[1] as i32, args[2]),
            SYS_BRK => self.brk(args[0]),
            SYS_GETCWD => self.getcwd(emu, args[0], args[1]),
            SYS_EXIT | SYS_EXIT_GROUP => return Outcome::Exit(args[0]),
            _ => errno(ENOSYS),
        };
        Outcome::Return(value)
    }

    fn read(&mut self, emu: &mut Emu, fd: u32, ptr: u32, count: u32) -> u32 {
        let count = count.min(MAX_IO as u32) as usize;
        let mut buffer = vec![0u8; count];
        let read = match fd {
            0 => std::io::stdin().read(&mut buffer),
            1 | 2 => return errno(EBADF),
            _ => match self.file(fd) {
                Some(file) => file.read(&mut buffer),
                None => return errno(EBADF),
            },
        };
        match read {
            Ok(n) => match emu.debug_write_mem(ptr, &buffer[..n]) {
                Ok(()) => n as u32,
                Err(_) => errno(EFAULT),
            },
            Err(error) => errno(os_error(&error)),
        }
    }

    fn write(&mut self, emu: &mut Emu, fd: u32, ptr: u32, count: u32) -> u32 {
        let count = count.min(MAX_IO as u32) as usize;
        let data = match emu.debug_read_mem(ptr, count) {
            Ok(data) => data,
            Err(_) => return errno(EFAULT),
        };
        let written = match fd {
            1 => {
                let mut out = std::io::stdout();
                out.write_all(&data).and_then(|()| out.flush())
            }
            2 => {
                let mut out = std::io::stderr();
                out.write_all(&data).and_then(|()| out.flush())
            }
            0 => return errno(EBADF),
            _ => match self.file(fd) {
                Some(file) => file.write_all(&data),
                None => return errno(EBADF),
            },
        };
        match written {
            Ok(()) => data.len() as u32,
            Err(error) => errno(os_error(&error)),
        }
    }

    fn openat(&mut self, emu: &mut Emu, dirfd: u32, path: u32, flags: u32) -> u32 {
        if dirfd != AT_FDCWD {
            return errno(EINVAL);
        }
        let path = match read_c_string(emu, path) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let mut options = std::fs::OpenOptions::new();
        match flags & 3 {
            0 => options.read(true),
            O_WRONLY => options.write(true),
            _ => options.read(true).write(true),
        };
        if flags & O_CREAT != 0 {
            options.create(true);
        }
        if flags & O_TRUNC != 0 {
            options.truncate(true);
        }
        if flags & O_APPEND != 0 {
            options.append(true);
        }
        match options.open(&path) {
            Ok(file) => self.insert(file),
            Err(error) => errno(os_error(&error)),
        }
    }

    fn close(&mut self, fd: u32) -> u32 {
        // 0、1、2 是宿主的标准流，关闭它们不影响后续系统调用。
        if fd < 3 {
            return 0;
        }
        match self.files.get_mut((fd - 3) as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                0
            }
            _ => errno(EBADF),
        }
    }

    fn lseek(&mut self, fd: u32, offset: i32, whence: u32) -> u32 {
        let Some(file) = self.file(fd) else {
            return errno(ESPIPE);
        };
        let position = match whence {
            0 if offset >= 0 => SeekFrom::Start(offset as u64),
            0 => return errno(EINVAL),
            1 => SeekFrom::Current(offset as i64),
            2 => SeekFrom::End(offset as i64),
            _ => return errno(EINVAL),
        };
        match file.seek(position) {
            Ok(offset) => offset as u32,
            Err(error) => errno(os_error(&error)),
        }
    }

    /// 与 Linux 相同：返回新的堆顶；放不下时原样返回，调用者据此判断失败。
    fn brk(&mut self, address: u32) -> u32 {
        if address == 0 {
            return self.brk;
        }
        if address < self.heap_start || address > self.brk_limit {
            return self.brk;
        }
        self.brk = address;
        address
    }

    fn getcwd(&mut self, emu: &mut Emu, ptr: u32, size: u32) -> u32 {
        let Ok(directory) = std::env::current_dir() else {
            return errno(EIO);
        };
        let mut bytes = directory.as_os_str().as_encoded_bytes().to_vec();
        bytes.push(0);
        if bytes.len() > size as usize {
            return errno(ERANGE);
        }
        match emu.debug_write_mem(ptr, &bytes) {
            Ok(()) => bytes.len() as u32,
            Err(_) => errno(EFAULT),
        }
    }

    fn file(&mut self, fd: u32) -> Option<&mut File> {
        let index = fd.checked_sub(3)? as usize;
        self.files.get_mut(index)?.as_mut()
    }

    fn insert(&mut self, file: File) -> u32 {
        for (index, slot) in self.files.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(file);
                return (index + 3) as u32;
            }
        }
        if self.files.len() >= FD_LIMIT {
            return errno(EMFILE);
        }
        self.files.push(Some(file));
        (self.files.len() + 2) as u32
    }
}

/// 按 ABI 把 argc/argv/envp 写进模拟内存，返回入口状态。
///
/// 数据块从低到高是：参数字符串，`argc`，`argv` 指针数组，NULL，`envp`
/// 指针数组，NULL。整块紧贴在 sp 下方：Shy 的栈向上生长，sp 以上才是程序
/// 可以自由使用的栈空间，数据块因此不会被栈覆盖。
pub fn setup_stack(emu: &mut Emu, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> anyhow::Result<Entry> {
    // argc, argv..., NULL, envp..., NULL
    let words = 1 + argv.len() + 1 + envp.len() + 1;
    let strings: u32 = argv
        .iter()
        .chain(envp)
        .map(|text| text.len() as u32 + 1)
        .sum();
    // 留 4 字节余量，指针数组按 4 字节对齐后仍不会越过 STACK_TOP。
    let data_start = (STACK_TOP - strings - words as u32 * 4 - 4) & !(STACK_ALIGN - 1);

    let mut cursor = data_start;
    let mut argv_pointers = Vec::with_capacity(argv.len());
    for arg in argv {
        argv_pointers.push(push_bytes(emu, &mut cursor, arg)?);
    }
    let mut envp_pointers = Vec::with_capacity(envp.len());
    for value in envp {
        envp_pointers.push(push_bytes(emu, &mut cursor, value)?);
    }

    let arrays = (cursor + 3) & !3;
    let argv_address = arrays + 4;
    let envp_address = argv_address + (argv.len() as u32 + 1) * 4;
    write_word(emu, arrays, argv.len() as u32)?;
    for (index, pointer) in argv_pointers.iter().enumerate() {
        write_word(emu, argv_address + index as u32 * 4, *pointer)?;
    }
    write_word(emu, argv_address + argv.len() as u32 * 4, 0)?;
    for (index, pointer) in envp_pointers.iter().enumerate() {
        write_word(emu, envp_address + index as u32 * 4, *pointer)?;
    }
    write_word(emu, envp_address + envp.len() as u32 * 4, 0)?;

    Ok(Entry {
        sp: STACK_TOP,
        argc: argv.len() as u32,
        argv: argv_address,
        envp: envp_address,
        data_start,
    })
}

/// 从 cursor 起向上写一个以 NUL 结尾的字节串，返回它的地址。
fn push_bytes(emu: &mut Emu, cursor: &mut u32, bytes: &[u8]) -> anyhow::Result<u32> {
    let address = *cursor;
    emu.debug_write_mem(address, bytes)?;
    emu.debug_write_mem(address + bytes.len() as u32, &[0])?;
    *cursor += bytes.len() as u32 + 1;
    Ok(address)
}

fn write_word(emu: &mut Emu, address: u32, value: u32) -> anyhow::Result<()> {
    emu.debug_write_mem(address, &value.to_be_bytes())
}

fn read_c_string(emu: &mut Emu, address: u32) -> Result<String, u32> {
    let mut bytes = Vec::new();
    for offset in 0..PATH_MAX {
        let Ok(byte) = emu.debug_read_mem(address.wrapping_add(offset as u32), 1) else {
            return Err(errno(EFAULT));
        };
        if byte[0] == 0 {
            return String::from_utf8(bytes).map_err(|_| errno(EINVAL));
        }
        bytes.push(byte[0]);
    }
    Err(errno(ENAMETOOLONG))
}

fn os_error(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(EIO)
}

/// 系统调用失败时返回负的 errno，和 Linux 一样。
fn errno(code: i32) -> u32 {
    (-code) as u32
}
