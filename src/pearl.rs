//! Pearl en el helper: arranca un motor por GPU, sus conexiones con el pool y sus turnos, y traduce sus avisos a
//! mensajes. Todas las GPU reciben los mismos trabajos y sus shares salen por la misma conexión.
//!
//! Eventos para quien lo usa: `{"type":"ready",…}`, `{"type":"pool","state":"connecting|connected|reconnecting"}`,
//! `{"type":"share","accepted":bool,"error":…}`,
//! `{"type":"stats","revision":n,"attempts":…,"rate":<MAC/s>,"devices":[<MAC/s de cada GPU>],"sm":[<multiprocesadores de cada GPU>]}`,
//! `{"type":"gpu","devices":[{"share":<% del tiempo para el motor>,"others":<% de los demás>,"mine":<% del motor>}]}` y
//! `{"type":"error","message":…}`. Las GPU van en el orden de CUDA, que el programa fija al de `nvidia-smi`.
use crate::ajustes::Moneda;
use crate::pool::{
    minar, turnos, wallet_comision, wallet_valida, Mesa, Mina, Share, TOKENMINER, USUARIO,
};
use crate::reparto::{vigilar, Mando};
use crate::send;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokenminer_pearl::trabajador::{trabajar, Aviso, Contadores, Orden};
use tokenminer_pearl::K;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const M: u32 = 2048;
const LOTE: u32 = 8;
const STATS_CADA: Duration = Duration::from_secs(2);

/// Un aviso de la GPU `i`, o que su motor ha terminado.
enum Evento {
    Aviso(usize, Aviso),
    Fin(usize),
}

fn etiquetar(i: usize, avisos: mpsc::Receiver<Aviso>, eventos: mpsc::Sender<Evento>) {
    for aviso in avisos {
        if eventos.send(Evento::Aviso(i, aviso)).is_err() {
            return;
        }
    }
    let _ = eventos.send(Evento::Fin(i));
}

/// Cada orden de la mesa va a todos los motores.
fn difundir(ordenes: mpsc::Receiver<Orden>, motores: Vec<mpsc::Sender<Orden>>) {
    for orden in ordenes {
        let parar = matches!(orden, Orden::Parar);
        for motor in &motores {
            let _ = motor.send(orden.clone());
        }
        if parar {
            return;
        }
    }
}

/// Pasa cada share a la conexión que dio su trabajo y el resto a la web. Cuenta la tasa de cada GPU desde el último
/// trabajo. Una GPU que no arranca no para a las demás: sólo si no arranca ninguna se avisa del error.
fn reenviar(
    eventos: mpsc::Receiver<Evento>,
    vigente: Arc<AtomicU64>,
    gpus: Vec<Arc<Contadores>>,
    parando: Arc<AtomicBool>,
    mesa: Arc<Mesa>,
    shares: [UnboundedSender<Share>; 2],
    tx: UnboundedSender<String>,
) {
    let cuantas = gpus.len();
    let (mut macs, mut listas, mut vivas, mut fallo) =
        (vec![0f64; cuantas], vec![false; cuantas], cuantas, None);
    let mut vueltas = vec![0u64; cuantas];
    // Multiprocesadores de cada GPU, 0 hasta que su motor está listo.
    let mut sms = vec![0u32; cuantas];
    let (mut revision, mut desde, mut aviso) = (0u64, Instant::now(), Instant::now() + STATS_CADA);
    let perdida = |mensaje: &str| {
        if !parando.load(Ordering::SeqCst) {
            send(&tx, json!({"type": "error", "message": mensaje}))
        }
    };
    loop {
        match eventos.recv_timeout(Duration::from_millis(200)) {
            Ok(Evento::Aviso(
                i,
                Aviso::Listo {
                    m,
                    n,
                    mu,
                    nu,
                    variante,
                    sm,
                },
            )) => {
                macs[i] = mu as f64 * nu as f64 * K as f64;
                sms[i] = sm;
                if !listas.contains(&true) {
                    send(
                        &tx,
                        json!({"type": "ready", "mode": "cuda", "coin": "pearl", "devices": cuantas, "m": m, "n": n,
                                     "k": K, "variant": format!("{variante:?}")}),
                    );
                }
                listas[i] = true;
            }
            Ok(Evento::Aviso(_, Aviso::Share { job_id, prueba, .. })) => {
                if let Some(conexion) = mesa.dueno(&job_id) {
                    let _ = shares[conexion].send(Share {
                        job_id,
                        datos: json!({"plain_proof": prueba}),
                    });
                }
            }
            Ok(Evento::Aviso(i, Aviso::Error(e))) if !listas[i] => fallo = Some(e),
            Ok(Evento::Aviso(_, Aviso::Error(e))) => {
                send(&tx, json!({"type": "error", "message": e}))
            }
            Ok(Evento::Fin(i)) => {
                vivas -= 1;
                if vivas == 0 && !listas.contains(&true) {
                    perdida(fallo.as_deref().unwrap_or("Se ha perdido la GPU"));
                } else if listas[i] {
                    perdida("Se ha perdido una GPU");
                }
                if vivas == 0 {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let ahora = Instant::now();
        let actual = vigente.load(Ordering::SeqCst);
        if actual != revision {
            (revision, desde, aviso) = (actual, ahora, ahora + STATS_CADA);
            vueltas = gpus
                .iter()
                .map(|c| c.rounds.load(Ordering::Relaxed))
                .collect();
        }
        if actual != 0 && ahora >= aviso {
            aviso = ahora + STATS_CADA;
            let segundos = ahora.duration_since(desde).as_secs_f64().max(0.001);
            let intentos: Vec<f64> = gpus
                .iter()
                .zip(&vueltas)
                .zip(&macs)
                .map(|((c, antes), macs)| (c.rounds.load(Ordering::Relaxed) - antes) as f64 * macs)
                .collect();
            let total: f64 = intentos.iter().sum();
            send(
                &tx,
                json!({"type": "stats", "revision": actual, "attempts": total, "rate": total / segundos,
                             "devices": intentos.iter().map(|i| i / segundos).collect::<Vec<_>>(), "sm": sms}),
            );
        }
    }
}

pub struct Session {
    ordenes: mpsc::Sender<Orden>,
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
        let vigente = Arc::new(AtomicU64::new(0));
        let parando = Arc::new(AtomicBool::new(false));
        let (ordenes, ord_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        let (usuario_tx, usuario_rx) = unbounded_channel();
        let (tokenminer_tx, tokenminer_rx) = unbounded_channel();
        let mesa = Arc::new(Mesa::pearl(ordenes.clone(), vigente.clone()));
        let (mut hilos, mut destinos, mut contadores) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..cuantas {
            let (o_tx, o_rx) = mpsc::channel();
            let (av_tx, av_rx) = mpsc::channel();
            let c = Arc::new(Contadores::default());
            let (vigente, c_motor, ev_tx) = (vigente.clone(), c.clone(), ev_tx.clone());
            hilos.push(std::thread::spawn(move || {
                let _ = trabajar(i as i32, M, 0, LOTE, None, o_rx, vigente, c_motor, av_tx);
            }));
            hilos.push(std::thread::spawn(move || etiquetar(i, av_rx, ev_tx)));
            destinos.push(o_tx);
            contadores.push(c);
        }
        drop(ev_tx);
        hilos.push(std::thread::spawn(move || difundir(ord_rx, destinos)));
        {
            let gpus = contadores
                .iter()
                .enumerate()
                .map(|(i, c)| (c.clone() as Arc<dyn Mando>, luids.get(i).cloned()))
                .collect();
            let (parando, tx) = (parando.clone(), tx.clone());
            hilos.push(std::thread::spawn(move || {
                vigilar(gpus, limite, parando, tx)
            }));
        }
        {
            let (parando, mesa, tx) = (parando.clone(), mesa.clone(), tx.clone());
            hilos.push(std::thread::spawn(move || {
                reenviar(
                    ev_rx,
                    vigente,
                    contadores,
                    parando,
                    mesa,
                    [usuario_tx, tokenminer_tx],
                    tx,
                )
            }));
        }
        Self {
            ordenes,
            mesa,
            parando,
            tx,
            hilos,
            shares: Mutex::new(Some([usuario_rx, tokenminer_rx])),
            tareas: Mutex::new(Vec::new()),
        }
    }

    /// Empieza a minar a nombre de `wallet`. Una sesión sólo mina para una cuenta.
    pub fn minar(&self, wallet: &str) -> Result<(), String> {
        if !wallet_valida(wallet) {
            return Err("Cuenta de minado incorrecta".into());
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
                    moneda: Moneda::Pearl,
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
        let _ = self.ordenes.send(Orden::Parar);
        drop(self.ordenes);
        for hilo in self.hilos {
            let _ = hilo.join();
        }
    }
}
