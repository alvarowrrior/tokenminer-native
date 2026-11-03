//! Icono junto al reloj: deja a la vista que el programa está en marcha y permite pararlo sin abrir la web.
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ICONO: &[u8] = include_bytes!("../assets/tokenminer.ico");
const AVISO: u32 = WM_APP + 1;
const CAMBIAR: usize = 1;
const ABRIR: usize = 2;
const SALIR: usize = 3;

pub enum Orden {
    Empezar,
    Parar,
    Salir,
}

struct Enlace {
    ordenes: UnboundedSender<Orden>,
    web: Vec<u16>,
}

static ENLACE: OnceLock<Enlace> = OnceLock::new();
static MINANDO: AtomicBool = AtomicBool::new(false);
static VENTANA: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static BARRA_NUEVA: AtomicU32 = AtomicU32::new(0);

fn ancho(texto: &str) -> Vec<u16> {
    texto.encode_utf16().chain(Some(0)).collect()
}

/// El ICO guarda varios tamaños: se usa el primero de al menos 32 píxeles. Windows acepta tanto PNG como mapa de bits.
fn icono() -> HICON {
    let leer = |i: usize, n: usize| {
        ICONO[i..i + n]
            .iter()
            .rev()
            .fold(0usize, |v, b| v << 8 | *b as usize)
    };
    let total = leer(4, 2);
    let entrada = (0..total)
        .map(|i| 6 + i * 16)
        .find(|&e| ICONO[e] == 0 || ICONO[e] >= 32)
        .unwrap_or(6);
    let (tamano, inicio) = (leer(entrada + 8, 4), leer(entrada + 12, 4));
    unsafe {
        let h = CreateIconFromResourceEx(
            ICONO[inicio..inicio + tamano].as_ptr(),
            tamano as u32,
            1,
            0x0003_0000,
            32,
            32,
            LR_DEFAULTCOLOR,
        );
        if h.is_null() {
            LoadIconW(null_mut(), IDI_APPLICATION)
        } else {
            h
        }
    }
}

fn datos(ventana: HWND) -> NOTIFYICONDATAW {
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = ventana;
    d.uID = 1;
    d
}

fn poner(ventana: HWND) {
    let mut d = datos(ventana);
    d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    d.uCallbackMessage = AVISO;
    d.hIcon = icono();
    for (destino, letra) in d.szTip.iter_mut().zip("TokenMiner".encode_utf16()) {
        *destino = letra;
    }
    unsafe { Shell_NotifyIconW(NIM_ADD, &d) };
}

fn menu(ventana: HWND) {
    let minando = MINANDO.load(Ordering::Relaxed);
    let (estado, cambiar) = if minando {
        ("TokenMiner: mining", "Stop mining")
    } else {
        ("TokenMiner: not mining", "Start mining")
    };
    let textos = [
        ancho(estado),
        ancho(cambiar),
        ancho("Open TokenMiner"),
        ancho("Exit"),
    ];
    unsafe {
        let m = CreatePopupMenu();
        AppendMenuW(m, MF_STRING | MF_GRAYED, 0, textos[0].as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, null());
        AppendMenuW(m, MF_STRING, CAMBIAR, textos[1].as_ptr());
        AppendMenuW(m, MF_STRING, ABRIR, textos[2].as_ptr());
        AppendMenuW(m, MF_STRING, SALIR, textos[3].as_ptr());
        let mut punto = POINT { x: 0, y: 0 };
        GetCursorPos(&mut punto);
        // Sin esto el menú no se cierra al pulsar fuera.
        SetForegroundWindow(ventana);
        let elegido = TrackPopupMenu(
            m,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            punto.x,
            punto.y,
            0,
            ventana,
            null(),
        );
        PostMessageW(ventana, WM_NULL, 0, 0);
        DestroyMenu(m);
        let Some(enlace) = ENLACE.get() else { return };
        match elegido as usize {
            CAMBIAR => {
                let _ = enlace.ordenes.send(if minando {
                    Orden::Parar
                } else {
                    Orden::Empezar
                });
            }
            ABRIR => {
                ShellExecuteW(
                    null_mut(),
                    ancho("open").as_ptr(),
                    enlace.web.as_ptr(),
                    null(),
                    null(),
                    SW_SHOWNORMAL,
                );
            }
            SALIR => {
                let _ = enlace.ordenes.send(Orden::Salir);
            }
            _ => {}
        }
    }
}

unsafe extern "system" fn proceso(ventana: HWND, mensaje: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if mensaje == AVISO {
        if matches!(l as u32 & 0xFFFF, WM_LBUTTONUP | WM_RBUTTONUP) {
            menu(ventana);
        }
        0
    } else if mensaje == BARRA_NUEVA.load(Ordering::Relaxed) {
        // Se reinició el explorador de Windows y el icono se ha perdido.
        poner(ventana);
        0
    } else {
        DefWindowProcW(ventana, mensaje, w, l)
    }
}

pub fn abrir(ordenes: UnboundedSender<Orden>, web: &str) {
    let _ = ENLACE.set(Enlace {
        ordenes,
        web: ancho(web),
    });
    std::thread::spawn(|| unsafe {
        let clase = ancho("TokenMinerBandeja");
        BARRA_NUEVA.store(
            RegisterWindowMessageW(ancho("TaskbarCreated").as_ptr()),
            Ordering::Relaxed,
        );
        let instancia = GetModuleHandleW(null());
        let mut registro: WNDCLASSW = std::mem::zeroed();
        registro.lpfnWndProc = Some(proceso);
        registro.hInstance = instancia;
        registro.lpszClassName = clase.as_ptr();
        RegisterClassW(&registro);
        let ventana = CreateWindowExW(
            0,
            clase.as_ptr(),
            clase.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instancia,
            null(),
        );
        VENTANA.store(ventana, Ordering::SeqCst);
        poner(ventana);
        let mut m: MSG = std::mem::zeroed();
        while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
            TranslateMessage(&m);
            DispatchMessageW(&m);
        }
    });
}

pub fn fijar_minando(minando: bool) {
    MINANDO.store(minando, Ordering::Relaxed);
}

pub fn quitar() {
    let ventana = VENTANA.load(Ordering::SeqCst);
    if !ventana.is_null() {
        unsafe { Shell_NotifyIconW(NIM_DELETE, &datos(ventana)) };
    }
}
