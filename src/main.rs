#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use tokenminer_quantus::cuda as quantus_cuda;
#[cfg(windows)]
mod ajustes;
#[cfg(windows)]
mod bandeja;
#[cfg(windows)]
mod cambio;
#[cfg(windows)]
mod economia;
#[cfg(windows)]
mod pearl;
#[cfg(windows)]
mod pool;
#[cfg(windows)]
mod potencia;
#[cfg(windows)]
mod quantus;
#[cfg(windows)]
mod reparto;
#[cfg(windows)]
mod residente;

use primitive_types::U512;
use rand::Rng;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::WebSocketStream;

const PORT: u16 = 39847;

#[derive(Clone)]
struct Work {
    revision: u64,
    job_id: String,
    header: [u8; 32],
    difficulty: U512,
    extranonce: Vec<u8>,
}

enum Command {
    Work(Work),
    Pause,
    Stop,
}

fn send(tx: &mpsc::UnboundedSender<String>, value: serde_json::Value) {
    let _ = tx.send(value.to_string());
}

/// Freno del motor CUDA de Quantus en una GPU, que mueve el reparto de esa GPU: parte del tiempo que puede ocuparla
/// (0 o 100: sin freno) y si se aparta del todo. Cada GPU tiene el suyo.
#[derive(Default)]
pub struct FrenoGpu {
    pub limite: AtomicU32,
    pub apartado: AtomicBool,
}

/// El motor CUDA de Quantus en la GPU `dispositivo` (orden de CUDA). Cada GPU empieza en un nonce aleatorio, así que
/// varias GPU con el mismo trabajo no repiten intentos.
fn quantus_cuda_worker(
    dispositivo: usize,
    freno: Arc<FrenoGpu>,
    rx: std::sync::mpsc::Receiver<Command>,
    tx: mpsc::UnboundedSender<String>,
    cancel: Arc<AtomicBool>,
) {
    // Por defecto, el motor disperso q106 (filtro probabilístico; cada candidato se re-verifica full512 en CPU).
    // `dense` fuerza la ruta densa exacta.
    let mut sparse_kind =
        std::env::var("TOKENMINER_QUANTUS_CUDA_NONCE").unwrap_or_else(|_| "q106".into());
    if !matches!(sparse_kind.as_str(), "dense" | "q106") {
        send(
            &tx,
            json!({"type":"error","message":"Estrategia de nonce CUDA incorrecta"}),
        );
        return;
    }
    let mut fallback: Option<String> = None;
    let mut sparse = if sparse_kind != "dense" {
        match quantus_cuda::sparse::SparseCuda::new_en(dispositivo, 256, &sparse_kind).and_then(|mut sparse| {
            sparse.self_check()?;
            Ok(sparse)
        }) {
            Ok(sparse) => Some(sparse),
            // Si el motor disperso no carga o no pasa su autocheck, minar con la ruta densa exacta en vez de fallar.
            Err(error) if sparse_kind == "q106" => {
                fallback = Some(error.to_string());
                sparse_kind = "dense".into();
                None
            }
            Err(error) => {
                send(&tx, json!({"type":"error","message":error.to_string()}));
                return;
            }
        }
    } else {
        None
    };
    let dense_engine = if sparse.is_some() {
        quantus_cuda::QuantusCuda::new_variant_en(dispositivo, 256, "internal")
    } else {
        let variante = std::env::var("TOKENMINER_QUANTUS_CUDA_VARIANT").unwrap_or_else(|_| "baseline".into());
        quantus_cuda::QuantusCuda::new_variant_en(dispositivo, 256, &variante)
    };
    let mut gpu = match dense_engine {
        Ok(gpu) => gpu,
        Err(error) => {
            send(&tx, json!({"type":"error","message":error.to_string()}));
            return;
        }
    };
    if let Err(error) = gpu.self_check() {
        send(
            &tx,
            json!({"type":"error","message":format!("Autocheck Quantus CUDA: {error}")}),
        );
        return;
    }
    let variant = sparse
        .as_ref()
        .map_or(gpu.variant(), |sparse| sparse.variant())
        .to_string();
    send(
        &tx,
        json!({"type":"ready","mode":"cuda","devices":1,"gpu":gpu.name(),"variant":variant,
        "nonceStrategy":sparse_kind,"sparsePrefixLimitBytes":32,"fallback":fallback}),
    );
    let mut active: Option<Work> = None;
    let mut using_sparse = false;
    let mut nonce_base = [0u8; 64];
    let mut counter = 0u64;
    let mut attempts = 0u64;
    let mut physical_attempts = 0u64;
    let mut started = Instant::now();
    let mut reported = started;
    loop {
        let next = if active.is_some() {
            rx.try_recv().ok()
        } else {
            rx.recv_timeout(Duration::from_millis(200)).ok()
        };
        if let Some(command) = next {
            match command {
                Command::Stop => break,
                Command::Pause => {
                    active = None;
                    cancel.store(false, Ordering::Release);
                }
                Command::Work(work) => {
                    rand::rng().fill(&mut nonce_base[..56]);
                    nonce_base[..work.extranonce.len()].copy_from_slice(&work.extranonce);
                    nonce_base[56..].fill(0);
                    using_sparse = sparse.is_some() && work.extranonce.len() <= 32;
                    let prepared = if using_sparse {
                        sparse.as_mut().unwrap().prepare(
                            work.header,
                            nonce_base[..32].try_into().unwrap(),
                            rand::rng().random::<u64>(),
                            U512::MAX / work.difficulty,
                        )
                    } else {
                        gpu.prepare(work.header, nonce_base, U512::MAX / work.difficulty)
                    };
                    if let Err(error) = prepared {
                        send(&tx, json!({"type":"error","message":error.to_string()}));
                        break;
                    }
                    active = Some(work);
                    counter = 0;
                    attempts = 0;
                    physical_attempts = 0;
                    started = Instant::now();
                    reported = started;
                    cancel.store(false, Ordering::Release);
                }
            }
        }
        let Some(work) = active.as_ref() else {
            continue;
        };
        if freno.apartado.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if using_sparse && counter > quantus_cuda::sparse::LOCAL_MAX {
            if let Err(error) = sparse.as_mut().unwrap().next_group() {
                send(&tx, json!({"type":"error","message":error.to_string()}));
                break;
            }
            counter = 0;
        }
        let count = if using_sparse {
            sparse.as_ref().unwrap().batch_at(counter).unwrap()
        } else {
            quantus_cuda::BATCH
        };
        if !using_sparse && counter > u64::MAX - count as u64 {
            send(
                &tx,
                json!({"type":"error","message":"Rango de nonces Quantus agotado"}),
            );
            break;
        }
        let result = if using_sparse {
            sparse.as_mut().unwrap().mine(counter, count)
        } else {
            gpu.mine(counter, count)
        };
        let batch = match result {
            Ok(batch) => batch,
            Err(error) => {
                send(&tx, json!({"type":"error","message":error.to_string()}));
                break;
            }
        };
        if cancel.load(Ordering::Acquire) {
            continue;
        }
        let target = U512::MAX / work.difficulty;
        for index in batch.indices {
            let mut nonce = nonce_base;
            if using_sparse {
                nonce = sparse
                    .as_ref()
                    .unwrap()
                    .nonce(counter + index as u64)
                    .unwrap();
            } else {
                nonce[56..].copy_from_slice(&(counter + index as u64).to_be_bytes());
            }
            // Complete consensus hash and strict U512 comparator. Upper256 equality
            // is only a GPU filter hit; never a valid-share declaration.
            if quantus_cuda::valid_candidate(work.header, nonce, target) {
                let recipe = if using_sparse {
                    json!({"kind":sparse_kind,"group":sparse.as_ref().unwrap().group().to_string(),
                    "local":(counter+index as u64).to_string()})
                } else {
                    serde_json::Value::Null
                };
                send(
                    &tx,
                    json!({"type":"candidate","revision":work.revision,
                    "jobId":work.job_id,"nonceHex":hex::encode(nonce),"nonceRecipe":recipe}),
                );
            }
        }
        counter += count as u64;
        attempts += batch.unique_attempts;
        physical_attempts += batch.physical_attempts;
        let limite = freno.limite.load(Ordering::Relaxed);
        if (1..100).contains(&limite) {
            std::thread::sleep(Duration::from_secs_f64(
                batch.kernel_ms / 1000.0 * (100 - limite) as f64 / limite as f64,
            ));
        }
        if reported.elapsed() >= Duration::from_millis(500) {
            send(
                &tx,
                json!({"type":"stats","revision":work.revision,"attempts":attempts,
                "physicalAttempts":physical_attempts,"rate":attempts as f64/started.elapsed().as_secs_f64().max(0.001),
                "batch":count,"gpuMs":batch.kernel_ms,"kernelMs":batch.kernel_ms,"apiMs":batch.api_ms,
                "nonceStrategy":if using_sparse {sparse_kind.as_str()}else if sparse.is_some(){"dense-prefix-fallback"}else{"dense"}}),
            );
            reported = Instant::now();
        }
    }
}

/// Comprueba el enlace con el que la web abre el programa: `tokenminer://start/<token>?mode=cuda&coin=pearl`. Sólo
/// queda un modo, el programa de fondo que mina con CUDA y habla él con el pool; los que calculaban para una página
/// que llevaba el pool (`u32`, `u64`, `web-u32`) ya no existen y se rechazan.
fn parse_launch(argument: &str) -> Result<(), String> {
    let rest = argument
        .strip_prefix("tokenminer://start/")
        .ok_or("La solicitud de TokenMiner es incorrecta")?;
    let (token, query) = rest.split_once('?').unwrap_or((rest, ""));
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Token de sesión incorrecto".into());
    }
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        match pair.split_once('=').unwrap_or((pair, "")) {
            ("mode", "cuda") | ("coin", "pearl" | "quantus") | ("profile", "0") => {}
            ("mode", _) => return Err("Modo de cálculo incorrecto".into()),
            _ => return Err("Parámetro de cálculo incorrecto".into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    /// El motor CUDA de Quantus en la GPU `TOKENMINER_PRUEBA_GPU` (0 por defecto): arranca, mina y sus candidatos
    /// pasan la comprobación completa en CPU. Con dificultad baja salen candidatos en pocos segundos.
    #[test]
    #[ignore = "Requires an NVIDIA GPU"]
    fn quantus_cuda_en_una_gpu_elegida() {
        let dispositivo: usize = std::env::var("TOKENMINER_PRUEBA_GPU")
            .map(|d| d.parse().unwrap())
            .unwrap_or(0);
        let (ordenes, rx) = std::sync::mpsc::channel();
        let (tx, mut avisos) = mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let hilo = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                quantus_cuda_worker(dispositivo, Arc::new(FrenoGpu::default()), rx, tx, cancel)
            })
        };
        ordenes
            .send(Command::Work(Work {
                revision: 1,
                job_id: "prueba".into(),
                header: [7; 32],
                difficulty: U512::from(1u64 << 24),
                extranonce: vec![0xab; 4],
            }))
            .unwrap();
        let fin = Instant::now() + Duration::from_secs(8);
        let (mut listo, mut candidatos, mut tasa) = (false, 0, 0.0);
        while Instant::now() < fin {
            match avisos.try_recv() {
                Ok(texto) => {
                    let v: serde_json::Value = serde_json::from_str(&texto).unwrap();
                    match v["type"].as_str() {
                        Some("ready") => listo = true,
                        Some("candidate") => candidatos += 1,
                        Some("stats") => tasa = v["rate"].as_f64().unwrap(),
                        Some("error") => panic!("{v}"),
                        _ => {}
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        ordenes.send(Command::Stop).unwrap();
        hilo.join().unwrap();
        println!("GPU {dispositivo}: {candidatos} candidatos, {:.0} MH/s", tasa / 1e6);
        assert!(listo && candidatos > 0 && tasa > 0.0);
    }

    #[test]
    fn el_enlace_de_la_web_abre_el_programa_y_los_modos_antiguos_no() {
        let token = "ABCDEF0123456789ABCDEF0123456789";
        for query in [
            "?mode=cuda&coin=pearl",
            "?mode=cuda&coin=quantus",
            "?mode=cuda",
            "?mode=cuda&profile=0",
            "",
        ] {
            parse_launch(&format!("tokenminer://start/{token}{query}")).unwrap();
        }
        for request in [
            format!("tokenminer://start/{token}?mode=u32"),
            format!("tokenminer://start/{token}?mode=u64"),
            format!("tokenminer://start/{token}?mode=web-u32"),
            format!("tokenminer://start/{token}?mode=pearl"),
            format!("tokenminer://start/{token}?mode=cuda&coin=otra"),
            format!("tokenminer://start/{token}?mode=cuda&profile=1"),
            format!("https://start/{token}?mode=cuda"),
            "tokenminer://start/short?mode=cuda".into(),
            "tokenminer://start/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz?mode=cuda".into(),
        ] {
            assert!(parse_launch(&request).is_err(), "{request}");
        }
    }
}

/// Acepta la conexión sólo si viene de una de las webs fijadas al compilar (lista separada por comas): el navegador
/// no deja que otra página finja su origen.
async fn accept_from(
    stream: TcpStream,
    origin: String,
) -> tokio_tungstenite::tungstenite::Result<WebSocketStream<TcpStream>> {
    tokio_tungstenite::accept_hdr_async(stream, move |request: &Request, response: Response| {
        let accepted = request
            .headers()
            .get("origin")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|received| origin.split(',').any(|allowed| allowed == received));
        if accepted {
            Ok(response)
        } else {
            Err(Response::builder()
                .status(403)
                .body(Some("Origen no autorizado".into()))
                .unwrap())
        }
    })
    .await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Las GPU en el mismo orden que `nvidia-smi`, para que la web junte bien sus datos.
    std::env::set_var("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
    let allowed_origin = option_env!("TOKENMINER_ALLOWED_ORIGIN")
        .unwrap_or("http://localhost:3000")
        .to_string();
    // Lo abre Windows al arrancar o el instalador (`--background`), o la web con su enlace. En los dos casos es el
    // mismo programa de fondo; si ya hay uno en marcha, la página habla con él.
    let argumento = std::env::args()
        .nth(1)
        .ok_or("Falta la solicitud de TokenMiner")?;
    if argumento != "--background" {
        parse_launch(&argumento)?;
    }
    residente::correr(allowed_origin).await
}
