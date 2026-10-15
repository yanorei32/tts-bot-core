use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use reqwest::{
    Url,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use ssml::Serialize as SsmlSerialize;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{debug, warn};
use tungstenite::client::IntoClientRequest;
use uuid::Uuid;

use super::sec_ms_gec::{self, SEC_MS_GEC_VERSION, TRUSTED_CLIENT_TOKEN};

const BING_SPEECH_MAX_CHARS: usize = 1000;

/// Truncate payloads so log lines stay readable.
fn preview(value: &str) -> String {
    const LIMIT: usize = 200;

    let head: String = value.chars().take(LIMIT).collect();
    if value.chars().nth(LIMIT).is_some() {
        format!("{head}… ({} bytes total)", value.len())
    } else {
        head
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "PascalCase")]
pub struct Voice {
    pub name: String,
    pub short_name: String,
    pub gender: String,
    pub locale: String,
    pub friendly_name: String,
}

fn generate_sec_ms_gec() -> String {
    let since_the_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");

    sec_ms_gec::generate(since_the_epoch.as_secs())
}

pub fn edgenize(headers: &mut HeaderMap) {
    headers.insert(
        "Authority",
        HeaderValue::from_static("speech.platform.bing.com"),
    );
    headers.insert(
        "Sec-CH-UA",
        HeaderValue::from_static(
            "\" Not;A Brand\";v=\"99\", \"Microsoft Edge\";v=\"130\", \"Chromium\";v=\"130\"",
        ),
    );
    headers.insert("Sec-CH-UA-Mobile", HeaderValue::from_static("?0"));
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Sec-Fetch-Site", HeaderValue::from_static("none"));
    headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("cors"));
    headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("empty"));
    headers.insert(
        "User-Agent",
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36 Edg/145.0.0.0",
        ),
    );
    headers.insert(
        "Accept-Encoding",
        HeaderValue::from_static("gzip, deflate, br"),
    );
    headers.insert(
        "Accept-Language",
        HeaderValue::from_static("en-US,en;q=0.9"),
    );
}

pub async fn list_voices() -> Result<Vec<Voice>> {
    let started = Instant::now();
    let url = format!(
        "https://speech.platform.bing.com/consumer/speech/synthesize/readaloud/voices/list?trustedclienttoken={}&Sec-MS-GEC={}&Sec-MS-GEC-Version={}",
        TRUSTED_CLIENT_TOKEN,
        generate_sec_ms_gec(),
        SEC_MS_GEC_VERSION
    );

    debug!(url, "bing speech: requesting voice list");

    let mut headers = HeaderMap::new();
    edgenize(&mut headers);

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .headers(headers)
        .send()
        .await
        .with_context(|| format!("bing speech: voice list request failed for {url}"))?;

    let status = resp.status();
    let resp = resp
        .error_for_status()
        .with_context(|| format!("bing speech: voice list returned {status}"))?;
    debug!(
        status = status.as_u16(),
        "bing speech: voice list response received"
    );

    let voices: Vec<Voice> = resp.json().await.with_context(|| {
        format!("bing speech: voice list response is not valid json ({status})")
    })?;

    if voices.is_empty() {
        warn!("bing speech: voice list is empty");
    }
    debug!(
        voices = voices.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "bing speech: voice list ready"
    );

    Ok(voices)
}

fn create_empty_wav() -> Result<Vec<u8>> {
    use std::io::Cursor;

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 24000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav_cursor = Cursor::new(Vec::new());
    let wav_writer = hound::WavWriter::new(&mut wav_cursor, spec)?;
    wav_writer.finalize()?;
    Ok(wav_cursor.into_inner())
}

fn combine_audio_parts(results: Vec<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    let total_parts = results.len();
    anyhow::ensure!(total_parts > 0, "bing speech: no audio parts received");

    let mut combined_audio = Vec::new();
    for (index, result) in results.into_iter().enumerate() {
        let part_number = index + 1;
        let audio = result.with_context(|| {
            format!("bing speech: synthesis failed for chunk {part_number}/{total_parts}")
        })?;
        anyhow::ensure!(
            !audio.is_empty(),
            "bing speech: no audio received for chunk {part_number}/{total_parts}"
        );
        combined_audio.extend(audio);
    }

    Ok(combined_audio)
}

pub async fn get_audio_bytes(
    text: &str,
    voice: &str,
    locale: &str,
    volume: f32,
) -> Result<Vec<u8>> {
    if text.trim().is_empty() {
        return create_empty_wav().context("bing speech: failed to build the empty wav");
    }

    let started = Instant::now();
    let parts = crate::tts::split_long_text(text, BING_SPEECH_MAX_CHARS);

    debug!(
        voice,
        locale,
        volume,
        text_chars = text.chars().count(),
        text = preview(text),
        parts = parts.len(),
        "bing speech: synthesis requested"
    );

    let futures: Vec<_> = parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let voice = voice.to_string();
            let locale = locale.to_string();
            async move {
                let part_started = Instant::now();
                let text = preview(&part);
                let chars = part.chars().count();

                debug!(index, chars, text, "bing speech: fetching part");

                let result = fetch_audio_part(part, voice, locale).await;
                match &result {
                    Ok(audio) => debug!(
                        index,
                        chars,
                        text,
                        mp3_bytes = audio.len(),
                        elapsed_ms = part_started.elapsed().as_millis(),
                        "bing speech: part received"
                    ),
                    Err(error) => warn!(
                        index,
                        chars,
                        text,
                        elapsed_ms = part_started.elapsed().as_millis(),
                        error = ?error,
                        "bing speech: part failed"
                    ),
                }
                result
            }
        })
        .collect();

    let results = futures::future::join_all(futures).await;
    let combined_audio = combine_audio_parts(results)?;

    let mp3_bytes = combined_audio.len();
    debug!(mp3_bytes, "bing speech: converting mp3 to wav");

    let wav = crate::tts::convert_mp3_to_wav(combined_audio, volume)
        .context("bing speech: failed to convert the mp3 stream to wav")?;

    debug!(
        mp3_bytes,
        wav_bytes = wav.len(),
        elapsed_ms = started.elapsed().as_millis(),
        "bing speech: synthesis done"
    );

    Ok(wav)
}

async fn fetch_audio_part(part: String, voice: String, locale: String) -> Result<Vec<u8>> {
    let started = Instant::now();
    let text = preview(&part);

    let url = Url::parse(&format!(
        "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1?TrustedClientToken={}&Sec-MS-GEC={}&Sec-MS-GEC-Version={}",
        TRUSTED_CLIENT_TOKEN,
        generate_sec_ms_gec(),
        SEC_MS_GEC_VERSION
    ))
    .context("bing speech: failed to build the websocket url")?;

    debug!(
        url = url.as_str(),
        voice, locale, text, "bing speech: connecting"
    );

    let mut request = url
        .to_string()
        .into_client_request()
        .context("bing speech: failed to build the websocket request")?;
    edgenize(request.headers_mut());

    let (mut ws_stream, response) = connect_async(request)
        .await
        .with_context(|| format!("bing speech: websocket handshake failed for {url}"))?;
    debug!(
        status = response.status().as_u16(),
        headers = ?response.headers(),
        "bing speech: websocket connected"
    );

    let config = "Content-Type:application/json; charset=utf-8\r\n\
            Path:speech.config\r\n\r\n\
            {\"context\":{\"synthesis\":{\"audio\":{\"outputFormat\":\"audio-24khz-48kbitrate-mono-mp3\"}}}}";
    debug!(payload = config, "bing speech: sending speech.config");

    ws_stream
        .send(Message::Text(config.to_string().into()))
        .await
        .context("bing speech: failed to send speech.config")?;

    let locale_regex = regex::Regex::new(r"^[a-z]{2}-[A-Z]{2}$").unwrap();
    let locale_option = if locale_regex.is_match(&locale) {
        Some(locale.as_str())
    } else {
        debug!(
            locale,
            "bing speech: locale is not a valid tag, omitting xml:lang"
        );
        None
    };

    let doc = ssml::speak(locale_option, [ssml::voice(&voice, [part])]);
    let ssml_string = doc
        .serialize_to_string(&ssml::SerializeOptions::default())
        .context("bing speech: failed to serialize the ssml document")?;

    let request_id = Uuid::new_v4().simple();
    debug!(
        request_id = request_id.to_string(),
        ssml = preview(&ssml_string),
        "bing speech: sending ssml"
    );

    ws_stream
        .send(Message::Text(
            format!(
                "X-RequestId:{}\r\n\
            Content-Type:application/ssml+xml\r\n\
            Path:ssml\r\n\r\n\
            {}",
                request_id, ssml_string
            )
            .into(),
        ))
        .await
        .context("bing speech: failed to send ssml")?;

    let mut audio_data = Vec::new();
    let mut messages = 0;
    let mut audio_messages = 0;
    let mut turn_ended = false;

    while let Some(message) = ws_stream.next().await {
        let message = message.context("bing speech: failed to receive a websocket message")?;
        messages += 1;

        match message {
            Message::Text(payload) => {
                debug!(
                    messages,
                    payload = preview(&payload),
                    "bing speech: text message"
                );
                if payload.contains("Path:turn.end") {
                    turn_ended = true;
                    break;
                }
            }
            Message::Binary(data) => {
                debug!(messages, bytes = data.len(), "bing speech: binary message");

                if data.len() < 2 {
                    warn!(
                        messages,
                        bytes = data.len(),
                        "bing speech: binary message is too short"
                    );
                    continue;
                }

                let header_len = u16::from_be_bytes([data[0], data[1]]) as usize;
                if data.len() < header_len + 2 {
                    warn!(
                        messages,
                        bytes = data.len(),
                        header_len,
                        "bing speech: binary message is shorter than its header"
                    );
                    continue;
                }

                let headers = String::from_utf8_lossy(&data[2..2 + header_len]).to_string();
                if headers.contains("Path:audio") && headers.contains("Content-Type:audio/mpeg") {
                    let chunk = &data[2 + header_len..];
                    audio_messages += 1;
                    audio_data.extend_from_slice(chunk);
                    debug!(
                        messages,
                        chunk_bytes = chunk.len(),
                        mp3_bytes = audio_data.len(),
                        "bing speech: audio chunk"
                    );
                } else {
                    debug!(
                        messages,
                        headers = preview(&headers),
                        "bing speech: skipped binary message"
                    );
                }
            }
            Message::Close(frame) => {
                debug!(messages, frame = ?frame, "bing speech: websocket closed");
                break;
            }
            other => debug!(messages, message = ?other, "bing speech: other message"),
        }
    }

    if !turn_ended {
        warn!(
            messages,
            audio_messages,
            mp3_bytes = audio_data.len(),
            "bing speech: stream ended without Path:turn.end"
        );
    }

    if audio_data.is_empty() {
        warn!(
            messages,
            elapsed_ms = started.elapsed().as_millis(),
            "bing speech: no audio bytes received"
        );
    } else {
        debug!(
            messages,
            audio_messages,
            mp3_bytes = audio_data.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "bing speech: stream done"
        );
    }

    Ok(audio_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_combine_audio_parts_preserves_order() {
        let audio = combine_audio_parts(vec![Ok(vec![1, 2]), Ok(vec![3]), Ok(vec![4, 5])]).unwrap();

        assert_eq!(audio, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_combine_audio_parts_rejects_failed_first_chunk() {
        let error = combine_audio_parts(vec![
            Err(anyhow::anyhow!("HTTP 403")),
            Ok(vec![2]),
            Ok(vec![3]),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("chunk 1/3"));
        assert_eq!(error.root_cause().to_string(), "HTTP 403");
    }

    #[test]
    fn test_combine_audio_parts_rejects_failed_middle_chunk_and_preserves_error_chain() {
        let source = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ))
        .context("websocket handshake failed");
        let error = combine_audio_parts(vec![Ok(vec![1]), Err(source), Ok(vec![3])]).unwrap_err();

        assert_eq!(
            error.chain().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                "bing speech: synthesis failed for chunk 2/3",
                "websocket handshake failed",
                "connection refused",
            ]
        );
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::ConnectionRefused
        );
    }

    #[test]
    fn test_combine_audio_parts_rejects_all_failed_chunks() {
        let error = combine_audio_parts(vec![
            Err(anyhow::anyhow!("first chunk failed")),
            Err(anyhow::anyhow!("second chunk failed")),
        ])
        .unwrap_err();

        assert!(error.to_string().contains("chunk 1/2"));
        assert_eq!(error.root_cause().to_string(), "first chunk failed");
    }

    #[test]
    fn test_combine_audio_parts_rejects_empty_audio() {
        for index in 0..3 {
            let mut results = vec![Ok(vec![1]), Ok(vec![2]), Ok(vec![3])];
            results[index] = Ok(Vec::new());
            let error = combine_audio_parts(results).unwrap_err();

            assert!(
                error
                    .to_string()
                    .contains(&format!("chunk {}/3", index + 1))
            );
            assert!(error.to_string().contains("no audio received"));
        }
    }

    #[test]
    fn test_combine_audio_parts_rejects_no_parts() {
        assert!(combine_audio_parts(Vec::new()).is_err());
    }

    #[test]
    fn test_get_audio_bytes_returns_empty_wav_for_blank_text() {
        let expected = create_empty_wav().unwrap();
        for text in ["", " \t\r\n", "\u{3000}\u{00a0}"] {
            let wav = futures::executor::block_on(get_audio_bytes(text, "", "", 1.0)).unwrap();

            assert_eq!(wav, expected);
            let reader = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
            assert_eq!(reader.len(), 0);
            assert_eq!(reader.spec().channels, 1);
            assert_eq!(reader.spec().sample_rate, 24000);
            assert_eq!(reader.spec().bits_per_sample, 16);
        }
    }
}
