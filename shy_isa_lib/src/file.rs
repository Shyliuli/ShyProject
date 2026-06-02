mod bindings {
    #![allow(non_camel_case_types)]
    #![allow(dead_code)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    #![allow(improper_ctypes)]

    include!(concat!(env!("OUT_DIR"), "/shy_file_bindings.rs"));
}

use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::{ffi, io, slice};

use bindings::{
    ShyFile, shy_close, shy_flush, shy_open, shy_push_back, shy_push_back_slice, shy_rename,
};

pub struct File {
    shy_file: NonNull<ShyFile>,
}
impl File {
    /// 开启一个文件
    pub fn open(file_name: &str) -> Option<File> {
        let c_file_name = ffi::CString::new(file_name).ok()?;
        //对C实现的包装
        let shy_file: *mut ShyFile = unsafe { shy_open(c_file_name.as_ptr()) };
        Some(File {
            shy_file: NonNull::new(shy_file)?,
        })
    }

    pub fn len(&self) -> usize {
        let len = unsafe { self.shy_file.as_ref().len };
        debug_assert!(len >= 0);
        len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.as_slice().as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_slice().as_mut_ptr()
    }

    pub fn as_slice(&self) -> &[u8] {
        let len = self.len();
        if len == 0 {
            return &[];
        }

        let content = unsafe { self.shy_file.as_ref().content };
        assert!(!content.is_null(), "ShyFile has len > 0 but null content");
        unsafe { slice::from_raw_parts(content, len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let len = self.len();
        if len == 0 {
            return &mut [];
        }

        let content = unsafe { self.shy_file.as_ref().content };
        assert!(!content.is_null(), "ShyFile has len > 0 but null content");
        unsafe { slice::from_raw_parts_mut(content, len) }
    }

    /// 将内存中数据写入
    pub fn flush(&self) -> io::Result<()> {
        let ret = unsafe { shy_flush(self.shy_file.as_ptr()) };

        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::other("shy_flush failed!"))
        }
    }
    /// 重命名
    pub fn rename(&mut self, file_name: &str) -> io::Result<()> {
        let c_file_name = ffi::CString::new(file_name)?;
        let ret = unsafe { shy_rename(self.shy_file.as_ptr(), c_file_name.as_ptr()) };
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::other("shy_rename failed!"))
        }
    }
    /// push_back 性能不好，谨慎使用
    pub fn push_back(&mut self, byte: u8) -> io::Result<()> {
        let ret = unsafe { shy_push_back(self.shy_file.as_ptr(), byte) };
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::other("shy_push_back failed!"))
        }
    }
    /// 一次性push_back一片，适合追加文件末尾
    pub fn push_back_slice(&mut self, bytes: &[u8]) -> io::Result<()> {
        let len = i32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "slice is too large"))?;
        let ret = unsafe { shy_push_back_slice(self.shy_file.as_ptr(), bytes.as_ptr(), len) };
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::other("shy_push_back_slice failed!"))
        }
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.push_back_slice(bytes)
    }
}
//当作&[u8]用
impl Deref for File {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}
//当作&mut [u8]用
impl DerefMut for File {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        //drop 不进行错误处理
        let _ = unsafe { shy_close(self.shy_file.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::File;
    use super::bindings::{SHY_FILE_NAME_MAX, shy_get_raw, shy_push_back_slice};
    use std::{env, fs, path::PathBuf, process};

    fn temp_file_name(name: &str) -> String {
        env::temp_dir()
            .join(format!("shy_isa_lib_{}_{}", process::id(), name))
            .to_str()
            .expect("temp path is not utf-8")
            .to_owned()
    }

    fn long_temp_file_name(name: &str) -> (PathBuf, String) {
        let root = env::temp_dir().join(format!("shy_isa_lib_{}_long_path", process::id()));
        let mut path = root.clone();
        while path.to_string_lossy().len() < SHY_FILE_NAME_MAX as usize {
            path.push("segment_abcdefghijklmnopqrstuvwxyz0123456789");
        }
        fs::create_dir_all(&path).expect("create long path parent");
        path.push(name);
        (
            root,
            path.to_str().expect("temp path is not utf-8").to_owned(),
        )
    }

    #[test]
    fn file_behaves_like_mutable_byte_slice() {
        let path = temp_file_name("slice.bin");
        let _ = fs::remove_file(&path);

        {
            let mut file = File::open(&path).expect("open");
            assert!(file.is_empty());

            file.extend_from_slice(b"abc").expect("append");
            assert_eq!(file.len(), 3);
            assert_eq!(&file[..], b"abc");

            file[1] = b'Z';
            let bytes = file.iter().copied().collect::<Vec<_>>();
            assert_eq!(bytes, b"aZc");
            file.flush().expect("flush");
        }

        assert_eq!(fs::read(&path).expect("read"), b"aZc");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rename_rejects_long_name_without_replacing_destination() {
        let old_path = temp_file_name("rename_old.bin");
        let (long_root, long_path) = long_temp_file_name("destination.bin");
        assert!(long_path.len() >= SHY_FILE_NAME_MAX as usize);
        let _ = fs::remove_file(&old_path);
        let _ = fs::remove_file(&long_path);

        fs::write(&old_path, b"old").expect("write old file");
        fs::write(&long_path, b"destination").expect("write destination file");

        {
            let mut file = File::open(&old_path).expect("open old file");
            assert!(file.rename(&long_path).is_err());
        }

        assert_eq!(fs::read(&old_path).expect("read old file"), b"old");
        assert_eq!(
            fs::read(&long_path).expect("read destination file"),
            b"destination"
        );
        let _ = fs::remove_file(&old_path);
        let _ = fs::remove_dir_all(&long_root);
    }

    #[test]
    fn push_back_slice_accepts_data_from_current_mapping() {
        let path = temp_file_name("alias_append.bin");
        let _ = fs::remove_file(&path);

        {
            let mut file = File::open(&path).expect("open");
            file.extend_from_slice(b"abc").expect("initial append");

            let raw = unsafe { shy_get_raw(file.shy_file.as_ptr()) };
            let ret =
                unsafe { shy_push_back_slice(file.shy_file.as_ptr(), raw, file.len() as i32) };
            assert_eq!(ret, 0);
            assert_eq!(&file[..], b"abcabc");
        }

        assert_eq!(fs::read(&path).expect("read"), b"abcabc");
        let _ = fs::remove_file(&path);
    }
}
