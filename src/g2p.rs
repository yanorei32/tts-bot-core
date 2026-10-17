use std::collections::HashMap;

use anyhow::{Context, Result};
use indexmap::IndexMap;
use reqwest::header::{HeaderMap, HeaderName};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug, Clone)]
pub struct Setting {
    pub url: reqwest::Url,
    pub friendly_name: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub targets: Vec<Target>,
    /// Set when one host serves more than one conversion.
    #[serde(default)]
    pub style: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Target {
    /// 未指定のフィールドは任意の値に一致する。
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub speaker: Option<String>,
    #[serde(default)]
    pub style: Option<String>,
}

impl Target {
    fn matches(&self, service: &str, speaker: &str, style: &str) -> bool {
        // 何も指定していない target は何にも一致しない。
        if self.service.is_none() && self.speaker.is_none() && self.style.is_none() {
            return false;
        }

        self.service.as_deref().is_none_or(|value| value == service)
            && self.speaker.as_deref().is_none_or(|value| value == speaker)
            && self.style.as_deref().is_none_or(|value| value == style)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Engine {
    pub friendly_name: String,
    targets: Vec<Target>,
    style: Option<String>,
    endpoint: reqwest::Url,
    client: reqwest::Client,
}

impl Engine {
    pub(crate) fn new(setting: &Setting) -> Result<Self> {
        let mut headers = HeaderMap::new();
        for (key, value) in &setting.headers {
            headers.insert(
                HeaderName::from_bytes(key.as_bytes()).context("Invalid HeaderName")?,
                value.parse().context("Invalid HeaderValue")?,
            );
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent("discord-tts-g2p/0.0.0")
            .build()
            .context("Failed to build G2P client")?;

        let mut endpoint = setting.url.clone();
        endpoint
            .path_segments_mut()
            .map_err(|()| anyhow::anyhow!("G2P url is not a base"))?
            .extend(["api", "g2p"]);

        Ok(Self {
            friendly_name: setting.friendly_name.clone(),
            targets: setting.targets.clone(),
            style: setting.style.clone(),
            endpoint,
            client,
        })
    }

    pub(crate) fn matches(&self, service: &str, speaker: &str, style: &str) -> bool {
        self.targets
            .iter()
            .any(|target| target.matches(service, speaker, style))
    }

    pub(crate) async fn convert(&self, text: &str) -> Result<String> {
        let response: G2pResponse = self
            .client
            .post(self.endpoint.clone())
            .json(&G2pRequest {
                text,
                style: self.style.as_deref(),
            })
            .send()
            .await
            .context("Failed to post /api/g2p (connect)")?
            .error_for_status()
            .context("Failed to post /api/g2p (status_code)")?
            .json()
            .await
            .context("Failed to parse as json /api/g2p")?;

        Ok(response.text)
    }
}

pub(crate) fn matching<'a>(
    engines: &'a IndexMap<String, Engine>,
    service: &str,
    speaker: &str,
    style: &str,
) -> Vec<(&'a str, &'a Engine)> {
    engines
        .iter()
        .filter(|(_, engine)| engine.matches(service, speaker, style))
        .map(|(id, engine)| (id.as_str(), engine))
        .collect()
}

#[derive(Serialize)]
struct G2pRequest<'a> {
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    style: Option<&'a str>,
}

#[derive(Deserialize)]
struct G2pResponse {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::{Engine, Setting, Target, matching};

    fn setting(friendly_name: &str, targets: Vec<Target>) -> Setting {
        Setting {
            url: "https://example.test".parse().unwrap(),
            friendly_name: friendly_name.to_string(),
            headers: std::collections::HashMap::new(),
            targets,
            style: None,
        }
    }

    fn target(service: Option<&str>, speaker: Option<&str>, style: Option<&str>) -> Target {
        Target {
            service: service.map(str::to_string),
            speaker: speaker.map(str::to_string),
            style: style.map(str::to_string),
        }
    }

    fn engine(friendly_name: &str, targets: Vec<Target>) -> Engine {
        Engine::new(&setting(friendly_name, targets)).unwrap()
    }

    #[test]
    fn matching_keeps_config_order() {
        let mut engines = indexmap::IndexMap::new();
        engines.insert(
            "jp2hangul".to_string(),
            engine(
                "G2P",
                vec![
                    target(Some("chongbong"), Some("Default"), Some("Default")),
                    target(Some("google"), Some("Google Translate"), Some("ko")),
                ],
            ),
        );
        engines.insert(
            "jp2chinese".to_string(),
            engine(
                "J2C",
                vec![target(
                    Some("google"),
                    Some("Google Translate"),
                    Some("zh-CN"),
                )],
            ),
        );

        let ids: Vec<_> = matching(&engines, "google", "Google Translate", "ko")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["jp2hangul"]);

        let ids: Vec<_> = matching(&engines, "google", "Google Translate", "zh-CN")
            .into_iter()
            .map(|(id, engine)| (id, engine.friendly_name.as_str()))
            .collect();
        assert_eq!(ids, [("jp2chinese", "J2C")]);
    }

    #[test]
    fn unspecified_fields_act_as_wildcards() {
        let engines = indexmap::IndexMap::from([(
            "all-hangul".to_string(),
            engine("G2P", vec![target(Some("chongbong"), None, None)]),
        )]);

        let ids: Vec<_> = matching(&engines, "chongbong", "Default", "Default")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, ["all-hangul"]);

        // service が違えば、他が未指定でも一致しない。
        let ids = matching(&engines, "mirae", "Default", "Default");
        assert!(ids.is_empty());
    }

    #[test]
    fn target_without_any_field_matches_nothing() {
        let engines = indexmap::IndexMap::from([(
            "inert".to_string(),
            engine("G2P", vec![target(None, None, None)]),
        )]);

        for (service, speaker, style) in [
            ("chongbong", "Default", "Default"),
            ("google", "Google Translate", "ko"),
        ] {
            let ids = matching(&engines, service, speaker, style);
            assert!(ids.is_empty(), "{service} / {speaker} / {style}");
        }
    }

    #[test]
    fn endpoint_is_api_g2p() {
        let engine = engine("G2P", vec![]);
        assert_eq!(engine.endpoint.as_str(), "https://example.test/api/g2p");
    }

    #[test]
    fn config_loads_g2p_engines_in_order() {
        let config: crate::model::TtsConfig = toml::from_str(
            r#"
default_style = { service_id = "chongbong", style_id = "Default" }
tts_services = {}

[g2p_engines.jp2hangul]
url = "https://jp2hangul.yr32.net"
friendly_name = "G2P"

[g2p_engines.jp2hangul.headers]
Authorization = "Bot 123"

[[g2p_engines.jp2hangul.targets]]
service = "chongbong"
speaker = "Default"
style = "Default"

[g2p_engines.jp2chinese]
url = "https://jp2chinese.yr32.net"
friendly_name = "J2C"

[[g2p_engines.jp2chinese.targets]]
service = "Google Translate"
speaker = "Google Translate"
style = "zh-CN"

[[g2p_engines.jp2chinese.targets]]
service = "Google Translate"
"#,
        )
        .unwrap();

        let ids: Vec<_> = config.g2p_engines.keys().map(String::as_str).collect();
        assert_eq!(ids, ["jp2hangul", "jp2chinese"]);
        assert_eq!(
            config.g2p_engines["jp2hangul"].headers["Authorization"],
            "Bot 123"
        );
        assert!(config.g2p_engines["jp2chinese"].style.is_none());
        assert_eq!(
            config.g2p_engines["jp2chinese"].targets[1]
                .service
                .as_deref(),
            Some("Google Translate")
        );
        assert!(
            config.g2p_engines["jp2chinese"].targets[1]
                .speaker
                .is_none()
        );
        assert!(config.g2p_engines["jp2chinese"].targets[1].style.is_none());
    }
}
