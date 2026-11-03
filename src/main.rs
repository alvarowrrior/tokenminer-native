#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use tokenminer_quantus::cuda as quantus_cuda;
use tokenminer_quantus::web as quantus_web;
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
mod quantus;
#[cfg(windows)]
mod reparto;
#[cfg(windows)]
mod residente;

use futures_util::{SinkExt, StreamExt};
use primitive_types::U512;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokenminer_quantus::engine_cpu::{AtomicBoolCancelCheck, EngineStatus, MinerEngine, Range};
use tokenminer_quantus::engine_gpu::GpuEngine;
use tokenminer_quantus::pow_core::JobContext;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const PORT: u16 = 39847;
const BATCH: u64 = 1_048_576;

#[derive(Clone, Copy)]
enum Mode {
    U32,
    U64,
    WebU32,
    Cuda,
    #[cfg(windows)]
    Pearl,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::WebU32 => "web-u32",
            Self::Cuda => "cuda",
            #[cfg(windows)]
            Self::Pearl => "cuda",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMessage {
    Hello {
        token: String,
    },
    Work {
        revision: u64,
        #[serde(rename = "jobId")]
        job_id: String,
        #[serde(rename = "miningHash")]
        mining_hash: String,
        difficulty: String,
        target: String,
        extranonce: String,
    },
    WebWork {
        revision: u64,
        #[serde(rename = "jobId")]
        job_id: String,
        prestate: Vec<u32>,
        target: String,
        #[serde(rename = "nonceBase")]
        nonce_base: String,
    },
    Pause,
    Stop,
}

struct Work {
    revision: u64,
    job_id: String,
    header: [u8; 32],
    difficulty: U512,
    extranonce: Vec<u8>,
}

struct WebWork {
    revision: u64,
    job_id: String,
    prestate: [u32; 24],
    target: [u32; 8],
    nonce_base: [u8; 64],
}

enum Command {
    Work(Work),
    WebWork(WebWork),
    Pause,
    Stop,
}

fn parse_web_work(message: ClientMessage) -> Result<WebWork, String> {
    let ClientMessage::WebWork {
        revision,
        job_id,
        prestate,
        target,
        nonce_base,
    } = message
    else {
        return Err("Se esperaba un trabajo Quantus u32".into());
    };
    if job_id.is_empty() || job_id.len() > 256 {
        return Err("ID de trabajo incorrecto".into());
    }
    let prestate: [u32; 24] = prestate
        .try_into()
        .map_err(|_| "El preestado debe tener 24 palabras")?;
    if target.len() != 128 || !target.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Objetivo Quantus incorrecto".into());
    }
    let mut target_words = [0u32; 8];
    for i in 0..8 {
        target_words[i] = u32::from_str_radix(&target[i * 8..i * 8 + 8], 16)
            .map_err(|_| "Objetivo Quantus incorrecto")?;
    }
    let nonce_base: [u8; 64] = hex::decode(nonce_base)
        .map_err(|_| "Nonce base incorrecto")?
        .try_into()
        .map_err(|_| "El nonce base debe tener 64 bytes")?;
    Ok(WebWork {
        revision,
        job_id,
        prestate,
        target: target_words,
        nonce_base,
    })
}

fn parse_work(message: ClientMessage) -> Result<Work, String> {
    let ClientMessage::Work {
        revision,
        job_id,
        mining_hash,
        difficulty,
        target,
        extranonce,
    } = message
    else {
        return Err("Se esperaba un trabajo Quantus".into());
    };
    if job_id.is_empty() || job_id.len() > 256 {
        return Err("ID de trabajo incorrecto".into());
    }
    let header: [u8; 32] = hex::decode(mining_hash)
        .map_err(|_| "Cabecera incorrecta")?
        .try_into()
        .map_err(|_| "La cabecera debe tener 32 bytes")?;
    let prefix = hex::decode(extranonce).map_err(|_| "Prefijo de nonce incorrecto")?;
    if prefix.len() > 56 {
        return Err("Prefijo de nonce demasiado largo".into());
    }
    let difficulty = U512::from_dec_str(&difficulty).map_err(|_| "Dificultad incorrecta")?;
    if difficulty.is_zero() {
        return Err("La dificultad debe ser positiva".into());
    }
    let expected = U512::MAX / difficulty;
    let target = U512::from_str_radix(&target, 16).map_err(|_| "Objetivo incorrecto")?;
    if expected != target {
        return Err("El objetivo no coincide con la dificultad".into());
    }
    Ok(Work {
        revision,
        job_id,
        header,
        difficulty,
        extranonce: prefix,
    })
}

fn send(tx: &mpsc::UnboundedSender<String>, value: serde_json::Value) {
    let _ = tx.send(value.to_string());
}

fn gpu_worker(
    mode: Mode,
    rx: std::sync::mpsc::Receiver<Command>,
    tx: mpsc::UnboundedSender<String>,
    cancel: Arc<AtomicBool>,
) {
    if matches!(mode, Mode::WebU32) {
        quantus_web_worker(rx, tx, cancel);
        return;
    }
    if matches!(mode, Mode::Cuda) {
        #[cfg(windows)]
        quantus_cuda_worker(rx, tx, cancel);
        #[cfg(not(windows))]
        send(
            &tx,
            json!({"type":"error", "message":"Quantus CUDA Q1 requiere Windows y NVIDIA SM120"}),
        );
        return;
    }
    if matches!(mode, Mode::U64) {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let supported = instance
            .enumerate_adapters(wgpu::Backends::PRIMARY)
            .into_iter()
            .any(|adapter| {
                adapter.get_info().device_type == wgpu::DeviceType::DiscreteGpu
                    && adapter.features().contains(wgpu::Features::SHADER_INT64)
            });
        if !supported {
            send(
                &tx,
                json!({ "type": "error", "message": "La GPU no anuncia SHADER_INT64 para el cálculo nativo" }),
            );
            return;
        }
    }
    if matches!(mode, Mode::U32) {
        std::env::set_var("TOKENMINER_QUANTUS_FORCE_U32", "1")
    }
    let engine = match GpuEngine::try_new(BATCH as u32, 0, false) {
        Ok(engine) => engine,
        Err(error) => {
            send(
                &tx,
                json!({ "type": "error", "message": format!("No se pudo iniciar la GPU: {error}") }),
            );
            return;
        }
    };
    send(
        &tx,
        json!({ "type": "ready", "mode": mode.label(), "devices": engine.device_count() }),
    );
    let mut active: Option<Work> = None;
    let mut context: Option<JobContext> = None;
    let mut base = U512::zero();
    let mut counter = 0u64;
    let mut attempts = 0u64;
    let mut started = Instant::now();
    let mut reported = Instant::now();

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
                    context = None;
                    cancel.store(false, Ordering::Release)
                }
                Command::WebWork(_) => {
                    send(
                        &tx,
                        json!({ "type": "error", "message": "Trabajo de moneda incorrecta" }),
                    );
                    break;
                }
                Command::Work(work) => {
                    let mut bytes = [0u8; 64];
                    rand::rng().fill(&mut bytes[..56]);
                    bytes[..work.extranonce.len()].copy_from_slice(&work.extranonce);
                    base = U512::from_big_endian(&bytes);
                    counter = 0;
                    attempts = 0;
                    started = Instant::now();
                    reported = started;
                    context = Some(engine.prepare_context(work.header, work.difficulty));
                    active = Some(work);
                    cancel.store(false, Ordering::Release);
                }
            }
        }
        let Some(work) = active.as_ref() else {
            continue;
        };
        if counter > u64::MAX - BATCH {
            send(
                &tx,
                json!({ "type": "error", "message": "Rango de nonces Quantus agotado" }),
            );
            active = None;
            continue;
        }
        let range_start = base + U512::from(counter);
        let outcome = engine.search_range(
            context.as_ref().expect("contexto preparado"),
            Range {
                start: range_start,
                end: range_start + U512::from(BATCH - 1),
            },
            &AtomicBoolCancelCheck(&cancel),
        );
        counter += BATCH;
        match outcome {
            EngineStatus::Found {
                candidate,
                hash_count,
                ..
            } => {
                attempts += hash_count;
                send(
                    &tx,
                    json!({ "type": "candidate", "revision": work.revision,
                    "jobId": work.job_id, "nonceHex": hex::encode(candidate.work) }),
                );
            }
            EngineStatus::Exhausted { hash_count } | EngineStatus::Cancelled { hash_count } => {
                attempts += hash_count;
            }
            EngineStatus::DeviceLost { .. } => {
                send(
                    &tx,
                    json!({ "type": "error", "message": "Se ha perdido la GPU" }),
                );
                break;
            }
            EngineStatus::Running { hash_count } => attempts += hash_count,
        }
        if reported.elapsed() >= Duration::from_millis(500) {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            send(
                &tx,
                json!({ "type": "stats", "revision": work.revision,
                "attempts": attempts, "rate": attempts as f64 / elapsed,
                "batch": BATCH }),
            );
            reported = Instant::now();
        }
    }
}

#[cfg(windows)]
/// Freno del motor CUDA de Quantus, que mueve el reparto de la GPU: parte del tiempo que puede ocuparla (0 o 100:
/// sin freno) y si se aparta del todo.
pub static LIMITE_QUANTUS: AtomicU32 = AtomicU32::new(0);
pub static APARTADO_QUANTUS: AtomicBool = AtomicBool::new(false);

fn quantus_cuda_worker(
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
        match quantus_cuda::sparse::SparseCuda::new(256, &sparse_kind).and_then(|mut sparse| {
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
        quantus_cuda::QuantusCuda::new_variant(256, "internal")
    } else {
        quantus_cuda::QuantusCuda::new(256)
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
                _ => {
                    send(
                        &tx,
                        json!({"type":"error","message":"Trabajo de moneda incorrecta"}),
                    );
                    break;
                }
            }
        }
        let Some(work) = active.as_ref() else {
            continue;
        };
        if APARTADO_QUANTUS.load(Ordering::Relaxed) {
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
        let limite = LIMITE_QUANTUS.load(Ordering::Relaxed);
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

fn quantus_web_worker(
    rx: std::sync::mpsc::Receiver<Command>,
    tx: mpsc::UnboundedSender<String>,
    cancel: Arc<AtomicBool>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            send(
                &tx,
                json!({ "type": "error", "message": error.to_string() }),
            );
            return;
        }
    };
    let gpu = match runtime.block_on(quantus_web::QuantusWebGpu::new()) {
        Ok(gpu) => gpu,
        Err(error) => {
            send(
                &tx,
                json!({ "type": "error", "message": format!("No se pudo preparar Quantus u32: {error}") }),
            );
            return;
        }
    };
    send(
        &tx,
        json!({ "type": "ready", "mode": "web-u32", "devices": 1, "gpu": gpu.name() }),
    );
    let mut active: Option<WebWork> = None;
    let mut counter = 0u64;
    let mut attempts = 0u64;
    let mut started = Instant::now();
    let mut reported = Instant::now();
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
                    cancel.store(false, Ordering::Release)
                }
                Command::WebWork(work) => {
                    active = Some(work);
                    counter = 0;
                    attempts = 0;
                    started = Instant::now();
                    reported = started;
                    cancel.store(false, Ordering::Release);
                }
                _ => {
                    send(
                        &tx,
                        json!({ "type": "error", "message": "Trabajo de moneda incorrecta" }),
                    );
                    break;
                }
            }
        }
        let Some(work) = active.as_ref() else {
            continue;
        };
        let batch_size = quantus_web::QuantusWebGpu::batch() as u64;
        if counter > u64::MAX - batch_size {
            send(
                &tx,
                json!({ "type": "error", "message": "Rango de nonces agotado" }),
            );
            break;
        }
        let (indices, elapsed) = match gpu.mine(&work.prestate, &work.target, counter) {
            Ok(result) => result,
            Err(error) => {
                send(
                    &tx,
                    json!({ "type": "error", "message": format!("Cálculo Quantus u32: {error}") }),
                );
                break;
            }
        };
        if cancel.load(Ordering::Acquire) {
            continue;
        }
        for index in indices {
            let mut nonce = work.nonce_base;
            nonce[56..64].copy_from_slice(&(counter + index as u64).to_be_bytes());
            send(
                &tx,
                json!({ "type": "candidate", "revision": work.revision,
                "jobId": work.job_id, "nonceHex": hex::encode(nonce) }),
            );
        }
        counter += batch_size;
        attempts += batch_size;
        if reported.elapsed() >= Duration::from_millis(500) {
            send(
                &tx,
                json!({ "type": "stats", "revision": work.revision,
                "attempts": attempts, "rate": attempts as f64 / started.elapsed().as_secs_f64().max(0.001),
                "batch": batch_size, "gpuMs": elapsed }),
            );
            reported = Instant::now();
        }
    }
}

fn launch() -> Result<(String, Mode), String> {
    let argument = std::env::args()
        .nth(1)
        .ok_or("Falta la solicitud de TokenMiner")?;
    parse_launch(&argument)
}

fn parse_launch(argument: &str) -> Result<(String, Mode), String> {
    let rest = argument
        .strip_prefix("tokenminer://start/")
        .ok_or("La solicitud de TokenMiner es incorrecta")?;
    let (token, query) = rest.split_once('?').unwrap_or((rest, ""));
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Token de sesión incorrecto".into());
    }

    let (mut mode_name, mut coin) = (None, None);
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match (name, value) {
            ("mode", value) => mode_name = Some(value),
            ("coin", value) => coin = Some(value),
            ("profile", "0") => {}
            _ => return Err("Parámetro de cálculo incorrecto".into()),
        }
    }
    let mode = match (mode_name, coin.unwrap_or("quantus")) {
        (Some("u64"), "quantus") => Mode::U64,
        (Some("u32"), "quantus") => Mode::U32,
        (Some("web-u32"), "quantus") => Mode::WebU32,
        (Some("cuda"), "quantus") => Mode::Cuda,
        #[cfg(windows)]
        (Some("cuda"), "pearl") => Mode::Pearl,
        _ => return Err("Modo de cálculo incorrecto".into()),
    };
    Ok((token.to_ascii_lowercase(), mode))
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn quantus_launch_modes_and_session_contract() {
        let token = "ABCDEF0123456789ABCDEF0123456789";
        for mode in ["cuda", "u64", "u32", "web-u32"] {
            let (parsed, selected) =
                parse_launch(&format!("tokenminer://start/{token}?mode={mode}")).unwrap();
            assert_eq!(parsed, token.to_ascii_lowercase());
            assert_eq!(selected.label(), mode);
            parse_launch(&format!(
                "tokenminer://start/{token}?mode={mode}&coin=quantus"
            ))
            .unwrap();
        }
        #[cfg(windows)]
        assert!(matches!(
            parse_launch(&format!("tokenminer://start/{token}?mode=cuda&coin=pearl"))
                .unwrap()
                .1,
            Mode::Pearl
        ));
        for request in [
            format!("tokenminer://start/{token}?mode=pearl"),
            format!("tokenminer://start/{token}?mode=u32&coin=pearl"),
            format!("tokenminer://start/{token}?mode=cuda&coin=otra"),
            format!("tokenminer://start/{token}?mode=cuda&profile=1"),
            format!("tokenminer://start/{token}"),
            format!("https://start/{token}?mode=cuda"),
            "tokenminer://start/short?mode=cuda".into(),
            "tokenminer://start/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz?mode=cuda".into(),
        ] {
            assert!(parse_launch(&request).is_err(), "{request}");
        }
        assert!(serde_json::from_value::<ClientMessage>(json!({
            "type":"pearlWork", "revision":1, "jobId":"old", "jobKey":"00", "target":"01"
        }))
        .is_err());
    }

    #[test]
    fn quantus_work_keeps_target_and_prefix_validation() {
        let work = |difficulty: &str, target: String, prefix_bytes: usize| ClientMessage::Work {
            revision: 7,
            job_id: "quantus-job".into(),
            mining_hash: "ab".repeat(32),
            difficulty: difficulty.into(),
            target,
            extranonce: "cd".repeat(prefix_bytes),
        };
        let target = U512::MAX / U512::from(2);
        let parsed = parse_work(work("2", format!("{target:x}"), 56)).unwrap();
        assert_eq!(parsed.header, [0xab; 32]);
        assert_eq!(parsed.extranonce, vec![0xcd; 56]);
        assert_eq!(parsed.revision, 7);
        assert!(parse_work(work("2", format!("{:x}", target - U512::one()), 0)).is_err());
        assert!(parse_work(work("0", "00".into(), 0)).is_err());
        assert!(parse_work(work("2", format!("{target:x}"), 57)).is_err());
    }

    // Extremo a extremo: el ejecutable real del helper, hablado como la web. Requiere
    // `cargo build --release --bin tokenminer-helper` (origen por defecto http://localhost:3000) y GPU NVIDIA libre.
    #[tokio::test]
    #[ignore = "Requires built helper exe, an NVIDIA GPU and port 39847 free"]
    async fn helper_end_to_end_default() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let exe = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("tokenminer-helper.exe");
        assert!(exe.exists(), "falta {exe:?}: compilar el helper antes");
        let token = "0123456789abcdef0123456789abcdef";
        let mut child = std::process::Command::new(&exe)
            .arg(format!("tokenminer://start/{token}?mode=cuda"))
            .spawn()
            .unwrap();
        let mut request = "ws://127.0.0.1:39847".into_client_request().unwrap();
        request
            .headers_mut()
            .insert("origin", "http://localhost:3000".parse().unwrap());
        let mut socket = None;
        for _ in 0..50 {
            if let Ok((s, _)) = tokio_tungstenite::connect_async(request.clone()).await {
                socket = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let mut socket = socket.expect("el helper no abrió el puente local");
        socket
            .send(Message::Text(
                json!({"type":"hello","token":token}).to_string(),
            ))
            .await
            .unwrap();
        let difficulty = U512::from(1u64 << 20);
        let target = U512::MAX / difficulty;
        let jobs = [
            ("job-a", [0x11u8; 32], "a1b2c3d4e5f60718", 1u64),
            ("job-b", [0x22u8; 32], "0f1e2d3c", 2),
        ];
        let send_work = |job: &(&str, [u8; 32], &str, u64)| {
            json!({"type":"work","revision":job.3,"jobId":job.0,
            "miningHash":hex::encode(job.1),"difficulty":difficulty.to_string(),"target":format!("{target:x}"),"extranonce":job.2}).to_string()
        };
        let (mut ready, mut stats_rate, mut checked, mut current) = (None, 0f64, 0usize, 0usize);
        let mut seen = std::collections::HashSet::new();
        socket
            .send(Message::Text(send_work(&jobs[0])))
            .await
            .unwrap();
        let begin = Instant::now();
        while begin.elapsed() < Duration::from_secs(70) {
            if current == 0 && begin.elapsed() > Duration::from_secs(45) {
                current = 1;
                socket
                    .send(Message::Text(send_work(&jobs[1])))
                    .await
                    .unwrap();
            }
            let Ok(Some(Ok(Message::Text(body)))) =
                tokio::time::timeout(Duration::from_secs(5), socket.next()).await
            else {
                continue;
            };
            let event: serde_json::Value = serde_json::from_str(&body).unwrap();
            match event["type"].as_str().unwrap() {
                "ready" => ready = Some(event.clone()),
                "error" => panic!("error del helper: {event}"),
                "stats" => {
                    if event["revision"] == jobs[current].3 {
                        stats_rate = event["rate"].as_f64().unwrap()
                    }
                }
                "candidate" => {
                    let revision = event["revision"].as_u64().unwrap();
                    let job = jobs.iter().find(|j| j.3 == revision).unwrap();
                    let nonce: [u8; 64] = hex::decode(event["nonceHex"].as_str().unwrap())
                        .unwrap()
                        .try_into()
                        .unwrap();
                    assert_eq!(event["jobId"], job.0);
                    assert!(
                        nonce.starts_with(&hex::decode(job.2).unwrap()),
                        "prefijo extranonce perdido"
                    );
                    assert!(
                        tokenminer_quantus::pow_core::get_nonce_hash(job.1, nonce) < target,
                        "candidato que no cumple el target full512"
                    );
                    assert!(seen.insert(nonce), "nonce repetido");
                    if current == 1 && begin.elapsed() > Duration::from_secs(47) {
                        assert_eq!(revision, 2, "candidato de un trabajo ya sustituido")
                    }
                    checked += 1;
                }
                _ => {}
            }
        }
        socket
            .send(Message::Text(json!({"type":"pause"}).to_string()))
            .await
            .unwrap();
        socket
            .send(Message::Text(json!({"type":"stop"}).to_string()))
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("el helper no terminó tras stop");
        let ready = ready.expect("sin mensaje ready");
        eprintln!("e2e: ready {ready}\n     {checked} candidatos verificados full512, tasa final {:.1} MH/s, salida {status}", stats_rate / 1e6);
        assert_eq!(ready["nonceStrategy"], "q106");
        assert_eq!(ready["variant"], "sparse-q106");
        assert!(ready["fallback"].is_null());
        assert!(checked > 1000 && stats_rate > 1e8);
    }

    // Web · tasa del shader WGSL u32 (el que ejecutaría el navegador) en nativo con wgpu. Backend con
    // TOKENMINER_WGPU_BACKEND=dx12 (como Chrome en Windows) o vulkan. Calienta 30 s, mide 20 s.
    #[tokio::test]
    #[ignore = "Requires a discrete GPU; GPU libre"]
    async fn web_u32_rate() {
        let gpu = quantus_web::QuantusWebGpu::new().await.unwrap();
        let vector: serde_json::Value = serde_json::from_str(quantus_web::VECTOR).unwrap();
        let prestate: [u32; 24] = vector["preestado"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w.as_u64().unwrap() as u32)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let target = [0u32; 8];
        let (mut counter, mut hashes) = (0u64, 0u64);
        let begin = Instant::now();
        while begin.elapsed() < Duration::from_secs(50) {
            let (_, _) = gpu.mine(&prestate, &target, counter).unwrap();
            counter += quantus_web::QuantusWebGpu::batch() as u64;
            if begin.elapsed() > Duration::from_secs(30) {
                hashes += quantus_web::QuantusWebGpu::batch() as u64;
            }
        }
        eprintln!(
            "web-u32 {} ({:?}): {:.1} MH/s",
            gpu.name(),
            std::env::var("TOKENMINER_WGPU_BACKEND").ok(),
            hashes as f64 / (begin.elapsed().as_secs_f64() - 30.0) / 1e6
        );
    }
}

async fn check_quantus_web() -> Result<(), Box<dyn std::error::Error>> {
    let vector: serde_json::Value = serde_json::from_str(quantus_web::VECTOR)?;
    let words = vector["preestado"]
        .as_array()
        .ok_or("Falta el preestado Quantus")?;
    let prestate: [u32; 24] = words
        .iter()
        .map(|word| {
            word.as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or("Preestado Quantus incorrecto")
        })
        .collect::<Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| "El preestado Quantus debe tener 24 palabras")?;
    let parse_target = |name: &str| -> Result<[u32; 8], Box<dyn std::error::Error>> {
        let target = vector[name].as_str().ok_or("Falta el objetivo Quantus")?;
        if target.len() != 64 {
            return Err("Objetivo Quantus de longitud incorrecta".into());
        }
        let mut words = [0u32; 8];
        for (i, word) in words.iter_mut().enumerate() {
            *word = u32::from_str_radix(&target[i * 8..i * 8 + 8], 16)?;
        }
        Ok(words)
    };
    let gpu = quantus_web::QuantusWebGpu::new().await?;
    if !gpu.check_one(&prestate, &parse_target("objetivoIgual")?)? {
        return Err("El shader Quantus u32 no coincide con el vector CPU".into());
    }
    if gpu.check_one(&prestate, &parse_target("objetivoMenor")?)? {
        return Err("El shader Quantus u32 aceptó un hash fuera del objetivo".into());
    }
    Ok(())
}

struct Session {
    commands: std::sync::mpsc::Sender<Command>,
    cancel: Arc<AtomicBool>,
    worker: std::thread::JoinHandle<()>,
}

impl Session {
    fn start(mode: Mode, events: mpsc::UnboundedSender<String>) -> Self {
        let (commands, commands_rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let worker =
            std::thread::spawn(move || gpu_worker(mode, commands_rx, events, thread_cancel));
        Self {
            commands,
            cancel,
            worker,
        }
    }

    /// Un mensaje de la web. Devuelve `true` si pide parar.
    fn handle(&self, body: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let Self {
            commands, cancel, ..
        } = self;
        match serde_json::from_str::<ClientMessage>(body)? {
            ClientMessage::Work { .. } => {
                cancel.store(true, Ordering::Release);
                let work = parse_work(serde_json::from_str(body)?)?;
                commands.send(Command::Work(work))?;
            }
            ClientMessage::WebWork { .. } => {
                cancel.store(true, Ordering::Release);
                let work = parse_web_work(serde_json::from_str(body)?)?;
                commands.send(Command::WebWork(work))?;
            }
            ClientMessage::Pause => {
                cancel.store(true, Ordering::Release);
                commands.send(Command::Pause)?;
            }
            ClientMessage::Stop => return Ok(true),
            ClientMessage::Hello { .. } => return Err("Autenticación duplicada".into()),
        }
        Ok(false)
    }

    fn finish(self) {
        self.cancel.store(true, Ordering::Release);
        let _ = self.commands.send(Command::Stop);
        let _ = self.worker.join();
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
    if std::env::args().nth(1).as_deref() == Some("--self-check-quantus-web") {
        return check_quantus_web().await;
    }
    let allowed_origin = option_env!("TOKENMINER_ALLOWED_ORIGIN")
        .unwrap_or("http://localhost:3000")
        .to_string();
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--background") {
        return residente::correr(allowed_origin).await;
    }
    let (token, mode) = launch()?;
    #[cfg(windows)]
    if matches!(mode, Mode::Pearl) {
        return residente::correr(allowed_origin).await;
    }
    let listener = TcpListener::bind(("127.0.0.1", PORT)).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut socket = loop {
        let (stream, _) = tokio::time::timeout_at(deadline, listener.accept()).await??;
        if let Ok(socket) = accept_from(stream, allowed_origin.clone()).await {
            break socket;
        }
    };
    let hello = tokio::time::timeout(Duration::from_secs(8), socket.next())
        .await?
        .ok_or("La página no completó la conexión")??;
    let ClientMessage::Hello { token: received } = serde_json::from_str(hello.to_text()?)? else {
        return Err("Falta la autenticación local".into());
    };
    if received.to_ascii_lowercase() != token {
        return Err("Token local incorrecto".into());
    }

    let (mut writer, mut reader) = socket.split();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let session = Session::start(mode, events_tx);
    let mut pings = tokio::time::interval(Duration::from_secs(10));
    let mut last_pong = Instant::now();
    let result: Result<(), Box<dyn std::error::Error>> = loop {
        tokio::select! {
            inbound = reader.next() => {
                let Some(inbound) = inbound else { break Ok(()) };
                match inbound? {
                    Message::Text(body) => {
                        if body.len() > 8192 { break Err("Mensaje local demasiado largo".into()) }
                        if session.handle(&body)? { break Ok(()) }
                    }
                    Message::Pong(_) => last_pong = Instant::now(),
                    Message::Close(_) => break Ok(()),
                    _ => {}
                }
            }
            outgoing = events_rx.recv() => {
                let Some(outgoing) = outgoing else { break Ok(()) };
                writer.send(Message::Text(outgoing)).await?;
            }
            _ = pings.tick() => {
                if last_pong.elapsed() > Duration::from_secs(30) { break Ok(()) }
                writer.send(Message::Ping(Vec::new())).await?;
            }
        }
    };
    session.finish();
    result
}
