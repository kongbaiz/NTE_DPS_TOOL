use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::Local;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

pub(crate) fn atomic_write_text(path: &Path, text: &str) -> Result<(), String> {
    atomic_write_file(path, |writer| {
        writer
            .write_all(text.as_bytes())
            .map_err(|error| error.to_string())
    })
}

pub(crate) fn atomic_write_file<F>(path: &Path, write: F) -> Result<(), String>
where
    F: FnOnce(&mut BufWriter<File>) -> Result<(), String>,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temp_path = temporary_write_path(path);
    let result = (|| {
        let file = File::create(&temp_path)
            .map_err(|error| format!("创建临时文件 {} 失败: {error}", temp_path.display()))?;
        let mut writer = BufWriter::new(file);
        write(&mut writer)?;
        writer.flush().map_err(|error| error.to_string())?;
        writer
            .get_ref()
            .sync_all()
            .map_err(|error| error.to_string())?;
        drop(writer);
        replace_file(&temp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn temporary_write_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_else(|| OsStr::new("nte-write")));
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        Local::now().format("%Y%m%d%H%M%S%3f")
    ));
    parent.join(name)
}

#[cfg(windows)]
fn replace_file(temp_path: &Path, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    let temp_wide = temp_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: Both wide strings are NUL-terminated and stay alive for the call.
    let result = unsafe {
        MoveFileExW(
            temp_wide.as_ptr(),
            path_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(format!(
            "替换 {} 失败: {}",
            path.display(),
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(temp_path: &Path, path: &Path) -> Result<(), String> {
    std::fs::rename(temp_path, path)
        .map_err(|error| format!("替换 {} 失败: {error}", path.display()))
}
