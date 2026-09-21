//! API C de la session « téléphone » (crate `streame-rtc`, feature `client`) pour l'app iOS.
//! Voir `ios/StreameCore/include/streame.h` pour la déclaration côté Swift.
//!
//! Conventions : les chaînes sont en UTF-8 terminées par zéro ; la configuration est un
//! objet JSON ; les événements arrivent par un callback (thread interne, à renvoyer sur le
//! thread principal côté Swift) avec le handle du client émetteur, un code et un JSON. Le
//! handle permet à l'app d'ignorer les événements tardifs d'un client qu'elle a déjà arrêté.
#![allow(clippy::not_unsafe_ptr_arg_deref)] // API C : les pointeurs viennent de l'app, contrat documenté dans streame.h

use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use serde::Deserialize;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::NonNull;
use std::time::Duration;
use streame_rtc::client::{AudioBackend, Client, ClientConfig, ClientEvent};

/// Codes d'événement (`streame_event_cb`).
pub const STREAME_EVENT_STATUS: i32 = 0;
pub const STREAME_EVENT_CONNECTED: i32 = 1;
pub const STREAME_EVENT_DISCONNECTED: i32 = 2;
pub const STREAME_EVENT_ON_AIR: i32 = 3;
pub const STREAME_EVENT_STATS: i32 = 4;
pub const STREAME_EVENT_ERROR: i32 = 5;
pub const STREAME_EVENT_ENDED: i32 = 6;
pub const STREAME_EVENT_CERT: i32 = 7;

/// Callback d'événement : `ctx` opaque, `client` = handle émetteur, `kind` = `STREAME_EVENT_*`,
/// `json` = charge utile.
pub type EventCb =
    Option<unsafe extern "C" fn(ctx: *mut c_void, client: *mut StreameClient, kind: i32, json: *const c_char)>;

/// Contexte utilisateur du callback (pointeur opaque de Swift, `Unmanaged`).
struct CbCtx(*mut c_void);
// SAFETY: le pointeur n'est jamais déréférencé ici, seulement rendu au callback.
unsafe impl Send for CbCtx {}
unsafe impl Sync for CbCtx {}
impl CbCtx {
    fn ptr(&self) -> *mut c_void {
        self.0
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct Config {
    host: String,
    port: u16,
    name: String,
    height: u32,
    fps: u32,
    mic_bitrate: i32,
    /// `true` = l'app fournit l'audio (AVAudioEngine) via `streame_client_push_audio` /
    /// `streame_client_pull_audio` ; `false` = vidéo seule.
    audio: bool,
    /// Empreinte SHA-256 hexadécimale du certificat mémorisé pour cette régie (absente à la
    /// première connexion).
    cert_fingerprint: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8443,
            name: "iPhone".into(),
            height: 1080,
            fps: 30,
            mic_bitrate: 96_000,
            audio: true,
            cert_fingerprint: None,
        }
    }
}

/// Handle opaque.
pub struct StreameClient {
    inner: Option<Client>,
}

/// Initialise les journaux (`tracing`) vers stderr — visibles dans la console Xcode.
/// `filter` : directive `RUST_LOG` (ex. `"info,streame_rtc=debug"`), NULL = `info`.
#[no_mangle]
pub extern "C" fn streame_log_init(filter: *const c_char) {
    let filter = if filter.is_null() {
        "info".to_string()
    } else {
        unsafe { CStr::from_ptr(filter) }.to_string_lossy().into_owned()
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Démarre le client. `config_json` : `{"host":"192.168.1.10","port":8443,"name":"iPhone",
/// "height":1080,"fps":30,"mic_bitrate":96000,"audio":true,"cert_fingerprint":"…"}` (toutes
/// les clés facultatives). Renvoie NULL en cas d'échec (détails dans les journaux).
#[no_mangle]
pub extern "C" fn streame_client_start(
    config_json: *const c_char,
    cb: EventCb,
    ctx: *mut c_void,
) -> *mut StreameClient {
    let json = if config_json.is_null() {
        "{}".to_string()
    } else {
        unsafe { CStr::from_ptr(config_json) }.to_string_lossy().into_owned()
    };
    let cfg: Config = match serde_json::from_str(&json) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("configuration JSON invalide : {e}");
            return std::ptr::null_mut();
        }
    };
    let ctx = CbCtx(ctx);
    // Le handle est alloué avant le démarrage pour que chaque événement porte son émetteur.
    let handle: *mut StreameClient = Box::into_raw(Box::new(StreameClient { inner: None }));
    let handle_for_cb = CbCtx(handle as *mut c_void);
    let on_event = move |ev: ClientEvent| {
        let Some(cb) = cb else { return };
        let (kind, payload) = match ev {
            ClientEvent::Status(t) => (STREAME_EVENT_STATUS, serde_json::json!({ "text": t })),
            ClientEvent::Connected => (STREAME_EVENT_CONNECTED, serde_json::json!({})),
            ClientEvent::Disconnected => (STREAME_EVENT_DISCONNECTED, serde_json::json!({})),
            ClientEvent::OnAir(on) => (STREAME_EVENT_ON_AIR, serde_json::json!({ "on": on })),
            ClientEvent::Stats(st) => (STREAME_EVENT_STATS, serde_json::to_value(st).unwrap_or_default()),
            ClientEvent::Error(m) => (STREAME_EVENT_ERROR, serde_json::json!({ "text": m })),
            ClientEvent::Ended(r) => (STREAME_EVENT_ENDED, serde_json::json!({ "text": r })),
            ClientEvent::Certificate { fingerprint, trusted } => {
                (STREAME_EVENT_CERT, serde_json::json!({ "fingerprint": fingerprint, "trusted": trusted }))
            }
        };
        let Ok(text) = CString::new(payload.to_string()) else { return };
        // SAFETY: `cb` est un pointeur de fonction fourni par l'app ; `ctx` et le handle lui
        // sont rendus tels quels (jamais déréférencés ici).
        unsafe { cb(ctx.ptr(), handle_for_cb.ptr() as *mut StreameClient, kind, text.as_ptr()) };
    };
    let client_cfg = ClientConfig {
        host: cfg.host,
        port: cfg.port,
        name: cfg.name,
        height: cfg.height,
        fps: cfg.fps,
        audio: if cfg.audio { AudioBackend::Bridge } else { AudioBackend::None },
        mic_bitrate: cfg.mic_bitrate,
        cert_fingerprint: cfg.cert_fingerprint,
    };
    match Client::start(client_cfg, on_event) {
        Ok(c) => {
            // SAFETY: le handle n'est pas encore connu de l'app ; seul ce thread y accède.
            unsafe { (*handle).inner = Some(c) };
            handle
        }
        Err(e) => {
            tracing::error!("démarrage du client : {e:#}");
            // SAFETY: alloué ci-dessus, jamais rendu à l'app.
            drop(unsafe { Box::from_raw(handle) });
            std::ptr::null_mut()
        }
    }
}

/// Image de la caméra : `pixel_buffer` = `CVPixelBufferRef` (NV12 ou BGRA), `pts_ns` =
/// horodatage de présentation en nanosecondes (`CMSampleBufferGetPresentationTimeStamp`).
/// Le tampon est retenu le temps de l'encodage ; l'appel ne bloque pas.
#[no_mangle]
pub extern "C" fn streame_client_push_video(
    client: *mut StreameClient,
    pixel_buffer: *mut c_void,
    pts_ns: i64,
) {
    let (Some(client), Some(pb)) = (unsafe { client.as_ref() }, NonNull::new(pixel_buffer as *mut CVPixelBuffer))
    else {
        return;
    };
    let Some(inner) = client.inner.as_ref() else { return };
    // SAFETY: `pb` est un CVPixelBuffer valide pour la durée de l'appel ; on le retient.
    let retained: CFRetained<CVPixelBuffer> = unsafe { CFRetained::retain(pb) };
    inner.push_video(retained, Duration::from_nanos(pts_ns.max(0) as u64));
}

/// Bloc du micro (callback temps réel du moteur audio) : F32 entrelacé, `channels` (1 ou 2),
/// `frames` trames, `sample_rate` Hz. Converti en stéréo 48 kHz et encodé en Opus côté Rust.
#[no_mangle]
pub extern "C" fn streame_client_push_audio(
    client: *mut StreameClient,
    samples: *const f32,
    frames: usize,
    channels: u32,
    sample_rate: u32,
) {
    let Some(bridge) = (unsafe { client.as_ref() }).and_then(|c| c.inner.as_ref()).and_then(|c| c.audio_bridge()) else {
        return;
    };
    if samples.is_null() || frames == 0 || channels == 0 {
        return;
    }
    // SAFETY: `samples` contient `frames × channels` flottants valides pour la durée de l'appel.
    let data = unsafe { std::slice::from_raw_parts(samples, frames * channels as usize) };
    bridge.push_mic(data, channels as usize, sample_rate);
}

/// Remplit un bloc de sortie (callback temps réel du moteur audio) : F32 entrelacé,
/// `channels` canaux, `frames` trames, `sample_rate` Hz. Renvoie `true` si du son a été
/// écrit (sinon silence : rien reçu du Mac, ou son coupé).
#[no_mangle]
pub extern "C" fn streame_client_pull_audio(
    client: *mut StreameClient,
    out: *mut f32,
    frames: usize,
    channels: u32,
    sample_rate: u32,
) -> bool {
    if out.is_null() || frames == 0 || channels == 0 {
        return false;
    }
    // SAFETY: `out` peut recevoir `frames × channels` flottants.
    let data = unsafe { std::slice::from_raw_parts_mut(out, frames * channels as usize) };
    let Some(bridge) = (unsafe { client.as_ref() }).and_then(|c| c.inner.as_ref()).and_then(|c| c.audio_bridge()) else {
        data.iter_mut().for_each(|s| *s = 0.0);
        return false;
    };
    bridge.pull_speaker(data, channels as usize, sample_rate)
}

#[no_mangle]
pub extern "C" fn streame_client_set_mic(client: *mut StreameClient, on: bool) {
    if let Some(c) = unsafe { client.as_ref() }.and_then(|c| c.inner.as_ref()) {
        c.set_mic(on);
    }
}

#[no_mangle]
pub extern "C" fn streame_client_set_speaker(client: *mut StreameClient, on: bool) {
    if let Some(c) = unsafe { client.as_ref() }.and_then(|c| c.inner.as_ref()) {
        c.set_speaker(on);
    }
}

/// Arrête le client (« bye » au Mac, fermeture) et libère le handle. Bloque au plus ~3 s.
#[no_mangle]
pub extern "C" fn streame_client_stop(client: *mut StreameClient) {
    if client.is_null() {
        return;
    }
    // SAFETY: le pointeur vient de `streame_client_start` et n'est plus utilisé après.
    let mut boxed = unsafe { Box::from_raw(client) };
    if let Some(c) = boxed.inner.take() {
        c.stop();
    }
}

#[cfg(test)]
mod tests {
    /// Les codes d'événement de `ios/StreameCore/include/streame.h` doivent être ceux d'ici.
    #[test]
    fn codes_evenement_identiques_au_header_c() {
        let header = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../ios/StreameCore/include/streame.h"
        ))
        .expect("streame.h");
        let expected = [
            ("STATUS", super::STREAME_EVENT_STATUS),
            ("CONNECTED", super::STREAME_EVENT_CONNECTED),
            ("DISCONNECTED", super::STREAME_EVENT_DISCONNECTED),
            ("ON_AIR", super::STREAME_EVENT_ON_AIR),
            ("STATS", super::STREAME_EVENT_STATS),
            ("ERROR", super::STREAME_EVENT_ERROR),
            ("ENDED", super::STREAME_EVENT_ENDED),
            ("CERT", super::STREAME_EVENT_CERT),
        ];
        let mut found = 0;
        for line in header.lines() {
            let Some(rest) = line.trim().strip_prefix("STREAME_EVENT_") else { continue };
            let Some((name, value)) = rest.split_once('=') else { continue };
            let value: i32 = value.split_whitespace().next().unwrap().trim_end_matches(',').parse().unwrap();
            let (_, rust) = expected
                .iter()
                .find(|(n, _)| *n == name.trim())
                .unwrap_or_else(|| panic!("STREAME_EVENT_{name} absent du Rust"));
            assert_eq!(*rust, value, "STREAME_EVENT_{name}");
            found += 1;
        }
        assert_eq!(found, expected.len(), "codes déclarés dans streame.h");
    }
}
