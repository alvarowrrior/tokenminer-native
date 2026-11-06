//! Quantus en el programa de fondo: un motor CUDA por GPU, sus conexiones con Kryptex y sus turnos, con los mismos
//! eventos que Pearl (`ready`, `pool`, `share`, `stats` con `devices`, `gpu` y `error`). Todas las GPU reciben los
//! mismos trabajos; cada una empieza en un nonce aleatorio, así que no repiten intentos, y sus shares salen por la
//! misma conexión.
use crate::ajustes::Moneda;
use crate::pool::{
    minar, turnos, wallet_comision, wallet_valida, Mesa, Mina, Motor, Share, TOKENMINER, USUARIO,
};
use crate::reparto::{vigilar, Mando};
use crate::{quantus_cuda_worker, send, Command, FrenoGpu, Work};
use primitive_types::U512;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use tokenminer_quantus::pow_core;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Un trabajo de Kryptex para el motor: `mining_hash` de 32 bytes, `extranonce` (el principio del nonce) y la
/// dificultad, que llega como número o como texto.
fn trabajo(v: &Value, revision: u64) -> Result<Work, String> {
    let job_id = v["job_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or("ID de trabajo incorrecto")?;
    let header: [u8; 32] = v["mining_hash"]
        .as_str()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or("La cabecera debe tener 32 bytes")?;
    let extranonce = hex::decode(v["extranonce"].as_str().unwrap_or_default())
        .map_err(|_| "Prefijo de nonce incorrecto")?;
    if extranonce.len() > 56 {
        return Err("Prefijo de nonce demasiado largo".into());
    }
    let texto = match &v["difficulty"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return Err("Falta la dificultad".into()),
    };
    let difficulty = U512::from_dec_str(&texto).map_err(|_| "Dificultad incorrecta")?;
    if difficulty.is_zero() {
        return Err("La dificultad debe ser positiva".into());
    }
    Ok(Work {
        revision,
        job_id: job_id.to_string(),
        header,
        difficulty,
        extranonce,
    })
}

/// La orden y el aviso de cancelación de cada motor, uno por GPU.
struct Motores {
    ordenes: Vec<mpsc::Sender<Command>>,
    cancelar: Vec<Arc<AtomicBool>>,
}

impl Motores {
    /// Cada motor deja de entregar lo que tenía a medias antes de recibir la orden.
    fn a_todos(&self, orden: impl Fn() -> Command) {
        for cancelar in &self.cancelar {
            cancelar.store(true, Ordering::Release);
        }
        for ordenes in &self.ordenes {
            let _ = ordenes.send(orden());
        }
    }
}

/// Guarda la cabecera de cada trabajo: la share lleva su hash, que se calcula con ella.
struct MotorQuantus {
    motores: Mutex<Motores>,
    cabeceras: Arc<Mutex<HashMap<String, [u8; 32]>>>,
}

impl Motor for MotorQuantus {
    fn validar(&self, trabajo_pool: &Value) -> Result<(), String> {
        trabajo(trabajo_pool, 0).map(|_| ())
    }

    fn trabajar(&self, trabajo_pool: &Value, revision: u64) -> Result<(), String> {
        let work = trabajo(trabajo_pool, revision)?;
        let mut cabeceras = self.cabeceras.lock().unwrap();
        // Un trabajo caduca en segundos: basta con no dejar crecer la tabla.
        if cabeceras.len() > 256 {
            cabeceras.clear();
        }
        cabeceras.insert(work.job_id.clone(), work.header);
        drop(cabeceras);
        self.motores
            .lock()
            .unwrap()
            .a_todos(|| Command::Work(work.clone()));
        Ok(())
    }

    fn pausar(&self) {
        self.motores.lock().unwrap().a_todos(|| Command::Pause);
    }
}

impl Mando for FrenoGpu {
    fn fijar(&self, limite: u32, apartado: bool) {
        self.limite.store(limite, Ordering::Relaxed);
        self.apartado.store(apartado, Ordering::Relaxed);
    }
}

/// Marca cada aviso de un motor con su GPU, y avisa cuando ese motor termina.
fn etiquetar(dispositivo: usize, mut avisos: UnboundedReceiver<String>, eventos: UnboundedSender<String>) {
    while let Some(texto) = avisos.blocking_recv() {
        if let Ok(mut v) = serde_json::from_str::<Value>(&texto) {
            v["device"] = json!(dispositivo);
            if eventos.send(v.to_string()).is_err() {
                return;
            }
        }
    }
    let _ = eventos.send(json!({"type": "fin", "device": dispositivo}).to_string());
}

/// Lo que se sabe de cada motor mientras dura la sesión.
struct Cuentas {
    listas: Vec<bool>,
    tasas: Vec<f64>,
    intentos: Vec<f64>,
    vivas: usize,
    /// Por qué no arrancó una GPU, para decirlo si no arranca ninguna.
    fallo: Option<String>,
    avisado: bool,
}

impl Cuentas {
    fn nuevas(cuantas: usize) -> Self {
        Cuentas {
            listas: vec![false; cuantas],
            tasas: vec![0.0; cuantas],
            intentos: vec![0.0; cuantas],
            vivas: cuantas,
            fallo: None,
            avisado: false,
        }
    }

    /// Qué mandar a la web por un aviso del motor de la GPU `i`. Una GPU que no arranca no para a las demás: sólo si no
    /// arranca ninguna se avisa del error. Devuelve también si la sesión se ha quedado sin motores.
    fn aviso(&mut self, i: usize, v: &Value, parando: bool) -> (Option<Value>, bool) {
        let i = i.min(self.listas.len().saturating_sub(1));
        match v["type"].as_str() {
            Some("ready") => {
                self.listas[i] = true;
                let primera = !self.listas.iter().enumerate().any(|(j, l)| *l && j != i);
                let ready = json!({"type": "ready", "mode": "cuda", "coin": "quantus",
                                   "devices": self.listas.len(), "gpu": v["gpu"], "variant": v["variant"]});
                (primera.then_some(ready), false)
            }
            Some("stats") => {
                self.tasas[i] = v["rate"].as_f64().unwrap_or(0.0);
                self.intentos[i] = v["attempts"].as_f64().unwrap_or(0.0);
                let total: f64 = self.tasas.iter().sum();
                let stats = json!({"type": "stats", "revision": v["revision"],
                                   "attempts": self.intentos.iter().sum::<f64>(),
                                   "rate": total, "devices": self.tasas});
                (Some(stats), false)
            }
            Some("error") if !self.listas[i] => {
                self.fallo = v["message"].as_str().map(str::to_string);
                (None, false)
            }
            Some("error") if !parando => {
                self.avisado = true;
                (Some(json!({"type": "error", "message": v["message"]})), false)
            }
            Some("fin") => {
                self.vivas = self.vivas.saturating_sub(1);
                self.tasas[i] = 0.0;
                let mensaje = if parando || self.avisado {
                    None
                } else if !self.listas.contains(&true) {
                    (self.vivas == 0).then(|| self.fallo.clone().unwrap_or("Se ha perdido la GPU".into()))
                } else if self.listas[i] {
                    Some("Se ha perdido una GPU".to_string())
                } else {
                    None
                };
                if mensaje.is_some() {
                    self.avisado = true;
                }
                (
                    mensaje.map(|m| json!({"type": "error", "message": m})),
                    self.vivas == 0,
                )
            }
            _ => (None, false),
        }
    }
}

/// Traduce los avisos de los motores: cada candidato va como share a la conexión que dio su trabajo, y la velocidad
/// de cada GPU va por separado en `devices`.
fn reenviar(
    mut eventos: UnboundedReceiver<String>,
    cuantas: usize,
    mesa: Arc<Mesa>,
    cabeceras: Arc<Mutex<HashMap<String, [u8; 32]>>>,
    shares: [UnboundedSender<Share>; 2],
    parando: Arc<AtomicBool>,
    tx: UnboundedSender<String>,
) {
    let mut cuentas = Cuentas::nuevas(cuantas);
    while let Some(texto) = eventos.blocking_recv() {
        let Ok(v) = serde_json::from_str::<Value>(&texto) else {
            continue;
        };
        if v["type"] == "candidate" {
            let (Some(job_id), Some(nonce_hex)) = (v["jobId"].as_str(), v["nonceHex"].as_str()) else {
                continue;
            };
            let Some(header) = cabeceras.lock().unwrap().get(job_id).copied() else {
                continue;
            };
            let Some(nonce) = hex::decode(nonce_hex)
                .ok()
                .and_then(|b| <[u8; 64]>::try_from(b).ok())
            else {
                continue;
            };
            let result = hex::encode(pow_core::get_nonce_hash(header, nonce).to_big_endian());
            if let Some(conexion) = mesa.dueno(job_id) {
                let _ = shares[conexion].send(Share {
                    job_id: job_id.to_string(),
                    datos: json!({"nonce": nonce_hex, "result": result}),
                });
            }
            continue;
        }
        let i = v["device"].as_u64().unwrap_or(0) as usize;
        let (mensaje, acabado) = cuentas.aviso(i, &v, parando.load(Ordering::SeqCst));
        if let Some(mensaje) = mensaje {
            send(&tx, mensaje);
        }
        if acabado {
            return;
        }
    }
}

pub struct Session {
    motores: Arc<MotorQuantus>,
    mesa: Arc<Mesa>,
    parando: Arc<AtomicBool>,
    tx: UnboundedSender<String>,
    hilos: Vec<JoinHandle<()>>,
    shares: Mutex<Option<[UnboundedReceiver<Share>; 2]>>,
    tareas: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Session {
    pub fn start(tx: UnboundedSender<String>, limite: Arc<AtomicU32>) -> Self {
        // Sin CUDA se intenta igual con una GPU: su motor dirá por qué no arranca.
        let luids = tokenminer_pearl::cuda::gpus().unwrap_or_default();
        let cuantas = luids.len().max(1);
        let parando = Arc::new(AtomicBool::new(false));
        let cabeceras = Arc::new(Mutex::new(HashMap::new()));
        let (eng_tx, eng_rx) = unbounded_channel();
        let (usuario_tx, usuario_rx) = unbounded_channel();
        let (tokenminer_tx, tokenminer_rx) = unbounded_channel();
        let mut hilos = Vec::new();
        let mut motores = Motores {
            ordenes: Vec::new(),
            cancelar: Vec::new(),
        };
        let mut frenos = Vec::new();
        for dispositivo in 0..cuantas {
            let (orden_tx, orden_rx) = mpsc::channel();
            let cancelar = Arc::new(AtomicBool::new(false));
            let freno = Arc::new(FrenoGpu::default());
            let (aviso_tx, aviso_rx) = unbounded_channel();
            {
                let (cancelar, freno) = (cancelar.clone(), freno.clone());
                hilos.push(std::thread::spawn(move || {
                    quantus_cuda_worker(dispositivo, freno, orden_rx, aviso_tx, cancelar)
                }));
            }
            {
                let eng_tx = eng_tx.clone();
                hilos.push(std::thread::spawn(move || {
                    etiquetar(dispositivo, aviso_rx, eng_tx)
                }));
            }
            motores.ordenes.push(orden_tx);
            motores.cancelar.push(cancelar);
            frenos.push(freno);
        }
        drop(eng_tx);
        let motor = Arc::new(MotorQuantus {
            motores: Mutex::new(motores),
            cabeceras: cabeceras.clone(),
        });
        let mesa = Arc::new(Mesa::nueva(MotorQuantusRef(motor.clone())));
        {
            let gpus = frenos
                .into_iter()
                .enumerate()
                .map(|(i, f)| (f as Arc<dyn Mando>, luids.get(i).cloned()))
                .collect();
            let (parando, tx) = (parando.clone(), tx.clone());
            hilos.push(std::thread::spawn(move || {
                vigilar(gpus, limite, parando, tx)
            }));
        }
        {
            let (mesa, parando, tx) = (mesa.clone(), parando.clone(), tx.clone());
            hilos.push(std::thread::spawn(move || {
                reenviar(
                    eng_rx,
                    cuantas,
                    mesa,
                    cabeceras,
                    [usuario_tx, tokenminer_tx],
                    parando,
                    tx,
                )
            }));
        }
        Self {
            motores: motor,
            mesa,
            parando,
            tx,
            hilos,
            shares: Mutex::new(Some([usuario_rx, tokenminer_rx])),
            tareas: Mutex::new(Vec::new()),
        }
    }

    /// Empieza a minar a nombre de `wallet`, que debe ser una cuenta de Kryptex: una dirección de Pearl no cobra
    /// Quantus.
    pub fn minar(&self, wallet: &str) -> Result<(), String> {
        if !wallet_valida(wallet) || !puede_minar(wallet) {
            return Err("Quantus necesita una cuenta de Kryptex".into());
        }
        let [usuario, tokenminer] = self
            .shares
            .lock()
            .unwrap()
            .take()
            .ok_or("Esta sesión ya está minando")?;
        let conectar = |wallet: String, conexion: usize, shares: UnboundedReceiver<Share>| {
            tokio::spawn(minar(
                Mina {
                    moneda: Moneda::Quantus,
                    wallet,
                    conexion,
                    mesa: self.mesa.clone(),
                    eventos: self.tx.clone(),
                },
                shares,
            ))
        };
        let mut tareas = self.tareas.lock().unwrap();
        tareas.push(conectar(wallet.to_string(), USUARIO, usuario));
        if let Some(comision) = wallet_comision(wallet) {
            tareas.push(conectar(comision, TOKENMINER, tokenminer));
            tareas.push(tokio::spawn(turnos(self.mesa.clone())));
        }
        Ok(())
    }

    pub fn finish(self) {
        for tarea in self.tareas.lock().unwrap().drain(..) {
            tarea.abort();
        }
        self.parando.store(true, Ordering::SeqCst);
        {
            let motores = self.motores.motores.lock().unwrap();
            motores.a_todos(|| Command::Stop);
        }
        // Sin órdenes, cada motor termina; la mesa se lleva otra referencia al motor.
        self.motores.motores.lock().unwrap().ordenes.clear();
        drop(self.mesa);
        for hilo in self.hilos {
            let _ = hilo.join();
        }
    }
}

/// La mesa necesita su propio dueño del motor; la sesión guarda otro para pararlo.
struct MotorQuantusRef(Arc<MotorQuantus>);

impl Motor for MotorQuantusRef {
    fn validar(&self, trabajo_pool: &Value) -> Result<(), String> {
        self.0.validar(trabajo_pool)
    }

    fn trabajar(&self, trabajo_pool: &Value, revision: u64) -> Result<(), String> {
        self.0.trabajar(trabajo_pool, revision)
    }

    fn pausar(&self) {
        self.0.pausar()
    }
}

/// Kryptex paga Quantus a una cuenta suya (`krx…`); una dirección de Pearl sólo cobra Pearl.
pub fn puede_minar(wallet: &str) -> bool {
    wallet
        .get(..3)
        .is_some_and(|inicio| inicio.eq_ignore_ascii_case("krx"))
}

#[cfg(test)]
mod pruebas {
    use super::*;

    #[test]
    fn lee_los_trabajos_de_kryptex() {
        let bueno = json!({"job_id": "q1", "mining_hash": "ab".repeat(32), "difficulty": 16777216u64,
                           "target": "00", "extranonce": "0a0b0c0d"});
        let w = trabajo(&bueno, 4).unwrap();
        assert_eq!(
            (w.revision, w.job_id.as_str(), w.extranonce.as_slice()),
            (4, "q1", &[10u8, 11, 12, 13][..])
        );
        assert_eq!(w.difficulty, U512::from(16777216u64));
        let mut texto = bueno.clone();
        texto["difficulty"] = json!("340282366920938463463374607431768211456");
        assert_eq!(trabajo(&texto, 1).unwrap().difficulty, U512::one() << 128);
        for (campo, valor) in [
            ("job_id", json!("")),
            ("mining_hash", json!("ab")),
            ("difficulty", json!(0)),
            ("difficulty", Value::Null),
            ("extranonce", json!("zz")),
        ] {
            let mut v = bueno.clone();
            v[campo] = valor;
            assert!(trabajo(&v, 1).is_err(), "{campo}");
        }
    }

    #[test]
    fn solo_una_cuenta_de_kryptex_mina_quantus() {
        assert!(puede_minar("krxY96J9JD.tm0123"));
        assert!(puede_minar("KRXabc"));
        assert!(!puede_minar(
            "prl1direcciondeejemploparapruebas000000000000000000000000000000"
        ));
        assert!(!puede_minar(""));
    }

    #[test]
    fn dos_gpu_suman_su_velocidad_y_una_que_no_arranca_no_para_a_la_otra() {
        let mut c = Cuentas::nuevas(2);
        // La primera que arranca anuncia la sesión con las dos GPU; la segunda ya no repite el aviso.
        let (ready, _) = c.aviso(0, &json!({"type": "ready", "gpu": "RTX 3070"}), false);
        assert_eq!(ready.unwrap()["devices"], 2);
        assert_eq!(c.aviso(1, &json!({"type": "ready"}), false).0, None);
        c.aviso(0, &json!({"type": "stats", "rate": 215e6, "attempts": 10.0}), false);
        let (stats, _) = c.aviso(1, &json!({"type": "stats", "rate": 180e6, "attempts": 5.0}), false);
        let stats = stats.unwrap();
        assert_eq!(stats["rate"], 395e6);
        assert_eq!(stats["devices"], json!([215e6, 180e6]));

        // Una GPU que falla al arrancar no corta la sesión mientras la otra mine.
        let mut c = Cuentas::nuevas(2);
        c.aviso(0, &json!({"type": "ready"}), false);
        assert_eq!(c.aviso(1, &json!({"type": "error", "message": "sin memoria"}), false).0, None);
        assert_eq!(c.aviso(1, &json!({"type": "fin"}), false), (None, false));
        // Si ninguna arranca, se dice por qué, y la sesión se acaba.
        let mut c = Cuentas::nuevas(2);
        c.aviso(0, &json!({"type": "error", "message": "sin memoria"}), false);
        assert_eq!(c.aviso(0, &json!({"type": "fin"}), false), (None, false));
        let (error, acabado) = c.aviso(1, &json!({"type": "fin"}), false);
        assert_eq!(error.unwrap()["message"], "sin memoria");
        assert!(acabado);
        // Al parar no se avisa de nada.
        let mut c = Cuentas::nuevas(1);
        c.aviso(0, &json!({"type": "ready"}), false);
        assert_eq!(c.aviso(0, &json!({"type": "fin"}), true), (None, true));
    }
}
