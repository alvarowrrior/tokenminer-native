//! Qué moneda minar. Cada moneda se mide en este equipo unos minutos (velocidad y consumo de la gráfica) y después
//! se mina la que deja más dinero tras la luz. Para no saltar de una a otra por el ruido del mercado, la otra tiene
//! que ganar por más de un 5 % durante media hora seguida, y la actual llevar media hora minando.
use crate::ajustes::{Eleccion, Moneda};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Muestras de 5 s para dar una moneda por medida: tres minutos.
pub const MUESTRAS: u32 = 36;
const VENTAJA: f64 = 1.05;
const SEGUIDA: Duration = Duration::from_secs(30 * 60);
const PERMANENCIA: Duration = Duration::from_secs(30 * 60);

/// Velocidad y vatios de la gráfica medidos minando una moneda, suavizados.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Medida {
    pub tasa: f64,
    pub vatios: f64,
    #[serde(default = "completa")]
    pub muestras: u32,
}

fn completa() -> u32 {
    MUESTRAS
}

impl Medida {
    pub fn lista(&self) -> bool {
        self.muestras >= MUESTRAS
    }

    /// Las primeras muestras pesan más, para llegar pronto a la cifra; después cambia despacio.
    pub fn sumar(anterior: Option<Medida>, tasa: f64, vatios: f64) -> Medida {
        match anterior {
            None => Medida {
                tasa,
                vatios,
                muestras: 1,
            },
            Some(m) => {
                let peso = 1.0 / (m.muestras.min(MUESTRAS) as f64 + 1.0).min(10.0);
                Medida {
                    tasa: m.tasa + (tasa - m.tasa) * peso,
                    vatios: m.vatios + (vatios - m.vatios) * peso,
                    muestras: m.muestras.saturating_add(1),
                }
            }
        }
    }
}

pub type Medidas = HashMap<Moneda, Medida>;

/// Las medidas se guardan junto a los ajustes: tras reiniciar no hace falta volver a medir.
pub fn leer(ruta: &Path) -> Medidas {
    std::fs::read(ruta)
        .ok()
        .and_then(|b| serde_json::from_slice::<Medidas>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, m)| m.tasa > 0.0 && m.vatios >= 0.0)
        .collect()
}

pub fn guardar(ruta: &Path, medidas: &Medidas) {
    let listas: Medidas = medidas
        .iter()
        .filter(|(_, m)| m.lista())
        .map(|(k, v)| (*k, *v))
        .collect();
    if let Ok(texto) = serde_json::to_vec_pretty(&listas) {
        let _ = std::fs::write(ruta, texto);
    }
}

/// Lleva la cuenta de cuándo empezó la moneda actual y desde cuándo gana otra.
#[derive(Default)]
pub struct Selector {
    actual: Option<(Moneda, Instant)>,
    ventaja: Option<(Moneda, Instant)>,
    /// La actual se eligió sólo para medirla: medidas las dos, se va enseguida a la mejor.
    midiendo: bool,
}

impl Selector {
    pub fn empezar(&mut self, moneda: Moneda, ahora: Instant) {
        self.actual = Some((moneda, ahora));
        self.ventaja = None;
    }

    pub fn parar(&mut self) {
        self.actual = None;
        self.ventaja = None;
        self.midiendo = false;
    }

    /// La moneda que toca. `posibles` son las que esta cuenta puede minar; `beneficio` da los euros por hora tras la
    /// luz de una moneda ya medida (o `None` si falta el mercado).
    pub fn elegir(
        &mut self,
        eleccion: Eleccion,
        posibles: &[Moneda],
        medidas: &Medidas,
        beneficio: impl Fn(Moneda) -> Option<f64>,
        ahora: Instant,
    ) -> Moneda {
        let fija = match eleccion {
            Eleccion::Pearl => Some(Moneda::Pearl),
            Eleccion::Quantus => Some(Moneda::Quantus),
            Eleccion::Auto => None,
        };
        if let Some(moneda) = fija.filter(|m| posibles.contains(m)) {
            return moneda;
        }
        let Some(&primera) = posibles.first() else {
            return Moneda::Pearl;
        };
        if posibles.len() == 1 || fija.is_some() {
            return primera;
        }
        let actual = self.actual.map(|(m, _)| m);
        let medida = |m: Moneda| medidas.get(&m).is_some_and(Medida::lista);
        // Primero se mide lo que falte, empezando por la actual.
        if let Some(m) = actual.filter(|m| !medida(*m)) {
            self.midiendo = true;
            return m;
        }
        if let Some(&m) = posibles.iter().find(|m| !medida(**m)) {
            self.midiendo = true;
            return m;
        }
        let ganancias: Vec<(Moneda, f64)> = posibles
            .iter()
            .filter_map(|&m| Some((m, beneficio(m)?)))
            .collect();
        let Some(&(mejor, gana_mejor)) = ganancias.iter().max_by(|a, b| a.1.total_cmp(&b.1)) else {
            return actual.unwrap_or(primera);
        };
        let Some((moneda, desde)) = self.actual.filter(|_| !std::mem::take(&mut self.midiendo))
        else {
            return mejor;
        };
        let gana_actual = ganancias.iter().find(|(m, _)| *m == moneda).map(|g| g.1);
        let supera =
            gana_actual.is_none_or(|g| gana_mejor - g > (VENTAJA - 1.0) * g.abs().max(0.001));
        if mejor == moneda || !supera {
            self.ventaja = None;
            return moneda;
        }
        let empieza = match self.ventaja {
            Some((m, inicio)) if m == mejor => inicio,
            _ => {
                self.ventaja = Some((mejor, ahora));
                ahora
            }
        };
        if ahora.duration_since(empieza) >= SEGUIDA && ahora.duration_since(desde) >= PERMANENCIA {
            mejor
        } else {
            moneda
        }
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;

    const AMBAS: [Moneda; 2] = [Moneda::Pearl, Moneda::Quantus];

    fn medidas(pearl: bool, quantus: bool) -> Medidas {
        let mut m = Medidas::new();
        if pearl {
            m.insert(
                Moneda::Pearl,
                Medida {
                    tasa: 80e12,
                    vatios: 110.0,
                    muestras: MUESTRAS,
                },
            );
        }
        if quantus {
            m.insert(
                Moneda::Quantus,
                Medida {
                    tasa: 277e6,
                    vatios: 105.0,
                    muestras: MUESTRAS,
                },
            );
        }
        m
    }

    #[test]
    fn mide_cada_moneda_antes_de_comparar() {
        let mut s = Selector::default();
        let t = Instant::now();
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &AMBAS,
                &medidas(false, false),
                |_| Some(1.0),
                t
            ),
            Moneda::Pearl
        );
        s.empezar(Moneda::Pearl, t);
        // Mientras Pearl no está medida, sigue en Pearl; después mide Quantus.
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &AMBAS,
                &medidas(false, false),
                |_| Some(1.0),
                t
            ),
            Moneda::Pearl
        );
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &AMBAS,
                &medidas(true, false),
                |_| Some(1.0),
                t
            ),
            Moneda::Quantus
        );
    }

    #[test]
    fn tras_medir_va_enseguida_a_la_mejor() {
        let mut s = Selector::default();
        let t = Instant::now();
        let gana = |c: Moneda| Some(if c == Moneda::Pearl { 0.05 } else { 0.04 });
        assert_eq!(
            s.elegir(Eleccion::Auto, &AMBAS, &medidas(true, false), gana, t),
            Moneda::Quantus
        );
        s.empezar(Moneda::Quantus, t);
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &AMBAS,
                &medidas(true, true),
                gana,
                t + Duration::from_secs(200)
            ),
            Moneda::Pearl
        );
    }

    #[test]
    fn cambia_solo_con_ventaja_clara_y_sostenida() {
        let mut s = Selector::default();
        let t0 = Instant::now();
        let m = medidas(true, true);
        let gana = |pearl: f64, quantus: f64| {
            move |c: Moneda| Some(if c == Moneda::Pearl { pearl } else { quantus })
        };
        assert_eq!(
            s.elegir(Eleccion::Auto, &AMBAS, &m, gana(0.05, 0.04), t0),
            Moneda::Pearl
        );
        s.empezar(Moneda::Pearl, t0);
        // Un 4 % más no basta.
        assert_eq!(
            s.elegir(Eleccion::Auto, &AMBAS, &m, gana(0.050, 0.052), t0),
            Moneda::Pearl
        );
        // Un 20 % más, pero sólo un rato: espera.
        let t1 = t0 + Duration::from_secs(40 * 60);
        assert_eq!(
            s.elegir(Eleccion::Auto, &AMBAS, &m, gana(0.05, 0.06), t1),
            Moneda::Pearl
        );
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &AMBAS,
                &m,
                gana(0.05, 0.06),
                t1 + Duration::from_secs(29 * 60)
            ),
            Moneda::Pearl
        );
        // Si la ventaja se corta, vuelve a contar desde cero.
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &AMBAS,
                &m,
                gana(0.05, 0.05),
                t1 + Duration::from_secs(30 * 60)
            ),
            Moneda::Pearl
        );
        let t2 = t1 + Duration::from_secs(31 * 60);
        assert_eq!(
            s.elegir(Eleccion::Auto, &AMBAS, &m, gana(0.05, 0.06), t2),
            Moneda::Pearl
        );
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &AMBAS,
                &m,
                gana(0.05, 0.06),
                t2 + Duration::from_secs(30 * 60)
            ),
            Moneda::Quantus
        );
    }

    #[test]
    fn la_moneda_fija_y_la_cuenta_mandan() {
        let mut s = Selector::default();
        let t = Instant::now();
        let m = medidas(true, true);
        assert_eq!(
            s.elegir(Eleccion::Quantus, &AMBAS, &m, |_| Some(1.0), t),
            Moneda::Quantus
        );
        assert_eq!(
            s.elegir(Eleccion::Pearl, &AMBAS, &m, |_| Some(1.0), t),
            Moneda::Pearl
        );
        // Una dirección de Pearl no cobra Quantus.
        assert_eq!(
            s.elegir(Eleccion::Quantus, &[Moneda::Pearl], &m, |_| Some(1.0), t),
            Moneda::Pearl
        );
        assert_eq!(
            s.elegir(
                Eleccion::Auto,
                &[Moneda::Pearl],
                &medidas(false, false),
                |_| Some(1.0),
                t
            ),
            Moneda::Pearl
        );
    }

    #[test]
    fn la_medida_se_suaviza_y_se_guarda() {
        let mut m = Medida::sumar(None, 100.0, 100.0);
        for _ in 0..MUESTRAS {
            m = Medida::sumar(Some(m), 80.0, 110.0);
        }
        assert!(
            m.lista() && (m.tasa - 80.0).abs() < 1.0 && (m.vatios - 110.0).abs() < 1.0,
            "{m:?}"
        );
        let ruta =
            std::env::temp_dir().join(format!("tokenminer-medidas-{}.json", std::process::id()));
        let mut todas = Medidas::new();
        todas.insert(Moneda::Pearl, m);
        todas.insert(Moneda::Quantus, Medida::sumar(None, 1.0, 1.0));
        guardar(&ruta, &todas);
        let leidas = leer(&ruta);
        assert_eq!(leidas.len(), 1);
        assert!(leidas[&Moneda::Pearl].lista());
        std::fs::remove_file(ruta).unwrap();
    }
}
