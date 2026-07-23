//! Small Unicode-text wrapper around the native Windows clipboard.

#[cfg(windows)]
pub fn write_text(text: &str) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    const CF_UNICODETEXT: u32 = 13;

    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);

    unsafe {
        OpenClipboard(HWND(0)).map_err(|e| format!("cannot open clipboard: {e}"))?;
        let result = (|| {
            EmptyClipboard().map_err(|e| format!("cannot clear clipboard: {e}"))?;
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * size_of::<u16>())
                .map_err(|e| format!("cannot allocate clipboard data: {e}"))?;
            let target = GlobalLock(memory).cast::<u16>();
            if target.is_null() {
                let _ = GlobalFree(memory);
                return Err("cannot lock clipboard data".to_string());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
            let _ = GlobalUnlock(memory);
            if let Err(error) =
                SetClipboardData(CF_UNICODETEXT, HANDLE(memory.0 as isize))
            {
                let _ = GlobalFree(memory);
                return Err(format!("cannot set clipboard data: {error}"));
            }
            // SetClipboardData owns `memory` after success.
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(windows)]
pub fn read_text() -> Result<String, String> {
    use windows::Win32::Foundation::{HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT)
            .map_err(|_| "clipboard does not contain text".to_string())?;
        OpenClipboard(HWND(0)).map_err(|e| format!("cannot open clipboard: {e}"))?;
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT)
                .map_err(|e| format!("cannot read clipboard: {e}"))?;
            let memory = HGLOBAL(handle.0 as *mut std::ffi::c_void);
            let source = GlobalLock(memory).cast::<u16>();
            if source.is_null() {
                return Err("cannot lock clipboard data".to_string());
            }
            let capacity = GlobalSize(memory) / size_of::<u16>();
            let slice = std::slice::from_raw_parts(source, capacity);
            let length = slice.iter().position(|&ch| ch == 0).unwrap_or(capacity);
            let text = String::from_utf16(&slice[..length])
                .map_err(|_| "clipboard contains invalid UTF-16".to_string());
            let _ = GlobalUnlock(memory);
            text
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(not(windows))]
pub fn write_text(_text: &str) -> Result<(), String> {
    Err("clipboard operations are only supported on Windows".into())
}

#[cfg(not(windows))]
pub fn read_text() -> Result<String, String> {
    Err("clipboard operations are only supported on Windows".into())
}
