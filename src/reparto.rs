//! Reparto de la GPU con otros programas: Windows dice cuánto la usa cada proceso (lo que enseña el Administrador de
//! tareas) y con eso se decide qué parte del tiempo toca al motor para no pasar del uso total elegido. Sólo se lee:
//! no cambia ningún ajuste de la gráfica.
use crate::send;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA,
};

const CADA: Duration = Duration::from_secs(2);
/// Uso total máximo cuando el usuario no ha puesto ninguno: el motor va sin pausas.
pub const SIN_LIMITE: u32 = u32::MAX;
/// Por debajo de esta parte no merece la pena minar: el motor se aparta. Vuelve cuando hay un poco más.
const PARTE_MINIMA: f64 = 5.0;
const PARTE_VUELTA: f64 = 10.0;
/// Con casi toda la GPU para él, el motor va sin pausas, que rinde más.
const PARTE_LLENA: f64 = 97.0;

/// Uso de la gráfica en el último intervalo, en %, del motor y de todos los demás programas juntos.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Uso {
    pub mio: f64,
    pub otros: f64,
}

/// Un contador se llama `pid_<pid>_luid_<a>_<b>_phys_<p>_eng_<n>_engtype_<tipo>`.
struct Instancia<'a> {
    pid: u32,
    gpu: &'a str,
    motor: &'a str,
    tipo: &'a str,
}

fn partir(nombre: &str) -> Option<Instancia<'_>> {
    let (pid, resto) = nombre.strip_prefix("pid_")?.split_once("_luid_")?;
    let (gpu, resto) = resto.split_once("_eng_")?;
    let (motor, tipo) = resto.split_once("_engtype_")?;
    Some(Instancia {
        pid: pid.parse().ok()?,
        gpu,
        motor,
        tipo,
    })
}

/// En la gráfica `luid` (`0x<alto>_0x<bajo>`), suma por motor lo de `pid` y lo de los demás y se queda con el motor
/// más ocupado de cada uno. Las copias de memoria no cuentan.
pub fn repartir(lecturas: &[(String, f64)], pid: u32, luid: &str) -> Uso {
    let prefijo = format!("{luid}_phys_").to_ascii_lowercase();
    let (mut mio, mut otros) = (HashMap::new(), HashMap::new());
    for (i, valor) in lecturas
        .iter()
        .filter_map(|(nombre, valor)| Some((partir(nombre)?, *valor)))
        .filter(|(i, _)| {
            !i.tipo.eq_ignore_ascii_case("copy") && i.gpu.to_ascii_lowercase().starts_with(&prefijo)
        })
    {
        *if i.pid == pid { &mut mio } else { &mut otros }
            .entry((i.gpu, i.motor))
            .or_insert(0.0) += valor;
    }
    let mayor = |m: HashMap<_, f64>| m.into_values().fold(0.0, f64::max).min(100.0);
    Uso {
        mio: mayor(mio),
        otros: mayor(otros),
    }
}

/// Qué parte del tiempo de GPU toca al motor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Parte {
    Llena,
    Limitada(u32),
    Apartado,
}

/// Decide la parte del motor con un uso total máximo `limite`: lo que dejan los demás. Cuando los demás suben se
/// cede enseguida; cuando bajan se recupera poco a poco, para no pelear con un juego que va a rachas.
#[derive(Default)]
pub struct Reparto {
    otros: Option<f64>,
    apartado: bool,
}

impl Reparto {
    pub fn decidir(&mut self, limite: u32, otros: f64) -> Parte {
        let otros = match self.otros {
            Some(antes) if otros < antes => 0.6 * antes + 0.4 * otros,
            _ => otros,
        };
        self.otros = Some(otros);
        let parte = limite as f64 - otros;
        self.apartado = parte
            < if self.apartado {
                PARTE_VUELTA
            } else {
                PARTE_MINIMA
            };
        if self.apartado {
            Parte::Apartado
        } else if parte >= PARTE_LLENA {
            Parte::Llena
        } else {
            Parte::Limitada(parte.round() as u32)
        }
    }
}

/// Consulta abierta de los contadores «GPU Engine» de Windows. Cada `leer` da el uso desde el anterior.
pub struct Lector {
    consulta: PDH_HQUERY,
    contador: PDH_HCOUNTER,
}

unsafe impl Send for Lector {}

impl Lector {
    pub fn abrir() -> Option<Self> {
        let ruta: Vec<u16> = "\\GPU Engine(*)\\Utilization Percentage"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut consulta = std::ptr::null_mut();
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut consulta) != 0 {
                return None;
            }
            let mut c = Self {
                consulta,
                contador: std::ptr::null_mut(),
            };
            if PdhAddEnglishCounterW(consulta, ruta.as_ptr(), 0, &mut c.contador) != 0
                || PdhCollectQueryData(consulta) != 0
            {
                return None;
            }
            Some(c)
        }
    }

    pub fn leer(&mut self) -> Option<Vec<(String, f64)>> {
        unsafe {
            if PdhCollectQueryData(self.consulta) != 0 {
                return None;
            }
            let (mut bytes, mut cuantos) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(
                self.contador,
                PDH_FMT_DOUBLE,
                &mut bytes,
                &mut cuantos,
                std::ptr::null_mut(),
            ) != PDH_MORE_DATA
            {
                return Some(Vec::new());
            }
            // Los nombres van en el mismo búfer, detrás de los elementos.
            let mut bufer = vec![0u64; (bytes as usize).div_ceil(8)];
            let elementos = bufer.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(
                self.contador,
                PDH_FMT_DOUBLE,
                &mut bytes,
                &mut cuantos,
                elementos,
            ) != 0
            {
                return None;
            }
            Some(
                std::slice::from_raw_parts(elementos, cuantos as usize)
                    .iter()
                    .filter(|e| e.FmtValue.CStatus == 0 && !e.szName.is_null())
                    .map(|e| {
                        let largo = (0..).take_while(|&i| *e.szName.add(i) != 0).count();
                        (
                            String::from_utf16_lossy(std::slice::from_raw_parts(e.szName, largo)),
                            e.FmtValue.Anonymous.doubleValue,
                        )
                    })
                    .collect(),
            )
        }
    }
}

impl Drop for Lector {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.consulta) };
    }
}

/// Lo que el reparto mueve en el motor de una GPU: qué parte del tiempo puede ocuparla y si se aparta.
pub trait Mando: Send + Sync {
    fn fijar(&self, limite: u32, apartado: bool);
}

impl Mando for tokenminer_pearl::trabajador::Contadores {
    fn fijar(&self, limite: u32, apartado: bool) {
        self.limite.store(limite, Ordering::Relaxed);
        self.apartado.store(apartado, Ordering::Relaxed);
    }
}

/// Mira cada dos segundos cuánto usan cada GPU los demás programas y fija la parte de su motor para no pasar de
/// `limite`, el uso total máximo elegido, de 0 a 100, o `SIN_LIMITE`. Sin el LUID de una GPU, o si Windows no da
/// sus contadores, no se ve a los demás y sólo se aplica el límite. Avisa con
/// `{"type":"gpu","devices":[{"share":…,"others":…,"mine":…}]}`: `mine` es lo que usa el motor.
pub fn vigilar(
    gpus: Vec<(Arc<dyn Mando>, Option<String>)>,
    limite: Arc<AtomicU32>,
    parando: Arc<AtomicBool>,
    tx: UnboundedSender<String>,
) {
    vigilar_con(Lector::abrir(), gpus, limite, parando, tx)
}

fn vigilar_con(
    mut lector: Option<Lector>,
    gpus: Vec<(Arc<dyn Mando>, Option<String>)>,
    limite: Arc<AtomicU32>,
    parando: Arc<AtomicBool>,
    tx: UnboundedSender<String>,
) {
    let mut repartos: Vec<Reparto> = gpus.iter().map(|_| Reparto::default()).collect();
    let pid = std::process::id();
    loop {
        for _ in 0..10 {
            if parando.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(CADA / 10);
        }
        // Sin contadores el límite elegido se sigue cumpliendo: antes el motor iba al 100 % sin avisar.
        let lecturas = lector.as_mut().and_then(Lector::leer);
        let tope = limite.load(Ordering::Relaxed);
        let dispositivos: Vec<_> = gpus
            .iter()
            .zip(&mut repartos)
            .map(|((mando, luid), reparto)| {
                let uso = lecturas
                    .as_ref()
                    .zip(luid.as_deref())
                    .map(|(lecturas, luid)| repartir(lecturas, pid, luid));
                let otros = uso.map(|u| u.otros);
                let parte = if tope == SIN_LIMITE {
                    *reparto = Reparto::default();
                    Parte::Llena
                } else {
                    reparto.decidir(tope, otros.unwrap_or(0.0))
                };
                let (motor, apartado, share) = match parte {
                    Parte::Llena => (0, false, 100),
                    Parte::Limitada(p) => (p, false, p),
                    Parte::Apartado => (0, true, 0),
                };
                mando.fijar(motor, apartado);
                json!({"share": share, "others": otros, "mine": uso.map(|u| u.mio)})
            })
            .collect();
        send(&tx, json!({"type": "gpu", "devices": dispositivos}));
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    fn lecturas(filas: &[(&str, f64)]) -> Vec<(String, f64)> {
        filas.iter().map(|(n, v)| (n.to_string(), *v)).collect()
    }

    #[test]
    fn separa_lo_mio_de_lo_de_otros_en_mi_grafica() {
        let l = lecturas(&[
            (
                "pid_500_luid_0x00000000_0x00014B88_phys_0_eng_0_engtype_3D",
                70.0,
            ),
            (
                "pid_500_luid_0x00000000_0x00014B88_phys_0_eng_3_engtype_Copy",
                9.0,
            ),
            (
                "pid_42_luid_0x00000000_0x00014B88_phys_0_eng_0_engtype_3D",
                20.0,
            ),
            (
                "pid_43_luid_0x00000000_0x00014B88_phys_0_eng_0_engtype_3D",
                5.0,
            ),
            (
                "pid_43_luid_0x00000000_0x00014B88_phys_0_eng_3_engtype_Copy",
                40.0,
            ),
            (
                "pid_44_luid_0x00000000_0x00014B88_phys_0_eng_5_engtype_VideoDecode",
                12.0,
            ),
            (
                "pid_45_luid_0x00000000_0x000143BB_phys_0_eng_0_engtype_3D",
                90.0,
            ),
            ("roto", 50.0),
        ]);
        assert_eq!(
            repartir(&l, 500, "0x00000000_0x00014B88"),
            Uso {
                mio: 70.0,
                otros: 25.0
            }
        );
        // CUDA escribe el LUID en mayúsculas y Windows a veces en minúsculas.
        assert_eq!(
            repartir(&l, 500, "0x00000000_0x00014b88"),
            Uso {
                mio: 70.0,
                otros: 25.0
            }
        );
        assert_eq!(
            repartir(&l, 500, "0x00000000_0x000143BB"),
            Uso {
                mio: 0.0,
                otros: 90.0
            }
        );
        assert_eq!(
            repartir(&l, 500, "0x00000000_0x00000001"),
            Uso {
                mio: 0.0,
                otros: 0.0
            }
        );
    }

    #[test]
    fn sin_contadores_de_windows_el_limite_se_cumple_igual() {
        #[derive(Default)]
        struct Freno(AtomicU32);
        impl Mando for Freno {
            fn fijar(&self, limite: u32, _apartado: bool) {
                self.0.store(limite, Ordering::SeqCst);
            }
        }
        let freno = Arc::new(Freno::default());
        let parando = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let hilo = {
            let (freno, parando) = (freno.clone(), parando.clone());
            std::thread::spawn(move || {
                vigilar_con(
                    None,
                    vec![(freno as Arc<dyn Mando>, None)],
                    Arc::new(AtomicU32::new(60)),
                    parando,
                    tx,
                )
            })
        };
        let aviso: serde_json::Value = serde_json::from_str(&rx.blocking_recv().unwrap()).unwrap();
        parando.store(true, Ordering::SeqCst);
        hilo.join().unwrap();
        assert_eq!(freno.0.load(Ordering::SeqCst), 60);
        assert_eq!(aviso["devices"][0]["share"], 60);
        assert!(aviso["devices"][0]["others"].is_null());
    }

    #[test]
    fn deja_a_los_demas_lo_suyo_y_se_aparta_si_queda_poco() {
        let mut r = Reparto::default();
        assert_eq!(r.decidir(100, 2.0), Parte::Llena);
        assert_eq!(r.decidir(100, 30.0), Parte::Limitada(70));
        assert_eq!(Reparto::default().decidir(0, 0.0), Parte::Apartado);
        let mut r = Reparto::default();
        assert_eq!(r.decidir(80, 5.0), Parte::Limitada(75));
        assert_eq!(r.decidir(80, 60.0), Parte::Limitada(20));
        assert_eq!(r.decidir(80, 78.0), Parte::Apartado);
        // Al bajar los demás, vuelve poco a poco y sólo con margen.
        assert_eq!(r.decidir(80, 74.0), Parte::Apartado);
        assert_eq!(r.decidir(80, 0.0), Parte::Limitada(34));
        assert_eq!(r.decidir(80, 0.0), Parte::Limitada(52));
    }
}
