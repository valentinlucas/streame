// API C de la session « téléphone » Streame (crates/streame-ios, Rust).
// Même pile que la régie Mac : webrtc-rs, libopus, VideoToolbox, cpal (CoreAudio).
#ifndef STREAME_H
#define STREAME_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct StreameClient StreameClient;

// Codes d'événement livrés au callback.
enum {
    STREAME_EVENT_STATUS = 0,        // {"text": "..."}
    STREAME_EVENT_CONNECTED = 1,     // {}
    STREAME_EVENT_DISCONNECTED = 2,  // {}  (reconnexion automatique en cours)
    STREAME_EVENT_ON_AIR = 3,        // {"on": true|false}
    STREAME_EVENT_STATS = 4,         // {"width","height","fps","bitrate_kbps","quality_limitation","rtt_ms","codec"}
    STREAME_EVENT_ERROR = 5,         // {"text": "..."}
    STREAME_EVENT_ENDED = 6,         // {"text": "raison"}  (le Mac a mis fin à la session)
    STREAME_EVENT_CERT = 7,          // {"fingerprint": "sha256 hex", "trusted": true|false}
                                     // trusted=false : certificat différent de celui mémorisé,
                                     // connexion refusée tant que l'app n'accepte pas le nouveau.
};

// Appelé depuis un thread interne : renvoyer sur le thread principal côté Swift, dans l'ordre
// (DispatchQueue.main.async). `client` = handle émetteur : à comparer au client courant pour
// ignorer les événements tardifs d'un client déjà arrêté.
typedef void (*streame_event_cb)(void *ctx, StreameClient *client, int32_t kind, const char *json);

// Journaux (tracing) vers stderr ; `filter` façon RUST_LOG (NULL = "info").
void streame_log_init(const char *filter);

// Démarre le client. `config_json` :
//   {"host":"192.168.1.10","port":8443,"name":"iPhone","height":1080,"fps":30,
//    "mic_bitrate":96000,"audio":true,"cert_fingerprint":"<sha256 hex, facultatif>"}
// "audio": true = l'app fournit le micro et joue la sortie (AVAudioEngine) via
// streame_client_push_audio / streame_client_pull_audio. "cert_fingerprint" = empreinte du
// certificat mémorisée pour cette régie (STREAME_EVENT_CERT la fournit à la première
// connexion). Ne pas appeler sur le thread principal (quelques dizaines de ms). NULL si échec.
StreameClient *streame_client_start(const char *config_json, streame_event_cb cb, void *ctx);

// Bloc du micro, depuis le callback temps réel du moteur audio : F32 entrelacé,
// `channels` (1 ou 2), `frames` trames (≤ 4096, tronqué au-delà), `sample_rate` Hz.
// Ne bloque pas, n'alloue pas (copie).
void streame_client_push_audio(StreameClient *client, const float *samples, size_t frames,
                               uint32_t channels, uint32_t sample_rate);

// Remplit un bloc de sortie, depuis le callback de rendu du moteur audio : F32 entrelacé,
// `frames` ≤ 4096 (silence au-delà). Renvoie true si du son a été écrit (sinon silence).
// Ne bloque pas, n'alloue pas.
bool streame_client_pull_audio(StreameClient *client, float *out, size_t frames,
                               uint32_t channels, uint32_t sample_rate);

// Image de la caméra (CVPixelBufferRef, NV12 « 420v/420f » de préférence) avec son
// horodatage de présentation en nanosecondes. Ne bloque jamais.
void streame_client_push_video(StreameClient *client, void *pixel_buffer, int64_t pts_ns);

void streame_client_set_mic(StreameClient *client, bool on);
void streame_client_set_speaker(StreameClient *client, bool on);

// « bye » au Mac, fermeture, libération du handle (≤ ~3 s).
void streame_client_stop(StreameClient *client);

#ifdef __cplusplus
}
#endif

#endif
