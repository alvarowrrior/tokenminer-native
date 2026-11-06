//! Programa de fondo: decide cuándo minar y qué moneda según sus ajustes, y sigue funcionando con la web cerrada.
//!
//! Sólo las webs de los orígenes fijados al compilar pueden conectarse; el icono abre la primera. Al conectar y en cada cambio recibe
//! `{"type":"state",…}`. Manda `{"type":"start"}`, `{"type":"stop"}`, `{"type":"settings","settings":{…}}` y
//! `{"type":"quit"}`. La GPU sólo se ocupa mientras se mina: al parar se libera el motor.
use crate::ajustes::{self, Ajustes, Cuando, Momento, Moneda, Plan};
use crate::cambio::{self, Medida, Medidas, Selector};
use crate::economia::{self, Cuentas, Mercado};
use crate::pool::{wallet_comision, COMISION};
use crate::reparto::SIN_LIMITE;
use crate::{accept_from, bandeja, PORT};
use crate::{pearl, quantus};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

const REPASO: Duration = Duration::from_secs(15);
const REINTENTO: Duration = Duration::from_secs(60);
// Sin trabajo, sin página y sin nada programado no se queda ocupando el equipo.
const ESPERA_CIERRE: Duration = Duration::from_secs(60);
const ORDEN_DE_SALIDA: &str = "Local\\TokenMinerSalir";
const LECTURA_GPU: Duration = Duration::from_secs(5);
const SIN_VENTANA: u32 = 0x0800_0000;
const LECTURA_MERCADO: Duration = Duration::from_secs(15 * 60);

/// La sesión de la moneda que se mina.
enum Sesion {
    Pearl(pearl::Session),
    Quantus(quantus::Session),
}

impl Sesion {
    fn empezar(moneda: Moneda, eventos: UnboundedSender<String>, limite: Arc<AtomicU32>) -> Self {
        match moneda {
            Moneda::Pearl => Self::Pearl(pearl::Session::start(eventos, limite)),
            Moneda::Quantus => Self::Quantus(quantus::Session::start(eventos, limite)),
        }
    }

    fn minar(&self, wallet: &str) -> Result<(), String> {
        match self {
            Self::Pearl(s) => s.minar(wallet),
            Self::Quantus(s) => s.minar(wallet),
        }
    }

    fn finish(self) {
        match self {
            Self::Pearl(s) => s.finish(),
            Self::Quantus(s) => s.finish(),
        }
    }
}

enum Entrada {
    Web(String),
    Bandeja(bandeja::Orden),
    Gpus(Vec<Gpu>),
    Mercado(Mercado),
}

/// Lo que `nvidia-smi` informa de una gráfica. Sólo se lee: nada de esto cambia ajustes de la GPU.
#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct Gpu {
    name: String,
    use_percent: Option<f64>,
    memory_used_mb: Option<f64>,
    memory_total_mb: Option<f64>,
    temperature_c: Option<f64>,
    power_w: Option<f64>,
    /// Reloj de los multiprocesadores ahora mismo, en MHz.
    clock_mhz: Option<f64>,
}

fn leer_tabla(salida: &str) -> Vec<Gpu> {
    salida
        .lines()
        .filter_map(|linea| {
            let campos: Vec<&str> = linea.split(',').map(str::trim).collect();
            let [name, usado, memoria, total, temperatura, potencia, reloj] = campos[..] else {
                return None;
            };
            if name.is_empty() {
                return None;
            }
            Some(Gpu {
                name: name.to_string(),
                use_percent: usado.parse().ok(),
                memory_used_mb: memoria.parse().ok(),
                memory_total_mb: total.parse().ok(),
                temperature_c: temperatura.parse().ok(),
                power_w: potencia.parse().ok(),
                clock_mhz: reloj.parse().ok(),
            })
        })
        .collect()
}

async fn leer_gpus() -> Vec<Gpu> {
    let salida = tokio::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,clocks.sm",
            "--format=csv,noheader,nounits",
        ])
        .creation_flags(SIN_VENTANA)
        .output()
        .await;
    salida
        .map(|s| leer_tabla(&String::from_utf8_lossy(&s.stdout)))
        .unwrap_or_default()
}

/// Techo teórico en MAC/s de una GPU que mina `moneda`: multiprocesadores por reloj de ahora. Sólo Pearl multiplica
/// matrices en los tensor cores; sin multiprocesadores o sin reloj no hay techo.
fn techo(moneda: Option<Moneda>, sm: Option<u32>, reloj_mhz: Option<f64>) -> Option<f64> {
    match (moneda, sm, reloj_mhz) {
        (Some(Moneda::Pearl), Some(sm), Some(mhz)) if mhz > 0.0 => {
            Some(tokenminer_pearl::techo_macs(sm, mhz))
        }
        _ => None,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Estado<'a> {
    r#type: &'a str,
    version: &'a str,
    fee_percent: u32,
    settings: &'a Ajustes,
    phase: &'a str,
    rate: f64,
    accepted: u64,
    rejected: u64,
    problem: Option<&'a str>,
    gpus: &'a [Gpu],
    /// Lo que hace el motor en cada GPU, en el mismo orden que `gpus`.
    devices: &'a [Dispositivo],
    /// La moneda que se mina, y lo que dejaría cada una.
    coin: Option<Moneda>,
    coins: Vec<Opcion>,
    economics: Option<Economia>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Opcion {
    coin: Moneda,
    /// Si ya se ha medido en este equipo; sin medida no hay cifras.
    measured: bool,
    earn_per_hour: Option<f64>,
    cost_per_hour: Option<f64>,
}

/// Velocidad del motor en una GPU, la parte de su tiempo que le toca (0: apartado para otro programa) y el uso de los
/// demás programas, en %.
#[derive(Serialize, Clone, Default)]
struct Dispositivo {
    rate: f64,
    share: Option<u32>,
    others: Option<f64>,
    /// Lo que usa el propio motor, en %, según Windows.
    mine: Option<f64>,
    /// Multiprocesadores de la GPU, si el motor ya los ha dicho.
    sm: Option<u32>,
    /// Techo teórico de esta GPU con la moneda que se mina, en la misma unidad que `rate` y al reloj de ahora: sólo
    /// Pearl, que multiplica matrices en los tensor cores. `rate` entre `peak` es lo cerca que está del límite.
    peak: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Economia {
    earn_per_hour: f64,
    cost_per_hour: f64,
    price_kwh: f64,
    profitable: bool,
}

struct Residente {
    ajustes: Ajustes,
    ruta: PathBuf,
    plan: Plan,
    sesion: Option<Sesion>,
    eventos: UnboundedSender<String>,
    fase: &'static str,
    rate: f64,
    accepted: u64,
    rejected: u64,
    problema: Option<String>,
    reintento: Option<Instant>,
    estado: watch::Sender<String>,
    gpus: Vec<Gpu>,
    mercado: Option<Mercado>,
    /// Velocidad y consumo de la gráfica medidos con cada moneda.
    medidas: Medidas,
    ruta_medidas: PathBuf,
    selector: Selector,
    moneda: Option<Moneda>,
    minando: Arc<AtomicBool>,
    /// Uso total máximo de la GPU que lee el motor mientras mina.
    limite: Arc<AtomicU32>,
    dispositivos: Vec<Dispositivo>,
}

impl Residente {
    /// Los dispositivos con su techo teórico: multiprocesadores por reloj de ahora, sólo para Pearl.
    fn con_techo(&self) -> Vec<Dispositivo> {
        self.dispositivos
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let reloj = self.gpus.get(i).and_then(|g| g.clock_mhz);
                Dispositivo {
                    peak: techo(self.moneda, d.sm, reloj),
                    ..d.clone()
                }
            })
            .collect()
    }

    fn publicar(&self) {
        let dispositivos = self.con_techo();
        let estado = Estado {
            r#type: "state",
            version: env!("CARGO_PKG_VERSION"),
            fee_percent: COMISION,
            settings: &self.ajustes,
            phase: self.fase,
            rate: self.rate,
            accepted: self.accepted,
            rejected: self.rejected,
            problem: self.problema.as_deref(),
            gpus: &self.gpus,
            devices: &dispositivos,
            coin: self.moneda,
            coins: self
                .posibles()
                .into_iter()
                .map(|m| {
                    let c = self.cuentas_de(m);
                    Opcion {
                        coin: m,
                        measured: self.medidas.get(&m).is_some_and(Medida::lista),
                        earn_per_hour: self.gana_de(m),
                        cost_per_hour: c.map(|c| c.cuesta_hora),
                    }
                })
                .collect(),
            economics: self.cuentas().map(|c| Economia {
                earn_per_hour: c.gana_hora,
                cost_per_hour: c.cuesta_hora,
                price_kwh: c.precio_kwh,
                profitable: c.beneficio() > 0.0,
            }),
        };
        self.estado
            .send_replace(serde_json::to_string(&estado).unwrap_or_default());
        bandeja::fijar_minando(self.sesion.is_some());
        self.minando.store(self.sesion.is_some(), Ordering::SeqCst);
    }

    /// Las monedas que esta cuenta puede minar: una dirección de Pearl sólo cobra Pearl, y Quantus necesita que se
    /// pueda leer lo que paga.
    fn posibles(&self) -> Vec<Moneda> {
        let quantus = quantus::puede_minar(&self.ajustes.wallet)
            && self.mercado.as_ref().is_none_or(|m| m.quantus.is_some());
        Moneda::TODAS
            .into_iter()
            .filter(|m| *m == Moneda::Pearl || quantus)
            .collect()
    }

    fn parte(&self) -> f64 {
        if wallet_comision(&self.ajustes.wallet).is_some() {
            1.0 - COMISION as f64 / 100.0
        } else {
            1.0
        }
    }

    fn medida_de(&self, moneda: Moneda) -> Option<&Medida> {
        self.medidas
            .get(&moneda)
            .filter(|m| m.lista() || self.moneda == Some(moneda))
    }

    fn cuentas_de(&self, moneda: Moneda) -> Option<Cuentas> {
        let medida = self.medida_de(moneda)?;
        economia::cuentas(
            self.mercado.as_ref()?,
            moneda,
            &self.ajustes.electricity,
            medida.tasa,
            medida.vatios,
            self.parte(),
            Momento::ahora().hora,
        )
    }

    /// Lo que se gana por hora con una moneda, se sepa o no el precio de la luz.
    fn gana_de(&self, moneda: Moneda) -> Option<f64> {
        Some(
            self.medida_de(moneda)?.tasa
                * self.mercado.as_ref()?.eur_hora_por_tasa(moneda)?
                * self.parte(),
        )
    }

    /// Para elegir moneda: lo que queda tras la luz, o lo ganado si no se sabe el precio de la luz.
    fn beneficio_de(&self, moneda: Moneda) -> Option<f64> {
        self.cuentas_de(moneda)
            .map(|c| c.beneficio())
            .or_else(|| self.gana_de(moneda))
    }

    /// Las cuentas de la moneda que se mina, o de la que se minaría.
    fn cuentas(&self) -> Option<Cuentas> {
        self.cuentas_de(self.moneda.unwrap_or_else(|| self.siguiente()))
    }

    /// La moneda que toca, sin cambiar nada.
    fn siguiente(&self) -> Moneda {
        let mut selector = Selector::default();
        if let Some(m) = self.moneda {
            selector.empezar(m, Instant::now());
        }
        self.elegir_con(&mut selector)
    }

    fn elegir_con(&self, selector: &mut Selector) -> Moneda {
        selector.elegir(
            self.ajustes.coin,
            &self.posibles(),
            &self.medidas,
            |m| self.beneficio_de(m),
            Instant::now(),
        )
    }

    /// Sin medida o sin precios no se sabe: se mina, y al minar se mide. Para no parar y arrancar por poco, minando se
    /// sigue mientras lo ganado no baje del 95 % de lo que cuesta.
    fn renta(&self) -> bool {
        match self.cuentas() {
            None => true,
            Some(c) if self.sesion.is_some() => c.gana_hora >= 0.95 * c.cuesta_hora,
            Some(c) => c.gana_hora > c.cuesta_hora,
        }
    }

    fn arrancar(&mut self) {
        let mut selector = std::mem::take(&mut self.selector);
        let moneda = self.elegir_con(&mut selector);
        selector.empezar(moneda, Instant::now());
        self.selector = selector;
        (self.fase, self.rate, self.accepted, self.rejected) = ("starting", 0.0, 0, 0);
        self.moneda = Some(moneda);
        self.sesion = Some(Sesion::empezar(
            moneda,
            self.eventos.clone(),
            self.limite.clone(),
        ));
    }

    fn parar(&mut self) {
        if let Some(sesion) = self.sesion.take() {
            sesion.finish();
        }
        (self.fase, self.rate, self.moneda) = ("idle", 0.0, None);
        self.selector.parar();
        self.dispositivos.clear();
    }

    /// Minando, cambia de moneda si toca otra.
    fn cambiar_moneda(&mut self) {
        let Some(actual) = self.moneda.filter(|_| self.sesion.is_some()) else {
            return;
        };
        let mut selector = std::mem::take(&mut self.selector);
        let elegida = self.elegir_con(&mut selector);
        self.selector = selector;
        if elegida != actual {
            self.parar();
            self.arrancar();
        }
    }

    /// La medida sirve para las cuentas de la luz sólo si la gráfica era casi toda del motor: con un juego, el consumo
    /// es sobre todo del juego. El escritorio, un fondo animado o el navegador ya ocupan un 5–15 %.
    fn medir(&mut self, vatios: f64) {
        let Some(moneda) = self.moneda else { return };
        if self.fase == "mining"
            && self.rate > 0.0
            && self
                .dispositivos
                .iter()
                .all(|d| d.others.is_none_or(|o| o < 25.0))
        {
            let medida = Medida::sumar(self.medidas.get(&moneda).copied(), self.rate, vatios);
            self.medidas.insert(moneda, medida);
            // Al quedar medida y después cada cuarto de hora.
            if medida.muestras == cambio::MUESTRAS || medida.muestras % (12 * 15) == 0 {
                cambio::guardar(&self.ruta_medidas, &self.medidas);
            }
        }
    }

    /// Compara lo que toca con lo que hay.
    fn repasar(&mut self) {
        let toca = self.plan.quiere(&self.ajustes, Momento::ahora());
        // A start requested manually overrides the time plan, but not this profitability gate.
        let renta = !self.ajustes.only_profitable || self.renta();
        let quiere = toca && renta;
        if !quiere {
            self.reintento = None;
        }
        let esperando = self.reintento.is_some_and(|hora| Instant::now() < hora);
        if quiere && !esperando && self.sesion.is_none() {
            self.arrancar();
        } else if !quiere && self.sesion.is_some() {
            self.parar();
        } else if quiere {
            self.cambiar_moneda();
        }
        if toca && !renta {
            self.fase = "waiting";
        } else if self.sesion.is_none() {
            self.fase = "idle";
        }
        self.publicar();
    }

    fn evento(&mut self, cuerpo: &str) {
        let Ok(v) = serde_json::from_str::<Value>(cuerpo) else {
            return;
        };
        match v["type"].as_str() {
            Some("ready") => {
                if let Some(Err(e)) = self.sesion.as_ref().map(|s| s.minar(&self.ajustes.wallet)) {
                    self.fallo(e);
                }
            }
            Some("pool") => {
                self.fase = match v["state"].as_str() {
                    Some("connected") => {
                        self.problema = None;
                        "mining"
                    }
                    Some("reconnecting") => "reconnecting",
                    _ => "connecting",
                };
                if self.fase != "mining" {
                    self.rate = 0.0;
                }
            }
            Some("stats") => {
                self.rate = v["rate"].as_f64().unwrap_or(0.0);
                for (i, tasa) in v["devices"].as_array().into_iter().flatten().enumerate() {
                    self.dispositivo(i).rate = tasa.as_f64().unwrap_or(0.0);
                }
                for (i, sm) in v["sm"].as_array().into_iter().flatten().enumerate() {
                    self.dispositivo(i).sm = sm.as_u64().map(|s| s as u32).filter(|s| *s > 0);
                }
            }
            Some("gpu") => {
                for (i, d) in v["devices"].as_array().into_iter().flatten().enumerate() {
                    let dispositivo = self.dispositivo(i);
                    (dispositivo.share, dispositivo.others, dispositivo.mine) = (
                        d["share"].as_u64().map(|p| p as u32),
                        d["others"].as_f64(),
                        d["mine"].as_f64(),
                    );
                }
            }
            Some("share") if v["accepted"] == true => self.accepted += 1,
            Some("share") => self.rejected += 1,
            Some("error") => self.fallo(
                v["message"]
                    .as_str()
                    .unwrap_or("Error desconocido")
                    .to_string(),
            ),
            _ => {}
        }
        self.publicar();
    }

    fn dispositivo(&mut self, i: usize) -> &mut Dispositivo {
        if self.dispositivos.len() <= i {
            self.dispositivos.resize(i + 1, Dispositivo::default());
        }
        &mut self.dispositivos[i]
    }

    /// El motor no se recupera solo. Con minado automático se reintenta; a mano decide el usuario.
    fn fallo(&mut self, mensaje: String) {
        self.problema = Some(mensaje);
        self.parar();
        if self.ajustes.when == Cuando::Manual {
            self.plan.ordenar(false);
        } else {
            self.reintento = Some(Instant::now() + REINTENTO);
        }
    }

    fn cambiar_ajustes(&mut self, nuevos: Ajustes) -> Result<(), String> {
        nuevos.validar()?;
        if nuevos.start_with_pc != self.ajustes.start_with_pc {
            ajustes::fijar_arranque(nuevos.start_with_pc)
                .map_err(|e| format!("No se pudo cambiar el arranque: {e}"))?;
        }
        nuevos
            .guardar(&self.ruta)
            .map_err(|e| format!("No se pudieron guardar los ajustes: {e}"))?;
        if nuevos.wallet != self.ajustes.wallet || nuevos.coin != self.ajustes.coin {
            self.parar();
        }
        self.limite
            .store(nuevos.gpu_limit.unwrap_or(SIN_LIMITE), Ordering::Relaxed);
        self.ajustes = nuevos;
        Ok(())
    }

    /// Un mensaje de la web. Devuelve `true` si pide cerrar el programa.
    fn web(&mut self, cuerpo: &str) -> bool {
        let Ok(v) = serde_json::from_str::<Value>(cuerpo) else {
            return false;
        };
        match v["type"].as_str() {
            Some("start") => {
                self.problema = None;
                self.reintento = None;
                self.plan.ordenar(true);
            }
            Some("stop") => self.plan.ordenar(false),
            Some("settings") => {
                let cambio = serde_json::from_value::<Ajustes>(v["settings"].clone())
                    .map_err(|e| e.to_string())
                    .and_then(|nuevos| self.cambiar_ajustes(nuevos));
                self.problema = cambio.err();
            }
            Some("quit") => return true,
            _ => {}
        }
        self.repasar();
        false
    }

    fn puede_cerrarse(&self, paginas: usize) -> bool {
        paginas == 0
            && self.sesion.is_none()
            && !self.ajustes.start_with_pc
            && self.ajustes.when == Cuando::Manual
    }
}

async fn cliente(
    stream: TcpStream,
    origen: Arc<str>,
    entrada: UnboundedSender<Entrada>,
    mut estado: watch::Receiver<String>,
    paginas: Arc<AtomicUsize>,
) {
    let Ok(Ok(socket)) = tokio::time::timeout(
        Duration::from_secs(8),
        accept_from(stream, origen.to_string()),
    )
    .await
    else {
        return;
    };
    paginas.fetch_add(1, Ordering::SeqCst);
    let (mut salida, mut lectura) = socket.split();
    let mut pings = tokio::time::interval(Duration::from_secs(10));
    let mut ultimo_pong = Instant::now();
    estado.mark_changed();
    loop {
        tokio::select! {
            mensaje = lectura.next() => match mensaje {
                Some(Ok(Message::Text(cuerpo))) if cuerpo.len() <= 8192 => {
                    let _ = entrada.send(Entrada::Web(cuerpo));
                }
                Some(Ok(Message::Pong(_))) => ultimo_pong = Instant::now(),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
            cambio = estado.changed() => {
                if cambio.is_err() {
                    break;
                }
                let actual = estado.borrow_and_update().clone();
                if salida.send(Message::Text(actual)).await.is_err() {
                    break;
                }
            }
            _ = pings.tick() => {
                if ultimo_pong.elapsed() > Duration::from_secs(30) || salida.send(Message::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
        }
    }
    paginas.fetch_sub(1, Ordering::SeqCst);
}

/// El instalador pide cerrar con este evento para poder cambiar el ejecutable.
fn esperar_orden_de_salida(ordenes: UnboundedSender<bandeja::Orden>) {
    let nombre: Vec<u16> = ORDEN_DE_SALIDA.encode_utf16().chain(Some(0)).collect();
    let evento = unsafe { CreateEventW(std::ptr::null(), 1, 0, nombre.as_ptr()) } as usize;
    if evento == 0 || unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }
    std::thread::spawn(move || unsafe {
        WaitForSingleObject(evento as _, INFINITE);
        CloseHandle(evento as _);
        let _ = ordenes.send(bandeja::Orden::Salir);
    });
}

pub async fn correr(origen: String) -> Result<(), Box<dyn std::error::Error>> {
    // Si ya hay uno en marcha, la página habla con él.
    let Ok(escucha) = TcpListener::bind(("127.0.0.1", PORT)).await else {
        return Ok(());
    };
    let ruta = ajustes::ruta().ok_or("No se encontró LOCALAPPDATA")?;
    let ruta_medidas = cambio::ruta_de_medidas(&ruta);
    let medidas = cambio::leer(&ruta_medidas);
    let ajustes = Ajustes::leer(&ruta);
    let (eventos, mut eventos_rx) = unbounded_channel();
    let (entrada, mut entrada_rx): (_, UnboundedReceiver<Entrada>) = unbounded_channel();
    let (estado, estado_rx) = watch::channel(String::new());
    let paginas = Arc::new(AtomicUsize::new(0));
    let minando = Arc::new(AtomicBool::new(false));

    let (bandeja_tx, mut bandeja_rx) = unbounded_channel();
    esperar_orden_de_salida(bandeja_tx.clone());
    let principal = origen.split(',').next().unwrap_or(&origen);
    bandeja::abrir(bandeja_tx, &format!("{principal}/dashboard"));
    {
        let entrada = entrada.clone();
        tokio::spawn(async move {
            while let Some(orden) = bandeja_rx.recv().await {
                let _ = entrada.send(Entrada::Bandeja(orden));
            }
        });
    }
    {
        // Sólo se pregunta mientras hay una página mirando o se mina: así se mide el consumo real.
        let (entrada, paginas, minando) = (entrada.clone(), paginas.clone(), minando.clone());
        tokio::spawn(async move {
            let mut lectura = tokio::time::interval(LECTURA_GPU);
            loop {
                lectura.tick().await;
                if paginas.load(Ordering::SeqCst) > 0 || minando.load(Ordering::SeqCst) {
                    let _ = entrada.send(Entrada::Gpus(leer_gpus().await));
                }
            }
        });
    }
    {
        let entrada = entrada.clone();
        tokio::spawn(async move {
            let mut lectura = tokio::time::interval(LECTURA_MERCADO);
            loop {
                lectura.tick().await;
                if let Ok(mercado) = economia::leer(&ajustes::fecha_de_hoy()).await {
                    let _ = entrada.send(Entrada::Mercado(mercado));
                }
            }
        });
    }
    {
        let (paginas, origen): (_, Arc<str>) = (paginas.clone(), origen.as_str().into());
        tokio::spawn(async move {
            while let Ok((stream, _)) = escucha.accept().await {
                tokio::spawn(cliente(
                    stream,
                    origen.clone(),
                    entrada.clone(),
                    estado_rx.clone(),
                    paginas.clone(),
                ));
            }
        });
    }

    let limite = Arc::new(AtomicU32::new(ajustes.gpu_limit.unwrap_or(SIN_LIMITE)));
    let mut r = Residente {
        ajustes,
        ruta,
        plan: Plan::default(),
        sesion: None,
        eventos,
        fase: "idle",
        rate: 0.0,
        accepted: 0,
        rejected: 0,
        problema: None,
        reintento: None,
        estado,
        gpus: Vec::new(),
        mercado: None,
        medidas,
        ruta_medidas,
        selector: Selector::default(),
        moneda: None,
        minando,
        limite,
        dispositivos: Vec::new(),
    };
    r.repasar();
    let mut repaso = tokio::time::interval(REPASO);
    let mut parado_desde = Instant::now();
    loop {
        tokio::select! {
            Some(e) = entrada_rx.recv() => match e {
                Entrada::Web(cuerpo) => if r.web(&cuerpo) { break },
                Entrada::Bandeja(bandeja::Orden::Empezar) => { r.problema = None; r.plan.ordenar(true); r.repasar(); }
                Entrada::Bandeja(bandeja::Orden::Parar) => { r.plan.ordenar(false); r.repasar(); }
                Entrada::Bandeja(bandeja::Orden::Salir) => break,
                Entrada::Gpus(gpus) => {
                    // Todas las GPU minan: el consumo es el de todas.
                    let vatios: Option<f64> = gpus.iter().map(|g| g.power_w).sum();
                    if let Some(vatios) = vatios {
                        r.medir(vatios);
                    }
                    r.gpus = gpus;
                    r.publicar();
                }
                Entrada::Mercado(mercado) => { r.mercado = Some(mercado); r.repasar(); }
            },
            Some(cuerpo) = eventos_rx.recv() => r.evento(&cuerpo),
            _ = repaso.tick() => {
                r.repasar();
                if !r.puede_cerrarse(paginas.load(Ordering::SeqCst)) {
                    parado_desde = Instant::now();
                } else if parado_desde.elapsed() >= ESPERA_CIERRE {
                    break;
                }
            }
        }
    }
    r.parar();
    bandeja::quitar();
    Ok(())
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn el_techo_sale_con_multiprocesadores_y_reloj_y_sólo_en_pearl() {
        // 24 SM a 2270 MHz: unos 55,8 TMAC/s.
        let t = techo(Some(Moneda::Pearl), Some(24), Some(2270.0)).unwrap();
        assert!((t - 55.8e12).abs() < 0.1e12, "{t}");
        assert_eq!(techo(Some(Moneda::Quantus), Some(24), Some(2270.0)), None);
        assert_eq!(techo(None, Some(24), Some(2270.0)), None);
        assert_eq!(techo(Some(Moneda::Pearl), None, Some(2270.0)), None);
        assert_eq!(techo(Some(Moneda::Pearl), Some(24), None), None);
        assert_eq!(techo(Some(Moneda::Pearl), Some(24), Some(0.0)), None);
    }

    #[test]
    fn lee_cada_grafica_y_tolera_datos_que_faltan() {
        let salida = "NVIDIA GeForce RTX 5070 Laptop GPU, 99, 5088, 8151, 75, 114.22, 2370\r\nNVIDIA GeForce RTX 3060, [N/A], 100, 12288, 40, [N/A], [N/A]\r\n\r\nrota\r\n";
        let gpus = leer_tabla(salida);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 5070 Laptop GPU");
        assert_eq!(
            (
                gpus[0].use_percent,
                gpus[0].memory_total_mb,
                gpus[0].power_w
            ),
            (Some(99.0), Some(8151.0), Some(114.22))
        );
        assert_eq!((gpus[0].clock_mhz, gpus[1].clock_mhz), (Some(2370.0), None));
        assert_eq!((gpus[1].use_percent, gpus[1].power_w), (None, None));
        assert_eq!(leer_tabla(""), Vec::<Gpu>::new());
    }
}
