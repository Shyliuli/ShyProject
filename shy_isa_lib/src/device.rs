// src/device.rs
use crate::{error::CoreError, types::Word};

/// 设备统一接口
pub trait AddrPort {
    /// 读取设备内偏移 off 处的字
    fn read(&self, off: usize) -> Result<Word, CoreError>;

    /// 写入设备内偏移 off 处的字
    fn write(&mut self, off: usize, val: Word) -> Result<(), CoreError>;

    /// 设备容量（以字为单位）
    fn len(&self) -> usize;

    /// 设备是否为空
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}