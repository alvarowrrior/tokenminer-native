//! ¿Renta minar ahora? Lo que se gana sale de la velocidad real y de lo que paga Kryptex; lo que cuesta, del consumo
//! real de la gráfica y del precio de la luz. Todo en euros. Sólo se leen datos públicos: no se envía nada del usuario.
use crate::ajustes::{Electricidad, Moneda, Tarifa};
use serde_json::Value;
use std::time::Duration;

const KRYPTEX: &str = "https://pool.kryptex.com";
const CAMBIO: &str = "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml";
const PVPC: &str = "https://apidatos.ree.es/es/datos/mercados/precios-mercados-tiempo-real";

/// IVA (21 %) e impuesto eléctrico (5,11 %) sobre el precio PVPC, que Red Eléctrica publica sin ellos. Faltan los
/// términos fijos de la factura, que no cambian por minar.
const IMPUESTOS_PVPC: f64 = 1.0511 * 1.21;

/// Lo que cambia con el mercado. Se vuelve a leer cada poco.
#[derive(Clone, Debug, PartialEq)]
pub struct Mercado {
    /// Euros por hora por unidad de velocidad (MAC/s en Pearl, H/s en Quantus), ya sin la comisión del pool.
    pub pearl: f64,
    pub quantus: Option<f64>,
    /// Precio de la luz en España peninsular por hora local, en €/kWh con impuestos. `None` si no se pudo leer.
    pub pvpc: Option<[Option<f64>; 24]>,
}

impl Mercado {
    pub fn eur_hora_por_tasa(&self, moneda: Moneda) -> Option<f64> {
        match moneda {
            Moneda::Pearl => Some(self.pearl),
            Moneda::Quantus => self.quantus,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cuentas {
    pub gana_hora: f64,
    pub cuesta_hora: f64,
    pub precio_kwh: f64,
}

impl Cuentas {
    pub fn beneficio(&self) -> f64 {
        self.gana_hora - self.cuesta_hora
    }
}

fn pool_por_tasa(info: &Value) -> Option<f64> {
    let (bloque, recompensa, comision, red) = (
        info["block_time"].as_f64()?,
        info["block_reward"].as_f64()?,
        info["fee"].as_f64()?,
        info["net_hashrate"].as_f64()?,
    );
    (bloque > 0.0 && red > 0.0).then(|| 3600.0 / bloque * recompensa * (1.0 - comision) / red)
}

fn ultimo_precio(precios: &Value) -> Option<f64> {
    precios.as_array()?.last()?["price"]
        .as_f64()
        .filter(|p| *p > 0.0)
}

fn usd_por_eur(xml: &str) -> Option<f64> {
    let resto = &xml[xml.find("currency='USD'")?..];
    let inicio = resto.find("rate='")? + 6;
    resto[inicio..inicio + resto[inicio..].find('\'')?]
        .parse()
        .ok()
        .filter(|r: &f64| *r > 0.0)
}

/// La fecha lleva la hora local española: `2026-11-03T05:00:00.000+01:00`.
fn pvpc_por_hora(datos: &Value) -> Option<[Option<f64>; 24]> {
    let serie = datos["included"]
        .as_array()?
        .iter()
        .find(|s| s["type"] == "PVPC")?;
    let mut horas = [None; 24];
    for punto in serie["attributes"]["values"].as_array()? {
        let hora: usize = punto["datetime"].as_str()?.get(11..13)?.parse().ok()?;
        if let (Some(destino), Some(mwh)) = (horas.get_mut(hora), punto["value"].as_f64()) {
            *destino = Some(mwh / 1000.0 * IMPUESTOS_PVPC);
        }
    }
    horas.iter().any(Option::is_some).then_some(horas)
}

async fn leer_json(cliente: &reqwest::Client, url: &str) -> Result<Value, String> {
    cliente
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// Dólares por hora por unidad de velocidad de una moneda en Kryptex (`prl`, `qtc`).
async fn moneda(cliente: &reqwest::Client, ticker: &str) -> Result<f64, String> {
    let pool =
        pool_por_tasa(&leer_json(cliente, &format!("{KRYPTEX}/{ticker}/api/v1/pool/info")).await?)
            .ok_or("Datos del pool incompletos")?;
    let precio = ultimo_precio(
        &leer_json(
            cliente,
            &format!("{KRYPTEX}/api/v1/coin/{ticker}/price/chart?time_range=day"),
        )
        .await?,
    )
    .ok_or("Falta el precio de la moneda")?;
    Ok(pool * precio)
}

/// Lee el mercado. Sin el precio de la luz se puede seguir con un precio fijo, y sin Quantus con Pearl; sin lo que
/// paga Pearl, no.
pub async fn leer(fecha: &str) -> Result<Mercado, String> {
    let cliente = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("TokenMiner/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let pearl = moneda(&cliente, "prl").await?;
    let quantus = moneda(&cliente, "qtc").await.ok();
    let xml = cliente
        .get(CAMBIO)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let cambio = usd_por_eur(&xml).ok_or("Falta el cambio del euro")?;
    let pvpc_url = format!("{PVPC}?start_date={fecha}T00:00&end_date={fecha}T23:59&time_trunc=hour&geo_limit=peninsular");
    let pvpc = leer_json(&cliente, &pvpc_url)
        .await
        .ok()
        .and_then(|d| pvpc_por_hora(&d));
    Ok(Mercado {
        pearl: pearl / cambio,
        quantus: quantus.map(|q| q / cambio),
        pvpc,
    })
}

/// `tasa` y `vatios` son los medidos al minar esa moneda; `parte` lo que se queda el usuario tras la comisión.
pub fn cuentas(
    m: &Mercado,
    moneda: Moneda,
    luz: &Electricidad,
    tasa: f64,
    vatios: f64,
    parte: f64,
    hora: u32,
) -> Option<Cuentas> {
    let precio_kwh = match luz.kind {
        Tarifa::Fija => luz.price_kwh?,
        Tarifa::Pvpc => m.pvpc.as_ref()?.get(hora as usize).copied().flatten()?,
    };
    Some(Cuentas {
        gana_hora: tasa * m.eur_hora_por_tasa(moneda)? * parte,
        cuesta_hora: (vatios + luz.extra_watts) / 1000.0 * precio_kwh,
        precio_kwh,
    })
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use serde_json::json;

    #[test]
    fn lee_cada_fuente() {
        let info = json!({"block_time": 194.0, "block_reward": 2200.0, "fee": 0.03, "net_hashrate": 7.0e19});
        let por_tasa = pool_por_tasa(&info).unwrap();
        // 80 TH/s dan algo más de 1 PRL al día, como en las cuentas a mano.
        assert!(
            (por_tasa * 80e12 * 24.0 - 1.086).abs() < 0.001,
            "{}",
            por_tasa * 80e12 * 24.0
        );
        assert_eq!(pool_por_tasa(&json!({"block_time": 0.0})), None);
        assert_eq!(
            ultimo_precio(&json!([{"price": 1.4}, {"price": 1.5}])),
            Some(1.5)
        );
        assert_eq!(ultimo_precio(&json!([])), None);
        assert_eq!(
            usd_por_eur("<Cube currency='JPY' rate='160'/><Cube currency='USD' rate='1.1216'/>"),
            Some(1.1216)
        );
        assert_eq!(usd_por_eur("<nada/>"), None);
        let ree = json!({"included": [
            {"type": "Precio mercado spot", "attributes": {"values": [{"value": 1.0, "datetime": "2026-11-03T00:00:00.000+01:00"}]}},
            {"type": "PVPC", "attributes": {"values": [
                {"value": 207.25, "datetime": "2026-11-03T00:00:00.000+01:00"},
                {"value": 45.96, "datetime": "2026-11-03T05:00:00.000+01:00"}]}}]});
        let horas = pvpc_por_hora(&ree).unwrap();
        // Con IVA e impuesto eléctrico.
        assert!(
            (horas[0].unwrap() - 0.20725 * 1.271831).abs() < 1e-6
                && (horas[5].unwrap() - 0.04596 * 1.271831).abs() < 1e-6
        );
        assert_eq!(horas[6], None);
        assert_eq!(pvpc_por_hora(&json!({"included": []})), None);
    }

    #[tokio::test]
    #[ignore = "Lee Kryptex, el BCE y Red Eléctrica por internet"]
    async fn lee_el_mercado_real() {
        let m = leer(&crate::ajustes::fecha_de_hoy()).await.unwrap();
        let pvpc = m.pvpc.unwrap();
        let quantus = m.quantus.unwrap();
        eprintln!(
            "80 TH/s de Pearl: {:.4} €/h; 277 MH/s de Quantus: {:.4} €/h; luz de {:?} a {:?} €/kWh",
            m.pearl * 80e12,
            quantus * 277e6,
            pvpc.iter().flatten().fold(f64::MAX, |a, &b| a.min(b)),
            pvpc.iter().flatten().fold(0.0f64, |a, &b| a.max(b))
        );
        assert!(m.pearl * 80e12 > 0.01 && m.pearl * 80e12 < 1.0);
        assert!(quantus * 277e6 > 0.005 && quantus * 277e6 < 1.0);
        assert!(pvpc.iter().flatten().count() >= 23);
    }

    #[test]
    fn compara_lo_ganado_con_la_luz() {
        let mut pvpc = [None; 24];
        pvpc[3] = Some(0.05);
        pvpc[20] = Some(0.50);
        let m = Mercado {
            pearl: 7.0e-16,
            quantus: None,
            pvpc: Some(pvpc),
        };
        let mut luz = Electricidad {
            kind: Tarifa::Pvpc,
            price_kwh: None,
            extra_watts: 20.0,
        };
        assert_eq!(
            cuentas(&m, Moneda::Quantus, &luz, 277e6, 115.0, 1.0, 3),
            None
        );
        let cuentas = |m: &Mercado, luz: &Electricidad, tasa, vatios, parte, hora| {
            cuentas(m, Moneda::Pearl, luz, tasa, vatios, parte, hora)
        };
        let barata = cuentas(&m, &luz, 80e12, 115.0, 1.0, 3).unwrap();
        assert!((barata.gana_hora - 0.056).abs() < 1e-9);
        assert!((barata.cuesta_hora - 0.135 * 0.05).abs() < 1e-9);
        assert!(barata.beneficio() > 0.0);
        assert!(
            cuentas(&m, &luz, 80e12, 115.0, 1.0, 20)
                .unwrap()
                .beneficio()
                < 0.0
        );
        assert_eq!(cuentas(&m, &luz, 80e12, 115.0, 1.0, 4), None);
        let con_comision = cuentas(&m, &luz, 80e12, 115.0, 0.9, 3).unwrap();
        assert!((con_comision.gana_hora - 0.0504).abs() < 1e-9);

        luz.kind = Tarifa::Fija;
        assert_eq!(cuentas(&m, &luz, 80e12, 115.0, 1.0, 20), None);
        luz.price_kwh = Some(0.15);
        assert!(
            (cuentas(&m, &luz, 80e12, 115.0, 1.0, 20)
                .unwrap()
                .cuesta_hora
                - 0.02025)
                .abs()
                < 1e-9
        );
    }
}
