//! Quantus en el programa de fondo: el motor CUDA en la primera GPU, sus conexiones con Kryptex y sus turnos, con
//! los mismos eventos que Pearl (`ready`, `pool`, `share`, `stats` con `devices`, `gpu` y `error`).
use crate::ajustes::Moneda;
use crate::pool::{
    minar, turnos, wallet_comision, wallet_valida, Mesa, Mina, Motor, Share, TOKENMINER, USUARIO,
};
use crate::reparto::{vigilar, Mando};
use crate::{gpu_worker, send, Command, Mode, Work, APARTADO_QUANTUS, LIMITE_QUANTUS};
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

/// Guarda la cabecera de cada trabajo: la share lleva su hash, que se calcula con ella.
struct MotorQuantus {
    ordenes: Mutex<mpsc::Sender<Command>>,
    cancel: Arc<AtomicBool>,
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
        self.cancel.store(true, Ordering::Release);
        let _ = self.ordenes.lock().unwrap().send(Command::Work(work));
        Ok(())
    }

    fn pausar(&self) {
        self.cancel.store(true, Ordering::Release);
        let _ = self.ordenes.lock().unwrap().send(Command::Pause);
    }
}

struct FrenoQuantus;

impl Mando for FrenoQuantus {
    fn fijar(&self, limite: u32, apartado: bool) {
        LIMITE_QUANTUS.store(limite, Ordering::Relaxed);
        APARTADO_QUANTUS.store(apartado, Ordering::Relaxed);
    }
}

/// Traduce los avisos del motor: cada candidato va como share a la conexión que dio su trabajo.
fn reenviar(
    mut eventos: UnboundedReceiver<String>,
    mesa: Arc<Mesa>,
    cabeceras: Arc<Mutex<HashMap<String, [u8; 32]>>>,
    shares: [UnboundedSender<Share>; 2],
    parando: Arc<AtomicBool>,
    tx: UnboundedSender<String>,
) {
    let mut fallo = false;
    while let Some(texto) = eventos.blocking_recv() {
        let Ok(v) = serde_json::from_str::<Value>(&texto) else {
            continue;
        };
        match v["type"].as_str() {
            Some("ready") => send(
                &tx,
                json!({"type": "ready", "mode": "cuda", "coin": "quantus", "devices": 1,
                                              "gpu": v["gpu"], "variant": v["variant"]}),
            ),
            Some("candidate") => {
                let (Some(job_id), Some(nonce_hex)) = (v["jobId"].as_str(), v["nonceHex"].as_str())
                else {
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
            }
            Some("stats") => {
                let rate = v["rate"].as_f64().unwrap_or(0.0);
                send(
                    &tx,
                    json!({"type": "stats", "revision": v["revision"], "attempts": v["attempts"], "rate": rate,
                                 "devices": [rate]}),
                );
            }
            Some("error") if !parando.load(Ordering::SeqCst) => {
                fallo = true;
                send(&tx, v);
            }
            _ => {}
        }
    }
    if !fallo && !parando.load(Ordering::SeqCst) {
        send(
            &tx,
            json!({"type": "error", "message": "Se ha perdido la GPU"}),
        );
    }
}

pub struct Session {
    ordenes: mpsc::Sender<Command>,
    cancel: Arc<AtomicBool>,
    mesa: Arc<Mesa>,
    parando: Arc<AtomicBool>,
    tx: UnboundedSender<String>,
    hilos: Vec<JoinHandle<()>>,
    shares: Mutex<Option<[UnboundedReceiver<Share>; 2]>>,
    tareas: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Session {
    pub fn start(tx: UnboundedSender<String>, limite: Arc<AtomicU32>) -> Self {
        FrenoQuantus.fijar(0, false);
        let (ordenes, ord_rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let parando = Arc::new(AtomicBool::new(false));
        let cabeceras = Arc::new(Mutex::new(HashMap::new()));
        let (eng_tx, eng_rx) = unbounded_channel();
        let (usuario_tx, usuario_rx) = unbounded_channel();
        let (tokenminer_tx, tokenminer_rx) = unbounded_channel();
        let mesa = Arc::new(Mesa::nueva(MotorQuantus {
            ordenes: Mutex::new(ordenes.clone()),
            cancel: cancel.clone(),
            cabeceras: cabeceras.clone(),
        }));
        let mut hilos = Vec::new();
        {
            let cancel = cancel.clone();
            hilos.push(std::thread::spawn(move || {
                gpu_worker(Mode::Cuda, ord_rx, eng_tx, cancel)
            }));
        }
        {
            let luid = tokenminer_pearl::cuda::gpus()
                .ok()
                .and_then(|gpus| gpus.into_iter().next());
            let (parando, tx) = (parando.clone(), tx.clone());
            hilos.push(std::thread::spawn(move || {
                vigilar(
                    vec![(Arc::new(FrenoQuantus) as Arc<dyn Mando>, luid)],
                    limite,
                    parando,
                    tx,
                )
            }));
        }
        {
            let (mesa, parando, tx) = (mesa.clone(), parando.clone(), tx.clone());
            hilos.push(std::thread::spawn(move || {
                reenviar(
                    eng_rx,
                    mesa,
                    cabeceras,
                    [usuario_tx, tokenminer_tx],
                    parando,
                    tx,
                )
            }));
        }
        Self {
            ordenes,
            cancel,
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
        self.cancel.store(true, Ordering::Release);
        let _ = self.ordenes.send(Command::Stop);
        drop(self.ordenes);
        drop(self.mesa);
        for hilo in self.hilos {
            let _ = hilo.join();
        }
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
}
