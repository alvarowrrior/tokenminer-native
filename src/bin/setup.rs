#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    use winreg::RegKey;

    const CLASSES: &str = "Software\\Classes\\tokenminer";
    const UNINSTALL: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\TokenMiner";
    const STARTUP: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const STARTUP_NAME: &str = "TokenMiner";
    const CLOSE_EVENT: &str = "Local\\TokenMinerSalir";

    fn location() -> io::Result<PathBuf> {
        Ok(PathBuf::from(
            std::env::var_os("LOCALAPPDATA")
                .ok_or_else(|| io::Error::other("No se encontró LOCALAPPDATA"))?,
        )
        .join("Programs")
        .join("TokenMiner"))
    }

    fn wide(value: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn show(message: &str, error: bool) {
        let body = wide(message);
        let title = wide("TokenMiner");
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                MB_OK | if error { MB_ICONERROR } else { 0 },
            );
        }
    }

    #[cfg(feature = "bundle")]
    fn helper_bytes() -> io::Result<Vec<u8>> {
        Ok(include_bytes!(env!("TOKENMINER_HELPER_PATH")).to_vec())
    }

    #[cfg(not(feature = "bundle"))]
    fn helper_bytes() -> io::Result<Vec<u8>> {
        fs::read(std::env::current_exe()?.with_file_name("tokenminer-helper.exe"))
    }

    /// El programa de fondo bloquea su ejecutable: se le pide cerrar y se espera a que lo suelte.
    fn close_program() {
        let name = wide(CLOSE_EVENT);
        unsafe {
            let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
            if !event.is_null() {
                SetEvent(event);
                CloseHandle(event);
            }
        }
    }

    fn write_when_free(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut result = fs::write(path, bytes);
        for _ in 0..40 {
            if result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            result = fs::write(path, bytes);
        }
        result
    }

    fn remove_when_free(path: &Path) -> io::Result<()> {
        let mut result = fs::remove_file(path);
        for _ in 0..40 {
            if result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            result = fs::remove_file(path);
        }
        result
    }

    fn starts_with_pc() -> bool {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(STARTUP)
            .and_then(|key| key.get_value::<String, _>(STARTUP_NAME))
            .is_ok()
    }

    fn install() -> io::Result<()> {
        let folder = location()?;
        fs::create_dir_all(&folder)?;
        let helper = folder.join("tokenminer-helper.exe");
        let setup = folder.join("tokenminer-setup.exe");
        close_program();
        write_when_free(&helper, &helper_bytes()?)?;
        let current = std::env::current_exe()?;
        if current != setup {
            fs::copy(current, &setup)?;
        }

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (scheme, _) = hkcu.create_subkey(CLASSES)?;
        scheme.set_value("", &"URL:TokenMiner")?;
        scheme.set_value("URL Protocol", &"")?;
        let (command, _) = hkcu.create_subkey(format!("{CLASSES}\\shell\\open\\command"))?;
        command.set_value("", &format!("\"{}\" \"%1\"", helper.display()))?;
        let (uninstall, _) = hkcu.create_subkey(UNINSTALL)?;
        uninstall.set_value("DisplayName", &"TokenMiner local")?;
        uninstall.set_value("Publisher", &"TokenMiner")?;
        uninstall.set_value("DisplayVersion", &env!("CARGO_PKG_VERSION"))?;
        uninstall.set_value("InstallLocation", &folder.to_string_lossy().as_ref())?;
        uninstall.set_value(
            "UninstallString",
            &format!("\"{}\" --uninstall", setup.display()),
        )?;
        if starts_with_pc() {
            std::process::Command::new(&helper)
                .arg("--background")
                .spawn()?;
        }
        Ok(())
    }

    fn schedule_remove(path: &Path) -> io::Result<()> {
        let wide_path = wide(&path.to_string_lossy());
        let done = unsafe {
            MoveFileExW(
                wide_path.as_ptr(),
                std::ptr::null(),
                MOVEFILE_DELAY_UNTIL_REBOOT,
            )
        };
        if done == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn uninstall() -> io::Result<()> {
        let folder = location()?;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let _ = hkcu.delete_subkey_all(CLASSES);
        let _ = hkcu.delete_subkey_all(UNINSTALL);
        if let Ok(startup) = hkcu.open_subkey_with_flags(STARTUP, KEY_SET_VALUE) {
            let _ = startup.delete_value(STARTUP_NAME);
        }
        close_program();
        let helper = folder.join("tokenminer-helper.exe");
        if helper.exists() {
            remove_when_free(&helper)?;
        }
        let setup = folder.join("tokenminer-setup.exe");
        if setup.exists() {
            schedule_remove(&setup)?;
            schedule_remove(&folder)?;
        }
        Ok(())
    }

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let removing = args.iter().any(|arg| arg == "--uninstall");
        let quiet = args.iter().any(|arg| arg == "--quiet");
        let result = if removing { uninstall() } else { install() };
        match result {
            Ok(()) => {
                if !quiet {
                    show(
                        if removing {
                            "TokenMiner local se ha desinstalado. El archivo del desinstalador se borrará al reiniciar."
                        } else {
                            "TokenMiner local está instalado para este usuario."
                        },
                        false,
                    )
                }
                0
            }
            Err(error) => {
                if !quiet {
                    show(
                        &format!(
                            "No se pudo {} TokenMiner local: {error}",
                            if removing { "desinstalar" } else { "instalar" }
                        ),
                        true,
                    )
                }
                1
            }
        }
    }
}

fn main() {
    #[cfg(windows)]
    std::process::exit(windows::run());
    #[cfg(not(windows))]
    eprintln!("Este instalador solo está disponible para Windows")
}
