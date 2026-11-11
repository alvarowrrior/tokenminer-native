//! Conexión con los pools de Kryptex de Pearl y de Quantus. Las direcciones están fijadas aquí: la web sólo elige a
//! nombre de quién se mina.
//!
//! Kryptex habla una línea JSON por mensaje y el nombre del trabajador va en la cuenta, `cuenta.nombre`. En Pearl,
//! `mining.authorize` lleva un objeto, no hay `mining.subscribe` y la dificultad llega como objetivo de cada trabajo.
//! En Quantus, `login` devuelve la sesión y el primer trabajo, los siguientes llegan con `job` y cada share se
//! entrega con `submit`, el nonce y su hash.
//!
//! Quien cobra directamente paga la comisión con turnos de minado para la cuenta de TokenMiner: dos conexiones
//! abiertas, y el motor trabaja para una u otra. Las esperas no tienen memoria: reiniciar el programa no las salta.
use crate::ajustes::Moneda;
use crate::send;
use rand::Rng;
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use tokenminer_pearl::trabajador::{Encargo, Orden};
use tokenminer_pearl::{share, HEADER_BYTES};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{sleep, timeout, Instant};

const AGENTE: &str = concat!("TokenMiner/", env!("CARGO_PKG_VERSION"));
const ESPERA_CONEXION: Duration = Duration::from_secs(15);
// Llega un trabajo cada 35 s de media: tanto silencio es una conexión muerta.
const SILENCIO_MAXIMO: Duration = Duration::from_secs(180);
const REINTENTO_MAXIMO: Duration = Duration::from_secs(30);
/// Conexiones seguidas que pueden acabar por un mensaje del pool que no se entiende antes de darlo por definitivo.
const RAROS_SEGUIDOS: u32 = 5;
/// Trabajos seguidos de Pearl con otra versión de certificado antes de darlo por definitivo: uno suelto puede ser un
/// cambio que el pool deshace; tres seguidos son la red, que ha cambiado.
const OTRA_VERSION_SEGUIDA: u32 = 3;

/// Cuenta los trabajos seguidos de Pearl con una versión de certificado que el motor no mina, y da el aviso cuando
/// llegan a `OTRA_VERSION_SEGUIDA`. El motor se salta esos trabajos sin decir nada: sin esto el programa seguía
/// «minando» Pearl a velocidad cero, y en automático no pasaba a la otra moneda.
fn otra_version(seguidos: &mut u32, version: Option<u64>) -> Option<String> {
    let version = version?;
    if version == tokenminer_pearl::CERT_VERSION {
        *seguidos = 0;
        return None;
    }
    *seguidos += 1;
    (*seguidos >= OTRA_VERSION_SEGUIDA).then(|| {
        format!(
            "Pearl ha cambiado de versión de certificado (ahora la {version}; este programa mina la {}). \
             Hace falta actualizar TokenMiner para seguir minando Pearl.",
            tokenminer_pearl::CERT_VERSION
        )
    })
}

/// Porcentaje del tiempo de minado que va a TokenMiner cuando el usuario cobra directamente, y su cuenta, que debe
/// ser la misma con la que la web mina tokens. Se fijan al compilar con `build.ps1 -Comision -CuentaComision`.
pub const COMISION: u32 = match option_env!("TOKENMINER_COMISION") {
    Some(texto) => porcentaje(texto),
    None => 10,
};
const CUENTA_COMISION: &str = match option_env!("TOKENMINER_CUENTA_COMISION") {
    Some(cuenta) => cuenta,
    None => "krxY96J9JD",
};

const fn porcentaje(texto: &str) -> u32 {
    let (cifras, mut i, mut n) = (texto.as_bytes(), 0, 0u32);
    assert!(
        !cifras.is_empty() && cifras.len() <= 2,
        "TOKENMINER_COMISION va de 0 a 50"
    );
    while i < cifras.len() {
        assert!(
            cifras[i].is_ascii_digit(),
            "TOKENMINER_COMISION va de 0 a 50"
        );
        n = n * 10 + (cifras[i] - b'0') as u32;
        i += 1;
    }
    assert!(n <= 50, "TOKENMINER_COMISION va de 0 a 50");
    n
}
const TURNO_COMISION: Duration = Duration::from_secs(30);

pub const USUARIO: usize = 0;
pub const TOKENMINER: usize = 1;

/// Una share para el pool: su trabajo y lo que cada moneda manda con él.
pub struct Share {
    pub job_id: String,
    pub datos: Value,
}

/// Lo que la mesa necesita del motor de cada moneda.
pub trait Motor: Send + Sync {
    /// Comprueba un trabajo del pool sin mandarlo al motor.
    fn validar(&self, trabajo: &Value) -> Result<(), String>;
    fn trabajar(&self, trabajo: &Value, revision: u64) -> Result<(), String>;
    fn pausar(&self);
}

/// El motor de Pearl, con la revisión vigente que comparte: las shares de un trabajo anterior ya no salen.
pub struct MotorPearl {
    ordenes: Mutex<mpsc::Sender<Orden>>,
    vigente: Arc<AtomicU64>,
}

impl Motor for MotorPearl {
    fn validar(&self, trabajo: &Value) -> Result<(), String> {
        encargo(trabajo, 0).map(|_| ())
    }

    fn trabajar(&self, trabajo: &Value, revision: u64) -> Result<(), String> {
        let e = encargo(trabajo, revision)?;
        // Antes de que el motor acabe su lote: las shares del trabajo anterior ya no salen.
        self.vigente.store(e.revision, Ordering::SeqCst);
        let _ = self.ordenes.lock().unwrap().send(Orden::Trabajo(e));
        Ok(())
    }

    fn pausar(&self) {
        self.vigente.store(0, Ordering::SeqCst);
        let _ = self.ordenes.lock().unwrap().send(Orden::Pausa);
    }
}

/// Lo que comparten las dos conexiones: para quién trabaja el motor y el último trabajo de cada una.
pub struct Mesa {
    motor: Box<dyn Motor>,
    revision: AtomicU64,
    activa: AtomicUsize,
    ultimos: Mutex<[Option<Value>; 2]>,
    duenos: Mutex<HashMap<String, usize>>,
}

impl Mesa {
    pub fn nueva(motor: impl Motor + 'static) -> Self {
        Self {
            motor: Box::new(motor),
            revision: AtomicU64::new(0),
            activa: AtomicUsize::new(USUARIO),
            ultimos: Mutex::new([None, None]),
            duenos: Mutex::new(HashMap::new()),
        }
    }

    pub fn pearl(ordenes: mpsc::Sender<Orden>, vigente: Arc<AtomicU64>) -> Self {
        Self::nueva(MotorPearl {
            ordenes: Mutex::new(ordenes),
            vigente,
        })
    }

    /// De qué conexión salió un trabajo: la share debe volver a ella.
    pub fn dueno(&self, job_id: &str) -> Option<usize> {
        self.duenos.lock().unwrap().get(job_id).copied()
    }

    fn lanzar(&self, conexion: usize) -> Result<(), String> {
        let Some(trabajo) = self.ultimos.lock().unwrap()[conexion].clone() else {
            return Ok(());
        };
        self.motor
            .trabajar(&trabajo, self.revision.fetch_add(1, Ordering::SeqCst) + 1)
    }

    fn recibir(&self, conexion: usize, trabajo: Value) -> Result<(), String> {
        self.motor.validar(&trabajo)?;
        let id = trabajo["job_id"].as_str().unwrap_or_default().to_string();
        let mut duenos = self.duenos.lock().unwrap();
        // Un trabajo caduca en minutos: basta con no dejar crecer la tabla.
        if duenos.len() > 256 {
            duenos.clear();
        }
        duenos.insert(id, conexion);
        drop(duenos);
        self.ultimos.lock().unwrap()[conexion] = Some(trabajo);
        if self.activa.load(Ordering::SeqCst) == conexion {
            self.lanzar(conexion)
        } else {
            Ok(())
        }
    }

    fn hay_trabajo(&self, conexion: usize) -> bool {
        self.ultimos.lock().unwrap()[conexion].is_some()
    }

    fn activar(&self, conexion: usize) -> Result<(), String> {
        self.activa.store(conexion, Ordering::SeqCst);
        self.lanzar(conexion)
    }

    fn perder(&self, conexion: usize) -> Result<(), String> {
        self.ultimos.lock().unwrap()[conexion] = None;
        if self.activa.load(Ordering::SeqCst) != conexion {
            return Ok(());
        }
        if conexion == TOKENMINER {
            return self.activar(USUARIO);
        }
        self.motor.pausar();
        Ok(())
    }
}

pub struct Mina {
    pub moneda: Moneda,
    pub wallet: String,
    pub conexion: usize,
    pub mesa: Arc<Mesa>,
    pub eventos: UnboundedSender<String>,
}

impl Mina {
    /// La conexión de la comisión no se enseña al usuario: si falla, sigue minando para él.
    fn avisar(&self, mensaje: Value) {
        if self.conexion == USUARIO {
            send(&self.eventos, mensaje);
        }
    }
}

enum Fin {
    /// Se reintenta: red, pool caído o silencio.
    Cortada(String),
    /// El pool mandó algo que no se entiende. Una vez puede ser un fallo suyo y se reintenta; si se repite en
    /// cada conexión, reintentar no lo arregla.
    Raro(String),
    /// Reintentar no lo arregla.
    Definitivo(String),
}

impl From<std::io::Error> for Fin {
    fn from(e: std::io::Error) -> Self {
        Self::Cortada(e.to_string())
    }
}

/// Sólo lo que un nombre de Kryptex o una dirección Pearl pueden llevar. Sin `:`, que activaría otros modos de pago.
pub fn wallet_valida(wallet: &str) -> bool {
    !wallet.is_empty()
        && wallet.len() <= 128
        && wallet
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A qué nombre se paga la comisión. Quien mina tokens ya mina para TokenMiner y no paga aparte.
pub fn wallet_comision(wallet: &str) -> Option<String> {
    let cuenta = wallet.split('.').next().unwrap_or_default();
    if COMISION == 0 || cuenta.eq_ignore_ascii_case(CUENTA_COMISION) {
        return None;
    }
    let mut h = DefaultHasher::new();
    wallet.hash(&mut h);
    Some(format!("{CUENTA_COMISION}.tf{:016x}", h.finish()))
}

fn encargo(v: &Value, revision: u64) -> Result<Encargo, String> {
    let job_id = v["job_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or("ID de trabajo incorrecto")?;
    let cabecera: [u8; HEADER_BYTES] = v["header"]
        .as_str()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or("La cabecera debe tener 76 bytes")?;
    if !share::cabecera_valida(&cabecera) {
        return Err("Cabecera incorrecta".into());
    }
    let objetivo: [u8; 32] = v["target"]
        .as_str()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or("El objetivo debe tener 32 bytes")?;
    Ok(Encargo {
        revision,
        job_id: job_id.to_string(),
        cabecera,
        objetivo,
        cert_version: v["cert_version"].as_u64().ok_or("Falta cert_version")?,
    })
}

async fn escribir<S: AsyncWrite + Unpin>(salida: &mut S, mensaje: Value) -> std::io::Result<()> {
    salida.write_all(format!("{mensaje}\n").as_bytes()).await
}

/// Una conexión de Pearl, desde el login hasta que se corte.
async fn sesion_pearl<S: AsyncRead + AsyncWrite>(
    conexion: S,
    mina: &Mina,
    shares: &mut UnboundedReceiver<Share>,
) -> Result<(), Fin> {
    let (lectura, mut salida) = tokio::io::split(conexion);
    let mut lineas = BufReader::new(lectura).lines();
    escribir(
        &mut salida,
        json!({"id": 1, "method": "mining.authorize",
        "params": {"wallet": mina.wallet, "pass": "x", "agent": AGENTE}}),
    )
    .await?;
    let mut id = 1u64;
    let mut dentro = false;
    let mut de_otra_version = 0;
    let silencio = sleep(ESPERA_CONEXION);
    tokio::pin!(silencio);
    loop {
        tokio::select! {
            linea = lineas.next_line() => {
                let linea = linea?.ok_or(Fin::Cortada("El pool cerró la conexión".into()))?;
                if linea.trim().is_empty() {
                    continue;
                }
                let v: Value = serde_json::from_str(&linea).map_err(|e| Fin::Raro(format!("Mensaje del pool: {e}")))?;
                silencio.as_mut().reset(Instant::now() + SILENCIO_MAXIMO);
                if v["method"] == "mining.notify" {
                    if let Some(aviso) = otra_version(&mut de_otra_version, v["params"]["cert_version"].as_u64()) {
                        return Err(Fin::Definitivo(aviso));
                    }
                    mina.mesa.recibir(mina.conexion, v["params"].clone()).map_err(Fin::Raro)?;
                    if !dentro {
                        dentro = true;
                        mina.avisar(json!({"type": "pool", "state": "connected"}));
                    }
                } else if v["id"] == 1 {
                    if v["result"] != true {
                        return Err(Fin::Definitivo(format!("El pool no acepta esta cuenta: {}", v["error"])));
                    }
                } else if v["id"].is_u64() {
                    mina.avisar(json!({"type": "share", "accepted": v["result"] == true, "error": v["error"]}));
                }
            }
            Some(s) = shares.recv() => {
                id += 1;
                escribir(&mut salida, json!({"id": id, "method": "mining.submit",
                    "params": {"job_id": s.job_id, "plain_proof": s.datos["plain_proof"]}})).await?;
            }
            _ = &mut silencio => return Err(Fin::Cortada("El pool no responde".into())),
        }
    }
}

/// Una conexión de Quantus, desde el login hasta que se corte.
async fn sesion_quantus<S: AsyncRead + AsyncWrite>(
    conexion: S,
    mina: &Mina,
    shares: &mut UnboundedReceiver<Share>,
) -> Result<(), Fin> {
    let (lectura, mut salida) = tokio::io::split(conexion);
    let mut lineas = BufReader::new(lectura).lines();
    escribir(
        &mut salida,
        json!({"id": 1, "jsonrpc": "2.0", "method": "login",
        "params": {"login": mina.wallet, "pass": "x", "agent": AGENTE}}),
    )
    .await?;
    let mut id = 1u64;
    let mut sesion: Option<String> = None;
    let silencio = sleep(ESPERA_CONEXION);
    tokio::pin!(silencio);
    loop {
        tokio::select! {
            linea = lineas.next_line() => {
                let linea = linea?.ok_or(Fin::Cortada("El pool cerró la conexión".into()))?;
                if linea.trim().is_empty() {
                    continue;
                }
                let v: Value = serde_json::from_str(&linea).map_err(|e| Fin::Raro(format!("Mensaje del pool: {e}")))?;
                silencio.as_mut().reset(Instant::now() + SILENCIO_MAXIMO);
                if v["method"] == "job" {
                    mina.mesa.recibir(mina.conexion, v["params"]["job"].clone()).map_err(Fin::Raro)?;
                } else if v["id"] == 1 && sesion.is_none() {
                    let Some(nombre) = v["result"]["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 256) else {
                        return Err(Fin::Definitivo(format!("El pool no acepta esta cuenta: {}", v["error"])));
                    };
                    sesion = Some(nombre.to_string());
                    mina.mesa.recibir(mina.conexion, v["result"]["job"].clone()).map_err(Fin::Raro)?;
                    mina.avisar(json!({"type": "pool", "state": "connected"}));
                } else if v["id"].is_u64() {
                    let aceptada = v["error"].is_null() && v["result"]["status"] == "OK";
                    mina.avisar(json!({"type": "share", "accepted": aceptada, "error": v["error"]}));
                }
            }
            Some(s) = shares.recv(), if sesion.is_some() => {
                id += 1;
                escribir(&mut salida, json!({"id": id, "jsonrpc": "2.0", "method": "submit",
                    "params": {"id": sesion, "job_id": s.job_id, "nonce": s.datos["nonce"], "result": s.datos["result"]}})).await?;
            }
            _ = &mut silencio => return Err(Fin::Cortada("El pool no responde".into())),
        }
    }
}

fn servidor(moneda: Moneda) -> (&'static str, u16) {
    match moneda {
        Moneda::Pearl => ("prl.kryptex.network", 8048),
        Moneda::Quantus => ("qtc-eu.kryptex.network", 8049),
    }
}

async fn conectar(moneda: Moneda) -> Result<tokio_native_tls::TlsStream<TcpStream>, String> {
    let (host, puerto) = servidor(moneda);
    let tcp = TcpStream::connect((host, puerto))
        .await
        .map_err(|e| e.to_string())?;
    tcp.set_nodelay(true).map_err(|e| e.to_string())?;
    let tls = tokio_native_tls::native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
    tokio_native_tls::TlsConnector::from(tls)
        .connect(host, tcp)
        .await
        .map_err(|e| e.to_string())
}

async fn sesion<S: AsyncRead + AsyncWrite>(
    conexion: S,
    mina: &Mina,
    shares: &mut UnboundedReceiver<Share>,
) -> Result<(), Fin> {
    match mina.moneda {
        Moneda::Pearl => sesion_pearl(conexion, mina, shares).await,
        Moneda::Quantus => sesion_quantus(conexion, mina, shares).await,
    }
}

/// Qué hacer cuando acaba una conexión: reconectar (con el motivo, si lo hay) o dejarlo con un error. `raros`
/// cuenta las conexiones seguidas que acabaron por un mensaje que no se entiende; una conexión `larga` lo pone a cero.
fn seguir(fin: Result<(), Fin>, raros: &mut u32, larga: bool) -> Result<Option<String>, String> {
    if larga {
        *raros = 0;
    }
    match fin {
        Ok(()) => Ok(None),
        Err(Fin::Cortada(e)) => Ok(Some(e)),
        Err(Fin::Definitivo(e)) => Err(e),
        Err(Fin::Raro(e)) => {
            *raros += 1;
            if *raros >= RAROS_SEGUIDOS {
                Err(e)
            } else {
                Ok(Some(e))
            }
        }
    }
}

/// Mantiene una conexión hasta que aborten la tarea, reconectando si hace falta. Sin la del usuario el motor para:
/// sus shares no llegarían. Sin la de la comisión sigue minando para el usuario.
pub async fn minar(mina: Mina, mut shares: UnboundedReceiver<Share>) {
    let mut espera = Duration::from_secs(1);
    let mut raros = 0;
    loop {
        mina.avisar(json!({"type": "pool", "state": "connecting"}));
        let inicio = Instant::now();
        let fin = match timeout(ESPERA_CONEXION, conectar(mina.moneda)).await {
            Ok(Ok(conexion)) => sesion(conexion, &mina, &mut shares).await,
            Ok(Err(e)) => Err(Fin::Cortada(e)),
            Err(_) => Err(Fin::Cortada("El pool no responde".into())),
        };
        let _ = mina.mesa.perder(mina.conexion);
        while shares.try_recv().is_ok() {}
        let larga = inicio.elapsed() > REINTENTO_MAXIMO;
        match seguir(fin, &mut raros, larga) {
            Err(e) => {
                mina.avisar(json!({"type": "error", "message": e}));
                return;
            }
            Ok(Some(e)) => {
                mina.avisar(json!({"type": "pool", "state": "reconnecting", "reason": e}))
            }
            Ok(None) => {}
        }
        if larga {
            espera = Duration::from_secs(1);
        }
        sleep(espera).await;
        espera = (espera * 2).min(REINTENTO_MAXIMO);
    }
}

/// Espera antes del siguiente turno. Exponencial: lo que falte no depende de lo que ya se ha esperado.
fn espera_comision(azar: f64) -> Duration {
    let media = TURNO_COMISION.as_secs_f64() * (100.0 - COMISION as f64) / COMISION as f64;
    Duration::from_secs_f64(-media * (1.0 - azar).ln())
}

/// Reparte el tiempo. Sólo cobra turno si las dos conexiones tienen trabajo.
pub async fn turnos(mesa: Arc<Mesa>) {
    loop {
        let azar = rand::rng().random::<f64>();
        sleep(espera_comision(azar)).await;
        if mesa.hay_trabajo(USUARIO) && mesa.hay_trabajo(TOKENMINER) {
            let _ = mesa.activar(TOKENMINER);
            sleep(TURNO_COMISION).await;
            if mesa.activa.load(Ordering::SeqCst) == TOKENMINER {
                let _ = mesa.activar(USUARIO);
            }
        }
    }
}

#[cfg(test)]
mod pruebas {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn parametros(job_id: &str) -> Value {
        json!({"job_id": job_id, "header": "00".repeat(76), "target": "ff".repeat(32), "cert_version": 3, "height": 9})
    }

    #[test]
    fn trabajo_del_pool_valida_cada_campo() {
        let bueno = parametros("j");
        assert_eq!(encargo(&bueno, 3).unwrap().revision, 3);
        for campo in ["job_id", "header", "target", "cert_version"] {
            let mut v = bueno.clone();
            v.as_object_mut().unwrap().remove(campo);
            assert!(encargo(&v, 3).is_err(), "{campo}");
        }
        for (campo, valor) in [
            ("job_id", json!("")),
            ("header", json!("00")),
            ("target", json!("ff")),
        ] {
            let mut v = bueno.clone();
            v[campo] = valor;
            assert!(encargo(&v, 3).is_err(), "{campo}");
        }
    }

    #[test]
    fn la_web_solo_elige_un_nombre() {
        for bueno in [
            "krxY96J9JD.tm0123456789abcdef",
            "prl1direcciondeejemploparapruebas000000000000000000000000000000.minero",
        ] {
            assert!(wallet_valida(bueno), "{bueno}");
        }
        let largo = "a".repeat(129);
        for malo in ["", "solo:krxY96J9JD", "a b", "a\"b", "a\nb", largo.as_str()] {
            assert!(!wallet_valida(malo), "{malo}");
        }
    }

    #[test]
    fn solo_paga_comision_quien_cobra_directamente() {
        assert_eq!(wallet_comision(&format!("{CUENTA_COMISION}.tm0123")), None);
        assert_eq!(
            wallet_comision(&format!("{}.tm0123", CUENTA_COMISION.to_uppercase())),
            None
        );
        if COMISION == 0 {
            assert_eq!(wallet_comision("krxOtra.tm0123"), None);
            return;
        }
        let a = wallet_comision("krxOtra.tm0123").unwrap();
        assert!(
            a.starts_with(&format!("{CUENTA_COMISION}.tf")) && wallet_valida(&a),
            "{a}"
        );
        assert_ne!(a, wallet_comision("krxOtra.tm0124").unwrap());
        assert_eq!(a, wallet_comision("krxOtra.tm0123").unwrap());
    }

    #[test]
    fn los_turnos_suman_la_comision_anunciada() {
        // Sin comisión no hay turnos.
        if COMISION == 0 {
            return;
        }
        let n = 2_000_000;
        let suma: f64 = (0..n)
            .map(|i| espera_comision((i as f64 + 0.5) / n as f64).as_secs_f64())
            .sum();
        let media = suma / n as f64;
        let turno = TURNO_COMISION.as_secs_f64();
        let parte = turno / (turno + media) * 100.0;
        assert!((parte - COMISION as f64).abs() < 0.01, "{parte}");
    }

    struct Banco {
        pool: BufReader<tokio::io::DuplexStream>,
        ordenes: mpsc::Receiver<Orden>,
        vigente: Arc<AtomicU64>,
        eventos: UnboundedReceiver<String>,
        shares: UnboundedSender<Share>,
        mesa: Arc<Mesa>,
        tarea: tokio::task::JoinHandle<Result<(), Fin>>,
    }

    fn abrir(conexion: usize) -> Banco {
        let (programa, pool) = tokio::io::duplex(1 << 16);
        let (ord_tx, ordenes) = mpsc::channel();
        let (ev_tx, eventos) = unbounded_channel();
        let (shares, mut sh_rx) = unbounded_channel();
        let vigente = Arc::new(AtomicU64::new(0));
        let mesa = Arc::new(Mesa::pearl(ord_tx, vigente.clone()));
        let mina = Mina {
            moneda: Moneda::Pearl,
            wallet: "cuenta.nombre".into(),
            conexion,
            mesa: mesa.clone(),
            eventos: ev_tx,
        };
        let tarea = tokio::spawn(async move { sesion(programa, &mina, &mut sh_rx).await });
        Banco {
            pool: BufReader::new(pool),
            ordenes,
            vigente,
            eventos,
            shares,
            mesa,
            tarea,
        }
    }

    async fn leer(banco: &mut Banco) -> Value {
        let mut linea = String::new();
        banco.pool.read_line(&mut linea).await.unwrap();
        serde_json::from_str(&linea).unwrap()
    }

    async fn enviar(banco: &mut Banco, mensaje: Value) {
        banco
            .pool
            .get_mut()
            .write_all(format!("{mensaje}\n").as_bytes())
            .await
            .unwrap();
    }

    fn trabajo(b: &Banco) -> Encargo {
        let Orden::Trabajo(e) = b.ordenes.recv_timeout(Duration::from_secs(2)).unwrap() else {
            panic!("sin trabajo")
        };
        e
    }

    #[tokio::test]
    async fn entra_recibe_trabajo_y_entrega_shares() {
        let mut b = abrir(USUARIO);
        let login = leer(&mut b).await;
        assert_eq!(login["method"], "mining.authorize");
        assert_eq!(login["params"]["wallet"], "cuenta.nombre");
        enviar(&mut b, json!({"id": 1, "result": true, "error": null})).await;
        enviar(
            &mut b,
            json!({"method": "mining.notify", "params": parametros("j_4194304")}),
        )
        .await;
        let mut evento = b.eventos.recv().await.unwrap();
        assert!(evento.contains("connected"), "{evento}");
        let e = trabajo(&b);
        assert_eq!((e.revision, e.job_id.as_str()), (1, "j_4194304"));
        assert_eq!(b.vigente.load(Ordering::SeqCst), 1);
        assert_eq!(b.mesa.dueno("j_4194304"), Some(USUARIO));

        b.shares
            .send(Share {
                job_id: "j_4194304".into(),
                datos: json!({"plain_proof": "cHJ1ZWJh"}),
            })
            .unwrap();
        let envio = leer(&mut b).await;
        assert_eq!(envio["method"], "mining.submit");
        assert_eq!(envio["params"]["job_id"], "j_4194304");
        assert_eq!(envio["params"]["plain_proof"], "cHJ1ZWJh");
        enviar(
            &mut b,
            json!({"id": envio["id"], "result": true, "error": null}),
        )
        .await;
        evento = b.eventos.recv().await.unwrap();
        assert!(evento.contains(r#""accepted":true"#), "{evento}");

        b.shares
            .send(Share {
                job_id: "viejo".into(),
                datos: json!({"plain_proof": "eA=="}),
            })
            .unwrap();
        let envio = leer(&mut b).await;
        enviar(
            &mut b,
            json!({"id": envio["id"], "result": false, "error": "stale"}),
        )
        .await;
        evento = b.eventos.recv().await.unwrap();
        assert!(
            evento.contains(r#""accepted":false"#) && evento.contains("stale"),
            "{evento}"
        );
        b.tarea.abort();
    }

    #[tokio::test]
    async fn los_eventos_de_la_comision_no_se_mezclan_con_los_del_usuario() {
        let mut b = abrir(TOKENMINER);
        leer(&mut b).await;
        enviar(&mut b, json!({"id": 1, "result": true, "error": null})).await;
        enviar(
            &mut b,
            json!({"method": "mining.notify", "params": parametros("c")}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(b.mesa.hay_trabajo(TOKENMINER));
        assert_eq!(b.mesa.dueno("c"), Some(TOKENMINER));
        assert!(b.ordenes.try_recv().is_err());
        assert!(b.eventos.try_recv().is_err());
        b.tarea.abort();
    }

    #[test]
    fn el_turno_cambia_de_trabajo_y_vuelve() {
        let (ord_tx, ordenes) = mpsc::channel();
        let mesa = Mesa::pearl(ord_tx, Arc::new(AtomicU64::new(0)));
        mesa.recibir(USUARIO, parametros("u")).unwrap();
        mesa.recibir(TOKENMINER, parametros("t")).unwrap();
        let siguiente = || {
            let Orden::Trabajo(e) = ordenes.try_recv().unwrap() else {
                panic!("sin trabajo")
            };
            (e.revision, e.job_id)
        };
        assert_eq!(siguiente(), (1, "u".to_string()));
        assert!(ordenes.try_recv().is_err());
        mesa.activar(TOKENMINER).unwrap();
        assert_eq!(siguiente(), (2, "t".to_string()));
        mesa.activar(USUARIO).unwrap();
        assert_eq!(siguiente(), (3, "u".to_string()));
    }

    #[test]
    fn sin_la_comision_sigue_para_el_usuario_y_sin_el_usuario_para() {
        let (ord_tx, ordenes) = mpsc::channel();
        let vigente = Arc::new(AtomicU64::new(0));
        let mesa = Mesa::pearl(ord_tx, vigente.clone());
        mesa.recibir(USUARIO, parametros("u")).unwrap();
        mesa.recibir(TOKENMINER, parametros("t")).unwrap();
        mesa.activar(TOKENMINER).unwrap();
        while ordenes.try_recv().is_ok() {}
        mesa.perder(TOKENMINER).unwrap();
        let Orden::Trabajo(e) = ordenes.try_recv().unwrap() else {
            panic!("sin trabajo")
        };
        assert_eq!(e.job_id, "u");
        mesa.perder(USUARIO).unwrap();
        assert!(matches!(ordenes.try_recv().unwrap(), Orden::Pausa));
        assert_eq!(vigente.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn login_rechazado_no_se_reintenta() {
        let mut b = abrir(USUARIO);
        leer(&mut b).await;
        enviar(&mut b, json!({"id": 1, "result": false, "error": "wallet"})).await;
        assert!(matches!(b.tarea.await.unwrap(), Err(Fin::Definitivo(_))));
    }

    #[tokio::test]
    async fn pool_que_cierra_se_reintenta() {
        let mut b = abrir(USUARIO);
        leer(&mut b).await;
        drop(b.pool);
        assert!(matches!(b.tarea.await.unwrap(), Err(Fin::Cortada(_))));
    }

    #[tokio::test]
    async fn trabajo_ilegible_no_se_mina() {
        let mut b = abrir(USUARIO);
        leer(&mut b).await;
        enviar(
            &mut b,
            json!({"method": "mining.notify", "params": {"job_id": "j"}}),
        )
        .await;
        assert!(matches!(b.tarea.await.unwrap(), Err(Fin::Raro(_))));
        assert!(b.ordenes.try_recv().is_err());
    }

    #[tokio::test]
    async fn una_linea_ilegible_corta_la_conexion_sin_darla_por_perdida() {
        let mut b = abrir(USUARIO);
        leer(&mut b).await;
        b.pool.get_mut().write_all(b"esto no es json
").await.unwrap();
        assert!(matches!(b.tarea.await.unwrap(), Err(Fin::Raro(_))));
    }

    #[tokio::test]
    async fn pearl_con_otra_version_de_certificado_se_dice_y_no_se_mina_en_vacio() {
        let mut b = abrir(USUARIO);
        leer(&mut b).await;
        enviar(&mut b, json!({"id": 1, "result": true, "error": null})).await;
        let mut nuevo = parametros("v4");
        nuevo["cert_version"] = json!(tokenminer_pearl::CERT_VERSION + 1);
        for _ in 0..OTRA_VERSION_SEGUIDA {
            enviar(&mut b, json!({"method": "mining.notify", "params": nuevo})).await;
        }
        let Err(Fin::Definitivo(aviso)) = b.tarea.await.unwrap() else {
            panic!("debía acabar con un error definitivo")
        };
        assert!(aviso.contains("certificado") && aviso.contains("actualizar"), "{aviso}");
    }

    #[test]
    fn un_trabajo_suelto_de_otra_version_no_para_nada() {
        let (esta, otra) = (tokenminer_pearl::CERT_VERSION, tokenminer_pearl::CERT_VERSION + 1);
        let mut seguidos = 0;
        assert_eq!(otra_version(&mut seguidos, Some(otra)), None);
        assert_eq!(otra_version(&mut seguidos, Some(otra)), None);
        // Un trabajo de la versión que se mina pone la cuenta a cero.
        assert_eq!(otra_version(&mut seguidos, Some(esta)), None);
        assert_eq!(otra_version(&mut seguidos, Some(otra)), None);
        assert_eq!(otra_version(&mut seguidos, Some(otra)), None);
        assert!(otra_version(&mut seguidos, Some(otra)).is_some());
        // Sin el campo no se cuenta: ese trabajo ya se rechaza por ilegible.
        assert_eq!(otra_version(&mut 2, None), None);
    }

    #[test]
    fn un_mensaje_raro_se_reintenta_y_solo_repetido_para() {
        let raro = || Err(Fin::Raro("raro".into()));
        let mut raros = 0;
        // Un mensaje que no se entiende reconecta; antes paraba el minado para siempre.
        for _ in 1..RAROS_SEGUIDOS {
            assert_eq!(seguir(raro(), &mut raros, false), Ok(Some("raro".into())));
        }
        assert_eq!(seguir(raro(), &mut raros, false), Err("raro".into()));
        // Una conexión que duró pone la cuenta a cero: fallos sueltos no se acumulan.
        let mut raros = RAROS_SEGUIDOS - 1;
        assert_eq!(seguir(raro(), &mut raros, true), Ok(Some("raro".into())));
        assert_eq!(raros, 1);
        // Un corte de red siempre se reintenta, y una cuenta rechazada nunca.
        assert_eq!(seguir(Err(Fin::Cortada("red".into())), &mut raros, false), Ok(Some("red".into())));
        assert_eq!(seguir(Err(Fin::Definitivo("cuenta".into())), &mut raros, false), Err("cuenta".into()));
        assert_eq!(seguir(Ok(()), &mut raros, false), Ok(None));
    }
}
