//! Configuration de l'application (fichier TOML).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub video: VideoConfig,
    pub output: OutputConfig,
    pub multiview: MultiviewConfig,
    pub audio: AudioConfig,
    pub transition: TransitionConfig,
    pub streamdeck: StreamDeckConfig,
    pub scenes: Vec<SceneConfig>,
    /// Répertoire de base pour les chemins relatifs (renseigné au chargement).
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Adresse d'écoute HTTPS (le téléphone s'y connecte).
    pub bind: String,
    /// Répertoire où sont stockés/générés le certificat et la clé TLS.
    pub cert_dir: String,
    /// Serveur STUN (optionnel, utile hors LAN). Vide = aucun.
    pub stun_server: String,
    /// Codec vidéo négocié avec le téléphone : "H264" seulement (décodage matériel VideoToolbox).
    pub video_codec: String,
    /// Profil H264 proposé (SDP `profile-level-id`) : "42e01f" (baseline, universel)
    /// ou "640c1f" (high, meilleure qualité à débit égal sur iPhone récent).
    pub h264_profile_level_id: String,
    /// Débit Opus vers le téléphone (retour audio), en bit/s.
    pub return_audio_bitrate: i32,
    /// Image-clé de sécurité demandée périodiquement au téléphone (s). Les images-clés sont
    /// normalement demandées à la demande (discontinuité détectée, plus d'images) ; ce filet
    /// lent borne toute corruption non détectée. 0 = désactivé.
    pub keyframe_interval_s: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8443".into(),
            cert_dir: "certs".into(),
            stun_server: "stun://stun.l.google.com:19302".into(),
            video_codec: "H264".into(),
            h264_profile_level_id: "42e01f".into(),
            return_audio_bitrate: 64000,
            keyframe_interval_s: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    /// Latence interne des mélangeurs (ms). Augmenter si des images sont perdues.
    pub mixer_latency_ms: u64,
    /// Lip-sync : retard d'affichage de la vidéo du téléphone (ms) pour l'aligner sur son
    /// audio, dont la lecture est tamponnée (~70 ms + une trame Opus). 0 = affichage au plus
    /// tôt (le son est alors légèrement en retard sur l'image). Ajuster à l'œil.
    pub av_offset_ms: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            mixer_latency_ms: 60,
            av_offset_ms: 80,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// Écran de sortie programme : sous-chaîne du nom de l'écran ("HDMI", "LG", ...)
    /// ou index numérique. Vide = écran secondaire s'il existe, sinon l'écran principal.
    pub display: String,
    pub fullscreen: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            display: String::new(),
            fullscreen: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MultiviewConfig {
    pub enabled: bool,
    pub width: i32,
    pub height: i32,
    pub columns: u32,
    /// Écran sur lequel ouvrir la fenêtre multiview (même syntaxe que output.display).
    pub display: String,
    /// Affiche l'horloge du Mac (UTC, ms) dans le bandeau de statistiques : avec la page
    /// `/latency` filmée par le téléphone, l'écart entre les deux horloges donne la latence
    /// verre à verre.
    pub clock: bool,
}

impl Default for MultiviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            width: 1280,
            height: 720,
            columns: 4,
            display: String::new(),
            clock: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub enabled: bool,
    /// Sortie (le son du téléphone est envoyé ici). "default" = sortie système,
    /// "none" = pas de sortie, sinon sous-chaîne du nom du périphérique ("WING").
    pub output_device: String,
    /// Entrée (retour envoyé au téléphone). Même syntaxe.
    pub input_device: String,
    /// Nombre de canaux du périphérique de sortie (0 = auto-détection).
    pub output_channels: i32,
    /// Nombre de canaux du périphérique d'entrée (0 = auto-détection).
    pub input_channels: i32,
    /// Canaux de sortie (1-based) recevant le son du stream WebRTC (téléphone).
    #[serde(alias = "phone_to_output_channels")]
    pub stream_output_channels: Vec<usize>,
    /// Canaux de sortie (1-based) recevant le son de l'habillage (vidéos d'overlay).
    pub branding_output_channels: Vec<usize>,
    /// Canaux d'entrée (1-based) renvoyés au téléphone (G, D).
    pub return_from_input_channels: Vec<usize>,
    pub sample_rate: i32,
    /// VU-mètres (élément `level`) sur chaque source, visibles dans le panneau de contrôle.
    pub meters: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            output_device: "default".into(),
            input_device: "default".into(),
            output_channels: 0,
            input_channels: 0,
            // Habillage stéréo → sorties 1/2 ; stream WebRTC stéréo → sorties 3/4.
            stream_output_channels: vec![3, 4],
            branding_output_channels: vec![1, 2],
            return_from_input_channels: vec![1],
            sample_rate: 48000,
            meters: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TransitionConfig {
    /// "cut" ou "fade".
    pub kind: String,
    pub duration_ms: u64,
}

impl Default for TransitionConfig {
    fn default() -> Self {
        Self {
            kind: "fade".into(),
            duration_ms: 400,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamDeckConfig {
    pub enabled: bool,
    /// Numéro de série du Stream Deck à utiliser (vide = le premier trouvé).
    pub serial: String,
    pub brightness: u8,
    /// Boutons. Si vide : une touche par scène dans l'ordre, puis une touche TAKE.
    pub buttons: Vec<ButtonConfig>,
}

impl Default for StreamDeckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            serial: String::new(),
            brightness: 60,
            buttons: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonConfig {
    /// Index de la touche (0 = en haut à gauche).
    pub index: u8,
    /// "program" (transition vers la scène), "cut" (bascule immédiate),
    /// "preview" (sélectionne en preview) ou "take" (preview → programme).
    pub action: String,
    pub scene: String,
    /// Texte affiché (défaut : nom de la scène).
    pub label: String,
    /// Couleur de fond au repos ("#RRGGBB").
    pub color: String,
}

impl Default for ButtonConfig {
    fn default() -> Self {
        Self {
            index: 0,
            action: "program".into(),
            scene: String::new(),
            label: String::new(),
            color: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SceneConfig {
    pub id: String,
    pub name: String,
    pub layers: Vec<LayerConfig>,
}

/// Géométrie d'un calque dans la scène (pixels). Absente = plein cadre.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LayerConfig {
    /// Le flux vidéo du téléphone.
    Phone {
        #[serde(flatten)]
        geometry: Geometry,
        #[serde(default = "one")]
        opacity: f64,
    },
    /// Aplat de couleur ("#RRGGBB" ou "#RRGGBBAA").
    Color {
        color: String,
        #[serde(flatten)]
        geometry: Geometry,
    },
    /// Image fixe (PNG avec transparence, JPEG...).
    Image {
        path: String,
        #[serde(flatten)]
        geometry: Geometry,
        #[serde(default = "one")]
        opacity: f64,
    },
    /// Fichier vidéo (mp4, mov, webm...). `loop` = lecture en boucle.
    Video {
        path: String,
        #[serde(default = "yes", rename = "loop")]
        looped: bool,
        #[serde(flatten)]
        geometry: Geometry,
        #[serde(default = "one")]
        opacity: f64,
    },
    /// Texte.
    Text {
        text: String,
        #[serde(default = "default_font")]
        font: String,
        #[serde(default = "white")]
        color: String,
        #[serde(flatten)]
        geometry: Geometry,
    },
}

fn one() -> f64 {
    1.0
}
fn yes() -> bool {
    true
}
fn default_font() -> String {
    "Sans Bold 48".into()
}
fn white() -> String {
    "#FFFFFF".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("lecture de la configuration {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("analyse de la configuration {}", path.display()))?;
        cfg.base_dir = path
            .parent()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    p.to_path_buf()
                }
            })
            .unwrap_or_else(|| PathBuf::from("."));
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&mut self) -> Result<()> {
        if self.scenes.is_empty() {
            self.scenes = Self::default_scenes();
        }
        for (i, s) in self.scenes.iter_mut().enumerate() {
            if s.id.is_empty() {
                s.id = format!("scene{}", i + 1);
            }
            if s.name.is_empty() {
                s.name = s.id.clone();
            }
        }
        let mut ids: Vec<&str> = self.scenes.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        anyhow::ensure!(
            ids.len() == self.scenes.len(),
            "identifiants de scènes en double"
        );
        anyhow::ensure!(
            self.video.width > 0 && self.video.height > 0 && self.video.fps > 0,
            "video.width/height/fps invalides"
        );
        match self.server.video_codec.to_ascii_uppercase().as_str() {
            "H264" => {}
            other => anyhow::bail!(
                "server.video_codec inconnu : {other} (seul H264 est décodé en matériel)"
            ),
        }
        if self.multiview.columns == 0 {
            self.multiview.columns = 4;
        }
        Ok(())
    }

    /// Résout un chemin relatif par rapport au dossier du fichier de configuration.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        }
    }

    pub fn default_scenes() -> Vec<SceneConfig> {
        vec![
            SceneConfig {
                id: "black".into(),
                name: "Noir".into(),
                layers: vec![LayerConfig::Color {
                    color: "#000000".into(),
                    geometry: Geometry::default(),
                }],
            },
            SceneConfig {
                id: "live".into(),
                name: "Direct".into(),
                layers: vec![LayerConfig::Phone {
                    geometry: Geometry::default(),
                    opacity: 1.0,
                }],
            },
        ]
    }

    /// Configuration d'exemple complète (écrite par `streame init`).
    pub fn example() -> Self {
        let mut cfg = Config::default();
        cfg.audio.output_device = "WING".into();
        cfg.audio.input_device = "WING".into();
        cfg.audio.branding_output_channels = vec![1, 2]; // habillage stéréo → sorties 1/2
        cfg.audio.stream_output_channels = vec![3, 4]; // stream WebRTC stéréo → sorties 3/4
        cfg.audio.return_from_input_channels = vec![1]; // entrée 1 → retour téléphone
        cfg.output.display = "HDMI".into();
        cfg.scenes = vec![
            SceneConfig {
                id: "black".into(),
                name: "Noir".into(),
                layers: vec![LayerConfig::Color {
                    color: "#000000".into(),
                    geometry: Geometry::default(),
                }],
            },
            SceneConfig {
                id: "live".into(),
                name: "Direct".into(),
                layers: vec![LayerConfig::Phone {
                    geometry: Geometry::default(),
                    opacity: 1.0,
                }],
            },
            SceneConfig {
                id: "branding".into(),
                name: "Habillage".into(),
                layers: vec![
                    LayerConfig::Phone {
                        geometry: Geometry::default(),
                        opacity: 1.0,
                    },
                    LayerConfig::Image {
                        path: "assets/lower-third.png".into(),
                        geometry: Geometry::default(),
                        opacity: 1.0,
                    },
                    LayerConfig::Text {
                        text: "EN DIRECT".into(),
                        font: "Sans Bold 40".into(),
                        color: "#FFFFFF".into(),
                        geometry: Geometry {
                            x: 120,
                            y: 900,
                            width: Some(600),
                            height: Some(90),
                        },
                    },
                ],
            },
            SceneConfig {
                id: "overlay".into(),
                name: "Overlay vidéo".into(),
                layers: vec![
                    LayerConfig::Phone {
                        geometry: Geometry::default(),
                        opacity: 1.0,
                    },
                    LayerConfig::Video {
                        path: "assets/overlay.mp4".into(),
                        looped: true,
                        geometry: Geometry {
                            x: 1280,
                            y: 60,
                            width: Some(576),
                            height: Some(324),
                        },
                        opacity: 0.9,
                    },
                ],
            },
        ];
        cfg.streamdeck.buttons = vec![
            ButtonConfig {
                index: 0,
                scene: "black".into(),
                ..Default::default()
            },
            ButtonConfig {
                index: 1,
                scene: "live".into(),
                ..Default::default()
            },
            ButtonConfig {
                index: 2,
                scene: "branding".into(),
                ..Default::default()
            },
            ButtonConfig {
                index: 3,
                scene: "overlay".into(),
                ..Default::default()
            },
            ButtonConfig {
                index: 4,
                action: "take".into(),
                label: "TAKE".into(),
                ..Default::default()
            },
        ];
        cfg
    }
}

/// Analyse "#RRGGBB" / "#RRGGBBAA" → (r, g, b, a).
pub fn parse_color(s: &str) -> Option<(u8, u8, u8, u8)> {
    let h = s.trim().trim_start_matches('#');
    let v = u32::from_str_radix(h, 16).ok()?;
    match h.len() {
        6 => Some((
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            (v & 0xff) as u8,
            0xff,
        )),
        8 => Some((
            ((v >> 24) & 0xff) as u8,
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            (v & 0xff) as u8,
        )),
        _ => None,
    }
}
