//! Ajustes del programa de fondo: se guardan en el equipo para que siga funcionando con la web cerrada.
use crate::pool::wallet_valida;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
use winreg::RegKey;

const ARRANQUE: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const NOMBRE_ARRANQUE: &str = "TokenMiner";
const HORAS_DEL_DIA: u32 = (1 << 24) - 1;

#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum Cuando {
    #[default]
    #[serde(rename = "manual")]
    Manual,
    #[serde(rename = "always")]
    Siempre,
    #[serde(rename = "schedule")]
    Horario,
}

#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum Tarifa {
    #[default]
    #[serde(rename = "fixed")]
    Fija,
    /// Precio regulado de España peninsular, distinto cada hora.
    #[serde(rename = "pvpc")]
    Pvpc,
}

/// Las monedas que mina el programa.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Moneda {
    Pearl,
    Quantus,
}

impl Moneda {
    pub const TODAS: [Moneda; 2] = [Moneda::Pearl, Moneda::Quantus];
}

/// Qué moneda minar: la que deje más dinero tras la luz, o una fija.
#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Eleccion {
    #[default]
    Auto,
    Pearl,
    Quantus,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Electricidad {
    pub kind: Tarifa,
    /// €/kWh, sólo con tarifa fija.
    pub price_kwh: Option<f64>,
    /// Lo que gasta de más el resto del equipo al minar: la gráfica se mide, esto no.
    pub extra_watts: f64,
}

impl Default for Electricidad {
    fn default() -> Self {
        Self {
            kind: Tarifa::Fija,
            price_kwh: None,
            extra_watts: 20.0,
        }
    }
}

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Ajustes {
    pub wallet: String,
    pub when: Cuando,
    /// Una máscara por día, de lunes a domingo: el bit h es la hora local h.
    pub hours: [u32; 7],
    pub start_with_pc: bool,
    /// En las horas que toque, mina sólo si lo ganado supera la luz.
    pub only_profitable: bool,
    pub electricity: Electricidad,
    /// Uso total máximo de la gráfica, en %, contando a los demás programas: TokenMiner toma lo que quede. Sin él,
    /// sin límite.
    pub gpu_limit: Option<u32>,
    pub coin: Eleccion,
}

/// Hora local. `dia` 0 es lunes.
#[derive(Clone, Copy)]
pub struct Momento {
    pub dia: usize,
    pub hora: u32,
}

fn hora_local() -> windows_sys::Win32::Foundation::SYSTEMTIME {
    unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    }
}

impl Momento {
    pub fn ahora() -> Self {
        let t = hora_local();
        Self {
            dia: (t.wDayOfWeek as usize + 6) % 7,
            hora: t.wHour as u32,
        }
    }
}

/// `AAAA-MM-DD` de hoy, en hora local.
pub fn fecha_de_hoy() -> String {
    let t = hora_local();
    format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay)
}

impl Ajustes {
    pub fn validar(&self) -> Result<(), String> {
        if !self.wallet.is_empty() && !wallet_valida(&self.wallet) {
            return Err("Cuenta de minado incorrecta".into());
        }
        if self.hours.iter().any(|h| h & !HORAS_DEL_DIA != 0) {
            return Err("Horario incorrecto".into());
        }
        let luz = &self.electricity;
        if luz.price_kwh.is_some_and(|p| !(p > 0.0 && p <= 2.0))
            || !(0.0..=2000.0).contains(&luz.extra_watts)
        {
            return Err("Datos de la luz incorrectos".into());
        }
        if self.gpu_limit.is_some_and(|l| l > 100) {
            return Err("Límite de la gráfica incorrecto".into());
        }
        Ok(())
    }

    fn toca(&self, m: Momento) -> bool {
        self.hours[m.dia] >> m.hora & 1 == 1
    }

    pub fn leer(ruta: &Path) -> Self {
        let leidos = std::fs::read(ruta)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok());
        leidos.filter(|a| a.validar().is_ok()).unwrap_or_default()
    }

    pub fn guardar(&self, ruta: &Path) -> std::io::Result<()> {
        if let Some(carpeta) = ruta.parent() {
            std::fs::create_dir_all(carpeta)?;
        }
        let provisional = ruta.with_extension("tmp");
        std::fs::write(&provisional, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(provisional, ruta)
    }
}

pub fn ruta() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("TokenMiner")
            .join("settings.json"),
    )
}

/// Arranque con Windows, sólo para este usuario.
pub fn fijar_arranque(activo: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if activo {
        let (clave, _) = hkcu.create_subkey(ARRANQUE)?;
        clave.set_value(
            NOMBRE_ARRANQUE,
            &format!("\"{}\" --background", std::env::current_exe()?.display()),
        )
    } else {
        match hkcu
            .open_subkey_with_flags(ARRANQUE, KEY_SET_VALUE)?
            .delete_value(NOMBRE_ARRANQUE)
        {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Decide si toca minar. Una orden del usuario manda hasta que cambia el estado del horario.
#[derive(Default)]
pub struct Plan {
    base: bool,
    orden: Option<bool>,
}

impl Plan {
    pub fn quiere(&mut self, a: &Ajustes, m: Momento) -> bool {
        let base = match a.when {
            Cuando::Manual => false,
            Cuando::Siempre => true,
            Cuando::Horario => a.toca(m),
        };
        if base != self.base {
            self.base = base;
            self.orden = None;
        }
        !a.wallet.is_empty() && self.orden.unwrap_or(base)
    }

    pub fn ordenar(&mut self, minar: bool) {
        self.orden = Some(minar);
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    const LUNES_10: Momento = Momento { dia: 0, hora: 10 };
    const LUNES_11: Momento = Momento { dia: 0, hora: 11 };
    const DOMINGO_10: Momento = Momento { dia: 6, hora: 10 };

    fn con_cuenta(when: Cuando) -> Ajustes {
        Ajustes {
            wallet: "cuenta.nombre".into(),
            when,
            ..Default::default()
        }
    }

    #[test]
    fn manual_solo_mina_con_orden() {
        let (a, mut p) = (con_cuenta(Cuando::Manual), Plan::default());
        assert!(!p.quiere(&a, LUNES_10));
        p.ordenar(true);
        assert!(p.quiere(&a, LUNES_10));
        p.ordenar(false);
        assert!(!p.quiere(&a, LUNES_10));
    }

    #[test]
    fn sin_cuenta_nunca_mina() {
        let (mut a, mut p) = (con_cuenta(Cuando::Siempre), Plan::default());
        a.wallet.clear();
        p.ordenar(true);
        assert!(!p.quiere(&a, LUNES_10));
    }

    #[test]
    fn horario_respeta_dia_y_hora() {
        let mut a = con_cuenta(Cuando::Horario);
        a.hours[0] = 1 << 10;
        let mut p = Plan::default();
        assert!(p.quiere(&a, LUNES_10));
        assert!(!p.quiere(&a, LUNES_11));
        assert!(!p.quiere(&a, DOMINGO_10));
    }

    #[test]
    fn parar_a_mano_dura_hasta_el_siguiente_cambio_del_horario() {
        let mut a = con_cuenta(Cuando::Horario);
        a.hours[0] = 1 << 10 | 1 << 11;
        a.hours[6] = 1 << 10;
        let mut p = Plan::default();
        assert!(p.quiere(&a, LUNES_10));
        p.ordenar(false);
        assert!(!p.quiere(&a, LUNES_11));
        assert!(!p.quiere(&a, Momento { dia: 0, hora: 12 }));
        assert!(p.quiere(&a, DOMINGO_10));
    }

    #[test]
    fn empezar_fuera_de_horario_acaba_al_cambiar_el_horario() {
        let mut a = con_cuenta(Cuando::Horario);
        a.hours[0] = 1 << 11;
        let mut p = Plan::default();
        p.ordenar(true);
        assert!(p.quiere(&a, LUNES_10));
        assert!(p.quiere(&a, LUNES_11));
        assert!(!p.quiere(&a, Momento { dia: 0, hora: 12 }));
    }

    #[test]
    fn valida_cuenta_y_horario() {
        assert!(Ajustes::default().validar().is_ok());
        assert!(con_cuenta(Cuando::Siempre).validar().is_ok());
        let mala = Ajustes {
            wallet: "solo:cuenta".into(),
            ..Default::default()
        };
        assert!(mala.validar().is_err());
        let mut horas = Ajustes::default();
        horas.hours[3] = 1 << 24;
        assert!(horas.validar().is_err());
        for (precio, extra) in [
            (Some(0.0), 20.0),
            (Some(3.0), 20.0),
            (Some(f64::NAN), 20.0),
            (None, -1.0),
            (None, 5000.0),
        ] {
            let mut luz = Ajustes::default();
            luz.electricity.price_kwh = precio;
            luz.electricity.extra_watts = extra;
            assert!(luz.validar().is_err(), "{precio:?} {extra}");
        }
        let mut bien = Ajustes::default();
        bien.electricity.price_kwh = Some(0.15);
        assert!(bien.validar().is_ok());
        for (limite, vale) in [
            (Some(80), true),
            (Some(100), true),
            (Some(0), true),
            (Some(101), false),
        ] {
            assert_eq!(
                Ajustes {
                    gpu_limit: limite,
                    ..Default::default()
                }
                .validar()
                .is_ok(),
                vale,
                "{limite:?}"
            );
        }
    }

    #[test]
    fn ajustes_antiguos_se_leen_con_la_luz_por_defecto() {
        let antiguos: Ajustes = serde_json::from_str(
            r#"{"wallet":"a.b","when":"always","hours":[0,0,0,0,0,0,0],"startWithPc":false}"#,
        )
        .unwrap();
        assert!(!antiguos.only_profitable);
        assert_eq!(antiguos.electricity, Electricidad::default());
        assert_eq!(antiguos.gpu_limit, None);
        assert_eq!(antiguos.coin, Eleccion::Auto);
        let quantus: Ajustes = serde_json::from_str(r#"{"coin":"quantus"}"#).unwrap();
        assert_eq!(quantus.coin, Eleccion::Quantus);
    }

    #[test]
    fn se_guardan_y_leen_con_los_nombres_de_la_web() {
        let carpeta =
            std::env::temp_dir().join(format!("tokenminer-ajustes-{}", std::process::id()));
        let ruta = carpeta.join("settings.json");
        let mut a = con_cuenta(Cuando::Horario);
        a.hours[2] = 7;
        a.start_with_pc = true;
        a.guardar(&ruta).unwrap();
        assert_eq!(Ajustes::leer(&ruta), a);
        let texto = std::fs::read_to_string(&ruta).unwrap();
        assert!(
            texto.contains("startWithPc") && texto.contains(r#""when": "schedule""#),
            "{texto}"
        );

        std::fs::write(&ruta, r#"{"wallet":"mal cuenta"}"#).unwrap();
        assert_eq!(Ajustes::leer(&ruta), Ajustes::default());
        std::fs::write(&ruta, "no es json").unwrap();
        assert_eq!(Ajustes::leer(&ruta), Ajustes::default());
        std::fs::remove_dir_all(carpeta).unwrap();
    }
}
