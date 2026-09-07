use crate::{
    args::Interface,
    error::ImageAnalysisError,
    utils::{format_error_chain, read_image_as_base64},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use log::{debug, error, info, warn};
use reqwest::Client;
use serde_json::Value;
use std::{
    collections::HashMap,
    num::NonZeroU32,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

impl Interface {
    /// Returns the API endpoint path for the given interface.
    #[inline]
    pub const fn endpoint(self) -> &'static str {
        match self {
            Self::Ollama => "/api/chat",
            Self::Llamacpp => "/v1/chat/completions",
        }
    }

    /// Returns `true` if the interface supports Bearer token authentication.
    #[inline]
    pub const fn supports_bearer_auth(self) -> bool {
        match self {
            Self::Ollama => false,
            Self::Llamacpp => true,
        }
    }

    /// Parses the response JSON and extracts the content string for the given interface.
    pub fn parse_response(self, json_value: &Value) -> Option<&str> {
        match self {
            Self::Ollama => json_value
                .get("message")
                .and_then(|msg| msg.get("content"))
                .and_then(|content| content.as_str()),
            Self::Llamacpp => json_value
                .get("choices")
                .and_then(|choices| choices.as_array())
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("message"))
                .and_then(|msg| msg.get("content"))
                .and_then(|content| content.as_str()),
        }
    }

    /// Builds the JSON request body specific to the AI service interface.
    pub fn build_request_body(self, model_name: &str, prompt: &str, base64_image: &str) -> Value {
        match self {
            Self::Ollama => serde_json::json!({
                "model": model_name,
                "messages": [
                    {
                        "role": "user",
                        "content": prompt,
                        "images": [base64_image]
                    }
                ],
                "stream": false,
            }),
            Self::Llamacpp => serde_json::json!({
                "model": model_name,
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "text",
                                "text": prompt
                            },
                            {
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:image/jpeg;base64,{}", base64_image)
                                }
                            }
                        ]
                    }
                ],
                "stream": false,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostManager {
    hosts: Vec<String>,
    interface: Interface,
    client: Client,
    model_name: String,
    timeout: u64,
    max_retries: Option<NonZeroU32>,
    retry_delay: Duration,
    unavailable_hosts: Arc<Mutex<HashMap<String, Instant>>>,
    unavailable_duration: Duration,
    api_key: Option<String>,
}

impl HostManager {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        hosts: Vec<String>,
        interface: Interface,
        client: Client,
        model_name: String,
        timeout: u64,
        max_retries: Option<NonZeroU32>,
        retry_delay: Duration,
        unavailable_duration: Duration,
        api_key: Option<String>,
    ) -> Self {
        Self {
            hosts,
            interface,
            client,
            model_name,
            timeout,
            max_retries,
            retry_delay,
            unavailable_hosts: Arc::new(Mutex::new(HashMap::new())),
            unavailable_duration,
            api_key,
        }
    }

    pub fn get_available_host(&self) -> Result<String, ImageAnalysisError> {
        debug!(
            "Looking for available {:?} hosts. Total hosts: {}",
            self.interface,
            self.hosts.len()
        );

        let mut unavailable = self
            .unavailable_hosts
            .lock()
            .expect("unavailable_hosts mutex poisoned");
        let now = Instant::now();
        let original_count = unavailable.len();
        unavailable
            .retain(|_, timestamp| now.duration_since(*timestamp) < self.unavailable_duration);

        if let Some(removed_count) = original_count.checked_sub(unavailable.len())
            && removed_count > 0
        {
            debug!("Cleaned up {removed_count} expired unavailable hosts");
        }

        debug!(
            "Currently unavailable hosts: {:?}",
            unavailable.keys().collect::<Vec<_>>()
        );

        for host in &self.hosts {
            if !unavailable.contains_key(host) {
                info!("Selected available {:?} host: {}", self.interface, host);
                return Ok(host.clone());
            }
        }

        if let Some((host, timestamp)) = unavailable.iter().min_by_key(|(_, timestamp)| *timestamp)
        {
            warn!(
                "All {:?} hosts unavailable. Using oldest unavailable host: {} (unavailable for {:?})",
                self.interface,
                host,
                now.duration_since(*timestamp)
            );
            return Ok(host.clone());
        }
        drop(unavailable);

        error!("No {:?} hosts available at all", self.interface);
        Err(ImageAnalysisError::AllHostsUnavailable)
    }

    pub fn mark_host_unavailable(&self, host: &str) {
        self.unavailable_hosts
            .lock()
            .expect("unavailable_hosts mutex poisoned")
            .insert(host.to_owned(), Instant::now());
        println!(
            "{}",
            rust_i18n::t!("host_manager.host_marked_unavailable", host = host)
        );
    }

    pub async fn analyze_image(
        &self,
        asset_id: uuid::Uuid,
        image_path: &Path,
        prompt: &str,
    ) -> Result<crate::database::ImageAnalysisResult, ImageAnalysisError> {
        let filename = asset_id.to_string();

        info!(
            "Starting {:?} analysis for image: {}",
            self.interface, filename
        );
        debug!("Model: {}, Timeout: {}s", self.model_name, self.timeout);

        let base64_image = read_image_as_base64(image_path, &filename).await?;
        let request_body =
            self.interface
                .build_request_body(&self.model_name, prompt, &base64_image);

        self.execute_request(asset_id, &filename, &request_body, false)
            .await
    }

    pub async fn analyze_video_chunk(
        &self,
        asset_id: uuid::Uuid,
        clip_path: &Path,
        prompt: &str,
        has_audio: bool,
    ) -> Result<crate::database::ImageAnalysisResult, ImageAnalysisError> {
        const MAX_VIDEO_CLIP_BYTES: u64 = 16 * 1024 * 1024;

        let filename = asset_id.to_string();

        info!(
            "Starting {:?} analysis for video: {}",
            self.interface, filename
        );
        debug!("Model: {}, Timeout: {}s", self.model_name, self.timeout);

        let metadata = tokio::fs::metadata(clip_path).await.map_err(|err| {
            ImageAnalysisError::ProcessingError {
                filename: filename.clone(),
                error: format_error_chain(&err),
            }
        })?;
        if metadata.len() == 0 {
            return Err(ImageAnalysisError::EmptyFile {
                filename: filename.clone(),
            });
        }
        if metadata.len() > MAX_VIDEO_CLIP_BYTES {
            return Err(ImageAnalysisError::ProcessingError {
                filename: filename.clone(),
                error: format!(
                    "Video clip is {} bytes, which exceeds the {}-byte limit",
                    metadata.len(),
                    MAX_VIDEO_CLIP_BYTES
                ),
            });
        }

        let clip_bytes = tokio::fs::read(clip_path).await.map_err(|err| {
            ImageAnalysisError::ProcessingError {
                filename: filename.clone(),
                error: format_error_chain(&err),
            }
        })?;
        if clip_bytes.is_empty() {
            return Err(ImageAnalysisError::EmptyFile {
                filename: filename.clone(),
            });
        }
        if clip_bytes.len() as u64 > MAX_VIDEO_CLIP_BYTES {
            return Err(ImageAnalysisError::ProcessingError {
                filename: filename.clone(),
                error: format!(
                    "Video clip is {} bytes, which exceeds the {}-byte limit",
                    clip_bytes.len(),
                    MAX_VIDEO_CLIP_BYTES
                ),
            });
        }

        let request_body = serde_json::json!({
            "model": self.model_name.as_str(),
            "messages": [{
                "role": "user",
                "content": [
                    {
                        "type": "video_url",
                        "video_url": {
                            "url": format!("data:video/mp4;base64,{}", STANDARD.encode(&clip_bytes))
                        }
                    },
                    {
                        "type": "text",
                        "text": prompt,
                    }
                ]
            }],
            "modalities": ["text"],
            "media_io_kwargs": {
                "video": {
                    "video_backend": "opencv",
                    "backend": "pyav",
                    "num_frames": 32_u32,
                    "fps": -1_i32,
                }
            },
            "mm_processor_kwargs": {
                "use_audio_in_video": has_audio,
            },
            "max_tokens": 512_u32,
            "stream": false,
        });
        drop(clip_bytes);

        self.execute_request(asset_id, &filename, &request_body, true)
            .await
    }

    async fn execute_request(
        &self,
        asset_id: uuid::Uuid,
        filename: &str,
        request_body: &Value,
        reject_truncated: bool,
    ) -> Result<crate::database::ImageAnalysisResult, ImageAnalysisError> {
        let endpoint = self.interface.endpoint();
        let mut attempt: u32 = 0;
        let mut last_error = None;

        loop {
            attempt = attempt.saturating_add(1);

            if self.max_retries.is_some() || attempt > 1 {
                info!(
                    "Retry attempt {}/{} for asset {}",
                    attempt,
                    self.max_retries
                        .map_or_else(|| "∞".to_owned(), |max| max.to_string()),
                    filename
                );
            }

            for _ in 0..self.hosts.len() {
                let host = match self.get_available_host() {
                    Ok(host) => host,
                    Err(err) => {
                        error!(
                            "Failed to get available {:?} host: {:?}",
                            self.interface, err
                        );
                        return Err(err);
                    }
                };

                let url = format!("{}{}", host.trim_end_matches('/'), endpoint);
                info!("Making {:?} request to: {}", self.interface, url);

                let mut request = self.client.post(&url).json(request_body);

                if self.interface.supports_bearer_auth() {
                    if let Some(api_key) = &self.api_key {
                        debug!("Adding Authorization header with API key");
                        request = request.header("Authorization", format!("Bearer {api_key}"));
                    } else {
                        debug!("No API key provided for {:?} request", self.interface);
                    }
                }

                match tokio::time::timeout(
                    Duration::from_secs(self.timeout.saturating_add(1)),
                    async {
                        debug!("Sending {:?} request...", self.interface);
                        request.send().await
                    },
                )
                .await
                {
                    Ok(Ok(response)) => {
                        let status = response.status();
                        debug!(
                            "Received {:?} response: {} {}",
                            self.interface,
                            status.as_u16(),
                            status.canonical_reason().unwrap_or("")
                        );

                        if response.status().is_success() {
                            let response_text = response.text().await.map_err(|err| {
                                error!("Failed to read response body: {err}");
                                ImageAnalysisError::ProcessingError {
                                    filename: filename.to_owned(),
                                    error: format_error_chain(&err),
                                }
                            })?;

                            debug!("Response body length: {} chars", response_text.len());

                            match serde_json::from_str::<Value>(&response_text) {
                                Ok(json_value) => {
                                    if reject_truncated
                                        && matches!(
                                            Self::response_finish_reason(&json_value),
                                            Some("length")
                                        )
                                    {
                                        return Err(ImageAnalysisError::ProcessingError {
                                            filename: filename.to_owned(),
                                            error:
                                                "AI response was truncated (finish_reason=length)"
                                                    .to_owned(),
                                        });
                                    }

                                    let content = self.interface.parse_response(&json_value);

                                    if let Some(raw_description) = content {
                                        let description = raw_description.trim().to_owned();
                                        if description.is_empty() {
                                            warn!("Empty response for asset: {filename}");
                                            last_error = Some(ImageAnalysisError::EmptyResponse {
                                                filename: filename.to_owned(),
                                            });
                                        } else {
                                            info!(
                                                "{:?} analysis successful for {}, description length: {}",
                                                self.interface,
                                                filename,
                                                description.len()
                                            );
                                            return Ok(crate::database::ImageAnalysisResult {
                                                description,
                                                asset_id,
                                            });
                                        }
                                    } else {
                                        error!(
                                            "Failed to extract content from response for {filename}"
                                        );
                                        last_error = Some(ImageAnalysisError::JsonParsing {
                                            filename: filename.to_owned(),
                                            error: "No content field found in response".to_owned(),
                                        });
                                    }
                                }
                                Err(parse_error) => {
                                    error!(
                                        "Failed to parse response as JSON for {filename}: {parse_error}"
                                    );
                                    let error = ImageAnalysisError::JsonParsing {
                                        filename: filename.to_owned(),
                                        error: format_error_chain(&parse_error),
                                    };
                                    if !error.is_retryable() {
                                        return Err(error);
                                    }
                                    last_error = Some(error);
                                }
                            }
                        } else {
                            let status = response.status().as_u16();
                            let response_text = response.text().await.unwrap_or_default();
                            error!(
                                "{:?} HTTP error {} for {}: {}",
                                self.interface, status, filename, response_text
                            );
                            let error = ImageAnalysisError::HttpError {
                                status,
                                filename: filename.to_owned(),
                                response: response_text,
                            };
                            if !error.is_retryable() {
                                return Err(error);
                            }
                            last_error = Some(error);
                        }
                    }
                    Ok(Err(err)) => {
                        error!(
                            "{:?} request failed for {}: {}",
                            self.interface, filename, err
                        );
                        last_error = Some(ImageAnalysisError::HttpError {
                            status: 0,
                            filename: filename.to_owned(),
                            response: format_error_chain(&err),
                        });
                    }
                    Err(_) => {
                        last_error = Some(ImageAnalysisError::AiRequestTimeout);
                    }
                }
                warn!(
                    "Marking {:?} host as unavailable due to error: {}",
                    self.interface, host
                );
                self.mark_host_unavailable(&host);
            }

            if let Some(last_err) = &last_error
                && !last_err.is_retryable()
            {
                return Err(last_err.clone());
            }

            if self.max_retries.is_none_or(|max| attempt < max.get()) {
                info!(
                    "All hosts failed for {}, waiting {}s before retry",
                    filename,
                    self.retry_delay.as_secs()
                );
                tokio::time::sleep(self.retry_delay).await;
            } else {
                break;
            }
        }

        Err(last_error.unwrap_or(ImageAnalysisError::AllHostsUnavailable))
    }

    fn response_finish_reason(json_value: &Value) -> Option<&str> {
        json_value
            .get("choices")
            .and_then(|choices| choices.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(Value::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::{HostManager, Interface};
    use crate::error::ImageAnalysisError;
    use std::{error::Error, num::NonZeroU32, time::Duration};
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    #[tokio::test]
    async fn truncated_video_is_rejected_without_changing_image_completion_behavior()
    -> Result<(), Box<dyn Error>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let host = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            for _ in 0..2_u8 {
                let (stream, _) = listener.accept().await?;
                let mut reader = BufReader::new(stream);
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await? == 0 {
                        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value
                            .trim()
                            .parse::<usize>()
                            .map_err(std::io::Error::other)?;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await?;
                let response_body = r#"{"choices":[{"message":{"content":"partial model output"},"finish_reason":"length"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                reader.get_mut().write_all(response.as_bytes()).await?;
                reader.get_mut().shutdown().await?;
            }
            Ok::<(), std::io::Error>(())
        });
        let manager = HostManager::new(
            vec![host],
            Interface::Llamacpp,
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()?,
            "fixture".to_owned(),
            2,
            NonZeroU32::new(1),
            Duration::ZERO,
            Duration::ZERO,
            None,
        );
        let media = tempfile::NamedTempFile::new()?;
        tokio::fs::write(media.path(), b"transport fixture bytes").await?;
        let asset_id = Uuid::from_u128(42);

        let video = manager
            .analyze_video_chunk(asset_id, media.path(), "Describe", true)
            .await;
        assert!(matches!(
            video,
            Err(ImageAnalysisError::ProcessingError { .. })
        ));
        let image = manager
            .analyze_image(asset_id, media.path(), "Describe")
            .await?;
        assert_eq!(image.asset_id, asset_id);
        assert_eq!(image.description, "partial model output");
        tokio::time::timeout(Duration::from_secs(5), server).await???;
        Ok(())
    }
}
