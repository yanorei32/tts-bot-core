use std::path::Path;

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[allow(clippy::upper_case_acronyms)]
#[derive(Deserialize, Debug)]
pub enum TtsServiceConfig {
    Voiceroid(crate::voiceroid::Setting),
    Voicevox(crate::voicevox::Setting),
    KTTS(crate::ktts::Setting),
    MiraeTTS(crate::mirae_tts::Setting),
    WinRTTTS(crate::winrttts::Setting),
    GoogleTranslate(crate::google_translate::Setting),
    Naver(crate::naver::Setting),
    BingSpeech(crate::bing_speech::Setting),
    CoefontTry(crate::coefont_try::Setting),
    CapCutTTSWrapper(crate::capcutttswrapper::Setting),
    AndroidTTS(crate::android_tts::Setting),
    OmniVoice(crate::omnivoice::Setting),
    SayServer(crate::sayserver::Setting),
    Volcengine(crate::volcengine::Setting),
}

#[derive(Deserialize, Debug)]
pub struct TtsConfig {
    pub default_style: TtsStyle,
    pub tts_services: IndexMap<String, TtsServiceConfig>,
    #[serde(default)]
    pub timestretch: Option<TimeStretchConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct TimeStretchConfig {
    pub target_speed: f64,
    pub ramp_duration: f64,
    pub initial_delay: f64,
}

impl Default for TimeStretchConfig {
    fn default() -> Self {
        Self {
            target_speed: 3.0,
            ramp_duration: 20.0,
            initial_delay: 10.0,
        }
    }
}

impl TtsConfig {
    pub fn new(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).context("Failed to read TtsConfig")?;
        toml::from_str(&s).context("Failed to parse TtsConfig")
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TtsStyle {
    pub service_id: String,
    pub style_id: String,
}
