pub mod shyfile {
    #![allow(dead_code)]

    use std::ffi::CString;
    use std::io;
    use std::os::raw::c_char;
    use std::ptr::NonNull;

    pub const SHY_FILE_NAME_MAX: usize = 256;

    #[repr(C)]
    struct RawFile {
        len: i32,
        filename: [c_char; SHY_FILE_NAME_MAX],
        content: *mut u8,
    }

    unsafe extern "C" {
        fn shy_open(filename: *const c_char) -> *mut RawFile;
        fn shy_close(file: *mut RawFile) -> i32;
        fn shy_flush(file: *mut RawFile) -> i32;
        fn shy_rename(file: *mut RawFile, new_name: *const c_char) -> i32;
        fn shy_push_back(file: *mut RawFile, byte: u8) -> i32;
        fn shy_push_back_slice(file: *mut RawFile, data: *const u8, len: i32) -> i32;
    }

    pub struct File {
        ptr: NonNull<RawFile>,
    }

    impl File {
        pub fn open(filename: &str) -> io::Result<Self> {
            let filename = c_string(filename)?;
            let ptr = unsafe { shy_open(filename.as_ptr()) };
            NonNull::new(ptr)
                .map(|ptr| Self { ptr })
                .ok_or_else(io::Error::last_os_error)
        }

        pub fn len(&self) -> usize {
            unsafe { self.ptr.as_ref().len as usize }
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        pub fn as_slice(&self) -> &[u8] {
            let len = self.len();
            if len == 0 {
                return &[];
            }

            unsafe { std::slice::from_raw_parts(self.ptr.as_ref().content, len) }
        }

        pub fn as_mut_slice(&mut self) -> &mut [u8] {
            let len = self.len();
            if len == 0 {
                return &mut [];
            }

            unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ref().content, len) }
        }

        pub fn flush(&self) -> io::Result<()> {
            check(unsafe { shy_flush(self.ptr.as_ptr()) })
        }

        pub fn rename(&mut self, name: &str) -> io::Result<()> {
            let name = c_string(name)?;
            check(unsafe { shy_rename(self.ptr.as_ptr(), name.as_ptr()) })
        }

        pub fn push_back(&mut self, byte: u8) -> io::Result<()> {
            check(unsafe { shy_push_back(self.ptr.as_ptr(), byte) })
        }

        pub fn push_back_slice(&mut self, bytes: &[u8]) -> io::Result<()> {
            let len = i32::try_from(bytes.len()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "slice length exceeds i32::MAX")
            })?;
            check(unsafe { shy_push_back_slice(self.ptr.as_ptr(), bytes.as_ptr(), len) })
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            let _ = unsafe { shy_close(self.ptr.as_ptr()) };
        }
    }

    fn c_string(value: &str) -> io::Result<CString> {
        CString::new(value).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "string contains an interior NUL byte",
            )
        })
    }

    fn check(code: i32) -> io::Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
