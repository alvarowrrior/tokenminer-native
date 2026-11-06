//! Modo eficiencia: el límite de potencia de cada GPU, con NVML (`nvml.dll`, que instala el driver de NVIDIA).
//!
//! Con menos potencia la GPU baja de reloj y de voltaje, y cada unidad de trabajo cuesta menos energía: con la luz
//! cara, una GPU limitada deja más dinero por hora que a tope. Sólo se toca mientras se mina, y siempre se vuelve al
//! límite que tenía: antes de cambiarlo se guarda el original en un archivo, se restaura al parar y, si el programa
//! se cerró sin parar, al volver a arrancar. El límite nunca sale del margen que admite la tarjeta, y Windows lo
//! olvida al reiniciar.
//!
//! Cambiarlo exige permisos de administrador, y muchos portátiles no lo admiten: entonces no se toca nada y se dice
//! por qué.
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::Path;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const EXITO: i32 = 0;
const NO_ADMITIDO: i32 = 3;
const SIN_PERMISO: i32 = 4;

/// Por qué no se ha podido aplicar el límite.
#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub enum Fallo {
    /// El programa no se ejecuta como administrador.
    Admin,
    /// La tarjeta o el driver no dejan cambiarlo (habitual en portátiles).
    Unsupported,
    /// No hay NVML o falló de otra forma.
    Nvml,
}

/// Lo que la web enseña del modo eficiencia.
#[derive(Serialize, Clone, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Estado {
    /// Porcentaje pedido del límite de fábrica; 100 es apagado.
    pub percent: u32,
    /// Si el límite pedido está puesto ahora mismo.
    pub applied: bool,
    pub problem: Option<Fallo>,
    /// Límite de cada GPU ahora y el de fábrica, en W, en el orden de `gpus`.
    pub limits_w: Vec<Option<f64>>,
    pub default_w: Vec<Option<f64>>,
}

type Dispositivo = *mut c_void;

struct Api {
    shutdown: unsafe extern "C" fn() -> i32,
    contar: unsafe extern "C" fn(*mut u32) -> i32,
    por_indice: unsafe extern "C" fn(u32, *mut Dispositivo) -> i32,
    margen: unsafe extern "C" fn(Dispositivo, *mut u32, *mut u32) -> i32,
    de_fabrica: unsafe extern "C" fn(Dispositivo, *mut u32) -> i32,
    actual: unsafe extern "C" fn(Dispositivo, *mut u32) -> i32,
    fijar: unsafe extern "C" fn(Dispositivo, u32) -> i32,
}

/// Límites de una GPU, en mW.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limites {
    pub minimo: u32,
    pub maximo: u32,
    pub fabrica: u32,
    pub actual: u32,
}

pub struct Nvml {
    api: Api,
}

fn ancho(texto: &str) -> Vec<u16> {
    texto.encode_utf16().chain(Some(0)).collect()
}

impl Nvml {
    /// NVML del driver, o `None` si no hay driver de NVIDIA.
    pub fn abrir() -> Option<Nvml> {
        unsafe {
            let mut dll = LoadLibraryW(ancho("nvml.dll").as_ptr());
            if dll.is_null() {
                dll = LoadLibraryW(ancho("C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll").as_ptr());
            }
            if dll.is_null() {
                return None;
            }
            macro_rules! simbolo {
                ($nombre:literal) => {
                    std::mem::transmute(GetProcAddress(dll, concat!($nombre, "\0").as_ptr())?)
                };
            }
            let init: unsafe extern "C" fn() -> i32 = simbolo!("nvmlInit_v2");
            let api = Api {
                shutdown: simbolo!("nvmlShutdown"),
                contar: simbolo!("nvmlDeviceGetCount_v2"),
                por_indice: simbolo!("nvmlDeviceGetHandleByIndex_v2"),
                margen: simbolo!("nvmlDeviceGetPowerManagementLimitConstraints"),
                de_fabrica: simbolo!("nvmlDeviceGetPowerManagementDefaultLimit"),
                actual: simbolo!("nvmlDeviceGetPowerManagementLimit"),
                fijar: simbolo!("nvmlDeviceSetPowerManagementLimit"),
            };
            (init() == EXITO).then_some(Nvml { api })
        }
    }

    pub fn gpus(&self) -> u32 {
        let mut n = 0;
        unsafe { (self.api.contar)(&mut n) };
        n
    }

    fn dispositivo(&self, i: u32) -> Result<Dispositivo, i32> {
        let mut d = std::ptr::null_mut();
        match unsafe { (self.api.por_indice)(i, &mut d) } {
            EXITO => Ok(d),
            e => Err(e),
        }
    }

    /// Los límites de la GPU `i` (orden de `nvidia-smi`), en mW.
    pub fn limites(&self, i: u32) -> Result<Limites, i32> {
        let d = self.dispositivo(i)?;
        let (mut minimo, mut maximo, mut fabrica, mut actual) = (0, 0, 0, 0);
        unsafe {
            for codigo in [
                (self.api.margen)(d, &mut minimo, &mut maximo),
                (self.api.de_fabrica)(d, &mut fabrica),
                (self.api.actual)(d, &mut actual),
            ] {
                if codigo != EXITO {
                    return Err(codigo);
                }
            }
        }
        Ok(Limites {
            minimo,
            maximo,
            fabrica,
            actual,
        })
    }

    pub fn fijar(&self, i: u32, mw: u32) -> Result<(), i32> {
        let d = self.dispositivo(i)?;
        match unsafe { (self.api.fijar)(d, mw) } {
            EXITO => Ok(()),
            e => Err(e),
        }
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe { (self.api.shutdown)() };
    }
}

fn fallo(codigo: i32) -> Fallo {
    match codigo {
        SIN_PERMISO => Fallo::Admin,
        NO_ADMITIDO => Fallo::Unsupported,
        _ => Fallo::Nvml,
    }
}

/// El límite que toca para `porcentaje` del de fábrica, dentro del margen de la tarjeta.
pub fn objetivo(l: &Limites, porcentaje: u32) -> u32 {
    let pedido = (l.fabrica as u64 * porcentaje.min(100) as u64 / 100) as u32;
    pedido.clamp(l.minimo, l.maximo.max(l.minimo))
}

fn guardar(ruta: &Path, originales: &BTreeMap<u32, u32>) {
    if let Ok(texto) = serde_json::to_vec(originales) {
        let _ = std::fs::write(ruta, texto);
    }
}

fn leer(ruta: &Path) -> BTreeMap<u32, u32> {
    std::fs::read(ruta)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Lo que hay ahora, sin cambiar nada.
pub fn consultar(porcentaje: u32) -> Estado {
    let Some(nvml) = Nvml::abrir() else {
        return Estado {
            percent: porcentaje,
            problem: (porcentaje < 100).then_some(Fallo::Nvml),
            ..Default::default()
        };
    };
    let leidos: Vec<_> = (0..nvml.gpus()).map(|i| nvml.limites(i)).collect();
    // Pedido y sin poder leer el límite de alguna GPU: no se podrá cambiar, y se dice ya.
    let problema = leidos
        .iter()
        .find_map(|l| l.err())
        .filter(|_| porcentaje < 100)
        .map(fallo);
    let limites: Vec<_> = leidos.into_iter().map(Result::ok).collect();
    Estado {
        percent: porcentaje,
        applied: false,
        problem: problema,
        limits_w: limites.iter().map(|l| l.map(|l| l.actual as f64 / 1000.0)).collect(),
        default_w: limites.iter().map(|l| l.map(|l| l.fabrica as f64 / 1000.0)).collect(),
    }
}

/// Pone cada GPU a `porcentaje` de su límite de fábrica. Antes guarda en `ruta` el límite que tenía cada una, para
/// volver a él. Con 100 o más no toca nada.
pub fn aplicar(porcentaje: u32, ruta: &Path) -> Estado {
    if porcentaje >= 100 {
        return consultar(100);
    }
    let Some(nvml) = Nvml::abrir() else {
        return consultar(porcentaje);
    };
    // Si quedó un archivo de una sesión anterior, sus valores son los de verdad: no se pisan.
    let mut originales = leer(ruta);
    let mut problema = None;
    let mut aplicado = false;
    for i in 0..nvml.gpus() {
        let Ok(l) = nvml.limites(i) else {
            problema = Some(Fallo::Unsupported);
            continue;
        };
        originales.entry(i).or_insert(l.actual);
        guardar(ruta, &originales);
        match nvml.fijar(i, objetivo(&l, porcentaje)) {
            Ok(()) => aplicado = true,
            Err(codigo) => problema = Some(fallo(codigo)),
        }
    }
    // Si no se cambió ninguna, no hay nada que restaurar.
    if !aplicado {
        let _ = std::fs::remove_file(ruta);
    }
    Estado {
        applied: aplicado,
        problem: problema,
        ..consultar(porcentaje)
    }
}

/// Devuelve cada GPU al límite que tenía antes de `aplicar`, y borra el archivo. Sin archivo no toca nada.
pub fn restaurar(ruta: &Path) {
    let originales = leer(ruta);
    if originales.is_empty() {
        let _ = std::fs::remove_file(ruta);
        return;
    }
    let Some(nvml) = Nvml::abrir() else {
        return;
    };
    let mut pendientes = BTreeMap::new();
    for (i, mw) in originales {
        if nvml.fijar(i, mw).is_err() {
            pendientes.insert(i, mw);
        }
    }
    if pendientes.is_empty() {
        let _ = std::fs::remove_file(ruta);
    } else {
        guardar(ruta, &pendientes);
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn el_limite_pedido_no_sale_del_margen_de_la_tarjeta() {
        let l = Limites {
            minimo: 100_000,
            maximo: 220_000,
            fabrica: 170_000,
            actual: 170_000,
        };
        assert_eq!(objetivo(&l, 70), 119_000);
        assert_eq!(objetivo(&l, 50), 100_000);
        assert_eq!(objetivo(&l, 100), 170_000);
        assert_eq!(objetivo(&l, 150), 170_000);
        let rara = Limites {
            maximo: 50_000,
            ..l
        };
        assert_eq!(objetivo(&rara, 70), 100_000);
    }

    #[test]
    fn el_original_se_guarda_y_se_lee() {
        let ruta = std::env::temp_dir().join(format!("tokenminer-potencia-{}.json", std::process::id()));
        let originales = BTreeMap::from([(0, 170_000), (1, 220_000)]);
        guardar(&ruta, &originales);
        assert_eq!(leer(&ruta), originales);
        let _ = std::fs::remove_file(&ruta);
        assert!(leer(&ruta).is_empty());
    }

    /// Sólo lee: límites de cada GPU de este equipo, sin cambiar nada.
    #[test]
    #[ignore = "Requires an NVIDIA driver"]
    fn lee_los_limites_sin_cambiarlos() {
        let nvml = Nvml::abrir().expect("sin NVML");
        for i in 0..nvml.gpus() {
            println!("GPU {i}: {:?}", nvml.limites(i));
        }
        println!("{:?}", consultar(70));
    }
}
