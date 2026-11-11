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

    /// El programa de fondo bloquea su ejecutable: se le pide cerrar y se espera a que lo suelte. Dice si había
    /// uno abierto.
    fn close_program() -> bool {
        let name = wide(CLOSE_EVENT);
        unsafe {
            let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
            if event.is_null() {
                return false;
            }
            SetEvent(event);
            CloseHandle(event);
            true
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
        let was_running = close_program();
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
        // Quien actualiza con el programa abierto lo tenía minando o esperando su hora: se vuelve a abrir, para
        // que una actualización no lo deje parado sin avisar.
        if was_running || starts_with_pc() {
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

    /// La carpeta de los ajustes y las medidas, que el programa crea al usarse (`ajustes::ruta`).
    fn data_folder() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("TokenMiner"))
    }

    /// Si dos rutas son el mismo archivo, se escriban como se escriban.
    fn same_file(a: &Path, b: &Path) -> bool {
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => a == b,
        }
    }

    /// Windows no deja que un programa borre su propio ejecutable mientras corre, y dejarlo para el siguiente
    /// reinicio exige ser administrador. El desinstalador instalado se copia a la carpeta temporal y sigue desde
    /// allí, donde sí puede borrar el original y su carpeta. Dice si lo ha hecho.
    fn continue_from_temp(args: &[String]) -> io::Result<bool> {
        let current = std::env::current_exe()?;
        if !same_file(&current, &location()?.join("tokenminer-setup.exe")) {
            return Ok(false);
        }
        let copy = std::env::temp_dir().join("tokenminer-uninstall.exe");
        fs::copy(&current, &copy)?;
        std::process::Command::new(copy).args(args).spawn()?;
        Ok(true)
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
        // Los ajustes guardan la cuenta con la que se mina: desinstalar no los deja atrás. Va después de cerrar el
        // programa, que al salir devuelve el límite de potencia que el modo eficiencia tuviera cambiado.
        if let Some(data) = data_folder() {
            let _ = fs::remove_dir_all(data);
        }
        let setup = folder.join("tokenminer-setup.exe");
        let current = std::env::current_exe()?;
        if setup.exists() {
            if same_file(&current, &setup) {
                // No se pudo seguir desde la carpeta temporal: queda para el reinicio, si Windows lo permite.
                let _ = schedule_remove(&setup);
                let _ = schedule_remove(&folder);
            } else {
                remove_when_free(&setup)?;
                // Sólo si ha quedado vacía: lo que otro haya dejado ahí no es nuestro.
                let _ = fs::remove_dir(&folder);
            }
        }
        // La copia temporal de este desinstalador no se puede borrar a sí misma; Windows la quita al reiniciar si
        // lo permite, y si no, queda en la carpeta temporal, que el sistema limpia.
        if current.parent().is_some_and(|p| same_file(p, &std::env::temp_dir())) {
            let _ = schedule_remove(&current);
        }
        Ok(())
    }

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let removing = args.iter().any(|arg| arg == "--uninstall");
        let quiet = args.iter().any(|arg| arg == "--quiet");
        if removing && continue_from_temp(&args).unwrap_or(false) {
            return 0;
        }
        let result = if removing { uninstall() } else { install() };
        match result {
            Ok(()) => {
                if !quiet {
                    show(
                        if removing {
                            "TokenMiner local se ha desinstalado."
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
