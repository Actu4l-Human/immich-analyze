use crate::{
    args::{Args, Interface},
    data_access::DataAccess,
    database::ImageAnalysisResult,
    error::ImageAnalysisError,
    health::mark_activity,
    host_manager::HostManager,
    utils::format_error_chain,
};
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::VecDeque, fmt::Write as _, io, num::NonZeroU32, path::Path, process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
    sync::Semaphore,
};
use uuid::Uuid;

const SEGMENT_MILLIS: u64 = 30_000;
const MAX_CLIP_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PROBE_BYTES: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

pub struct VideoAnalyzer {
    host_manager: HostManager,
    prompt: String,
    max_bytes: u64,
    timeout: Duration,
    permits: Semaphore,
}

impl VideoAnalyzer {
    pub async fn from_args(
        args: &Args,
        client: Client,
    ) -> Result<Option<Self>, ImageAnalysisError> {
        if args.video_hosts.is_empty() {
            return Ok(None);
        }
        if args.video_max_concurrent == 0 || args.video_max_concurrent > Semaphore::MAX_PERMITS {
            return Err(ImageAnalysisError::InvalidConfig {
                error: format!(
                    "Video concurrency must be between 1 and {}",
                    Semaphore::MAX_PERMITS
                ),
            });
        }
        let timeout = Duration::from_secs(args.timeout);
        for binary in ["ffmpeg", "ffprobe"] {
            let mut command = Command::new(binary);
            command.arg("-version");
            run_media_command(command, timeout, Uuid::nil())
                .await
                .map_err(|err| ImageAnalysisError::InvalidConfig {
                    error: format!("Video analysis requires working {binary}: {err}"),
                })?;
        }
        Ok(Some(Self {
            host_manager: HostManager::new(
                args.video_hosts.clone(),
                Interface::Llamacpp,
                client,
                args.video_model_name.clone(),
                args.timeout,
                NonZeroU32::new(args.max_retries),
                Duration::from_secs(args.retry_delay_seconds),
                Duration::from_secs(args.unavailable_duration),
                args.video_api_key.clone(),
            ),
            prompt: args.video_prompt.clone(),
            max_bytes: args.video_max_bytes,
            timeout,
            permits: Semaphore::new(args.video_max_concurrent),
        }))
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub async fn analyze(
        &self,
        data_access: &DataAccess,
        asset_id: Uuid,
        prompt: &str,
    ) -> Result<ImageAnalysisResult, ImageAnalysisError> {
        let _permit = self.permits.acquire().await.map_err(|err| {
            processing_error(asset_id, format!("Video worker is unavailable: {err}"))
        })?;
        let workspace = tempfile::Builder::new()
            .prefix("immich-video-")
            .tempdir()
            .map_err(|err| processing_error(asset_id, format_error_chain(&err)))?;
        mark_activity();
        let destination = workspace.path().join("original");
        let original = tokio::time::timeout(
            self.timeout,
            data_access.materialize_original(&asset_id, &destination, self.max_bytes, self.timeout),
        )
        .await
        .map_err(|_| processing_error(asset_id, "Original acquisition timed out"))??;
        mark_activity();
        let probe = probe_media(&original, false, self.timeout, asset_id).await?;
        let selection = select_streams(&probe, asset_id)?;
        let clip = workspace.path().join("segment.mp4");
        let mut description = String::new();
        let mut start = 0;
        while start < selection.duration_ms {
            let end = start
                .saturating_add(SEGMENT_MILLIS)
                .min(selection.duration_ms);
            let has_audio = transcode_segment(
                &original,
                &clip,
                &selection,
                start,
                end,
                self.timeout,
                asset_id,
            )
            .await?;
            mark_activity();
            let audio_instruction = if has_audio {
                "Analyze both this segment's video and audio."
            } else {
                "This segment has no audio samples; describe only what is visible."
            };
            let segment_prompt = format!(
                "{prompt}\n\nSegment interval: {}–{}. {audio_instruction}",
                timestamp(start),
                timestamp(end)
            );
            let segment = self
                .host_manager
                .analyze_video_chunk(asset_id, &clip, &segment_prompt, has_audio)
                .await?;
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            write!(
                &mut description,
                "[{}–{}] {}",
                timestamp(start),
                timestamp(end),
                segment.description.trim()
            )
            .map_err(|err| processing_error(asset_id, err.to_string()))?;
            tokio::fs::remove_file(&clip)
                .await
                .map_err(|err| processing_error(asset_id, format_error_chain(&err)))?;
            start = end;
            mark_activity();
        }
        Ok(ImageAnalysisResult {
            description,
            asset_id,
        })
    }
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    index: Option<u32>,
    codec_type: String,
    duration: Option<String>,
    start_time: Option<String>,
    nb_read_packets: Option<String>,
    disposition: Option<Disposition>,
}

#[derive(Deserialize)]
struct Disposition {
    #[serde(default)]
    attached_pic: u8,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    start_time: Option<String>,
}

struct MediaSelection {
    video_index: u32,
    audio_index: Option<u32>,
    duration_ms: u64,
    start_offset_micros: u64,
}

fn processing_error(asset_id: Uuid, error: impl Into<String>) -> ImageAnalysisError {
    ImageAnalysisError::ProcessingError {
        filename: asset_id.to_string(),
        error: error.into(),
    }
}

// ffprobe emits decimal seconds. Parse them exactly, rounding only a nonzero
// fractional millisecond upward so floating-point error cannot invent a tail.
fn duration_millis(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut total = whole.parse::<u64>().ok()?.checked_mul(1000)?;
    let mut fraction_ms = 0_u64;
    let mut digits = fraction.bytes();
    for _ in 0..3_u8 {
        let digit = digits
            .next()
            .map_or(0, |byte| u64::from(byte.saturating_sub(b'0')));
        fraction_ms = fraction_ms.checked_mul(10)?.checked_add(digit)?;
    }
    total = total.checked_add(fraction_ms)?;
    if digits.any(|byte| byte != b'0') {
        total = total.checked_add(1)?;
    }
    (total > 0).then_some(total)
}

fn time_micros(value: &str) -> Option<i64> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned_value| (true, unsigned_value));
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut total = whole.parse::<i64>().ok()?.checked_mul(1_000_000)?;
    let mut fraction_us = 0_i64;
    let mut digits = fraction.bytes();
    for _ in 0..6_u8 {
        let digit = digits
            .next()
            .map_or(0, |byte| i64::from(byte.saturating_sub(b'0')));
        fraction_us = fraction_us.checked_mul(10)?.checked_add(digit)?;
    }
    total = total.checked_add(fraction_us)?;
    if negative {
        total.checked_neg()
    } else {
        Some(total)
    }
}

fn timestamp(millis: u64) -> String {
    let seconds = millis / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60,
        millis % 1000
    )
}

fn seconds_argument(millis: u64) -> String {
    format!("{}.{:03}", millis / 1000, millis % 1000)
}

fn select_streams(probe: &Probe, asset_id: Uuid) -> Result<MediaSelection, ImageAnalysisError> {
    let video = probe
        .streams
        .iter()
        .find(|stream| {
            stream.codec_type == "video"
                && stream
                    .disposition
                    .as_ref()
                    .is_none_or(|value| value.attached_pic == 0)
        })
        .ok_or_else(|| processing_error(asset_id, "Original contains no video stream"))?;
    let video_index = video
        .index
        .ok_or_else(|| processing_error(asset_id, "Video stream has no index"))?;
    let audio_index = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type == "audio")
        .map(|stream| {
            stream
                .index
                .ok_or_else(|| processing_error(asset_id, "Audio stream has no index"))
        })
        .transpose()?;
    let container_start = probe
        .format
        .as_ref()
        .and_then(|format| format.start_time.as_deref())
        .and_then(time_micros)
        .or_else(|| {
            probe
                .streams
                .iter()
                .filter_map(|stream| stream.start_time.as_deref().and_then(time_micros))
                .min()
        })
        .unwrap_or(0);
    let video_start = video
        .start_time
        .as_deref()
        .and_then(time_micros)
        .unwrap_or(container_start);
    let start_offset_micros = video_start
        .checked_sub(container_start)
        .and_then(|offset| u64::try_from(offset).ok())
        .ok_or_else(|| processing_error(asset_id, "Video starts before the container timeline"))?;
    let duration_ms = video
        .duration
        .as_deref()
        .and_then(duration_millis)
        .or_else(|| {
            let duration = probe
                .format
                .as_ref()?
                .duration
                .as_deref()
                .and_then(time_micros)?;
            u64::try_from(duration)
                .ok()?
                .checked_sub(start_offset_micros)
                .map(|remaining| remaining.div_ceil(1000))
                .filter(|millis| *millis > 0)
        })
        .ok_or_else(|| {
            processing_error(asset_id, "Video duration is missing, invalid, or empty")
        })?;
    Ok(MediaSelection {
        video_index,
        audio_index,
        duration_ms,
        start_offset_micros,
    })
}

async fn probe_media(
    path: &Path,
    packets: bool,
    timeout: Duration,
    asset_id: Uuid,
) -> Result<Probe, ImageAnalysisError> {
    let mut command = Command::new("ffprobe");
    command.args(["-v", "error"]);
    if packets {
        command.args([
            "-count_packets",
            "-show_entries",
            "stream=codec_type,nb_read_packets",
        ]);
    } else {
        command.args(["-show_entries", "format=duration,start_time:stream=index,codec_type,duration,start_time:stream_disposition=attached_pic"]);
    }
    command.args(["-of", "json"]).arg(path);
    let output = run_media_command(command, timeout, asset_id).await?;
    serde_json::from_slice(&output)
        .map_err(|err| processing_error(asset_id, format!("Invalid ffprobe response: {err}")))
}

async fn transcode_segment(
    source: &Path,
    clip: &Path,
    selection: &MediaSelection,
    start: u64,
    end: u64,
    timeout: Duration,
    asset_id: Uuid,
) -> Result<bool, ImageAnalysisError> {
    let duration = end
        .checked_sub(start)
        .filter(|value| *value > 0)
        .ok_or_else(|| processing_error(asset_id, "Invalid video segment interval"))?;
    let seek = start
        .checked_mul(1000)
        .and_then(|offset| offset.checked_add(selection.start_offset_micros))
        .ok_or_else(|| processing_error(asset_id, "Video segment seek offset overflowed"))?;
    let mut command = Command::new("ffmpeg");
    command
        .args(["-nostdin", "-v", "error", "-xerror", "-y", "-ss"])
        .arg(format!("{}.{:06}", seek / 1_000_000, seek % 1_000_000))
        .args(["-err_detect", "explode", "-i"])
        .arg(source)
        .arg("-t")
        .arg(seconds_argument(duration))
        .arg("-map")
        .arg(format!("0:{}", selection.video_index));
    if let Some(index) = selection.audio_index {
        command.arg("-map").arg(format!("0:{index}"));
    } else {
        command.arg("-an");
    }
    command.args([
        "-vf",
        "scale=w=640:h=640:force_original_aspect_ratio=decrease:force_divisible_by=2",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "28",
        "-pix_fmt",
        "yuv420p",
    ]);
    if selection.audio_index.is_some() {
        command.args(["-c:a", "aac", "-b:a", "96k"]);
    }
    command.args(["-movflags", "+faststart"]).arg(clip);
    run_media_command(command, timeout, asset_id).await?;
    let bytes = tokio::fs::metadata(clip)
        .await
        .map_err(|err| processing_error(asset_id, format_error_chain(&err)))?
        .len();
    if bytes == 0 || bytes > MAX_CLIP_BYTES {
        return Err(processing_error(
            asset_id,
            format!("Encoded clip has {bytes} bytes; expected 1 through {MAX_CLIP_BYTES}"),
        ));
    }
    let probe = probe_media(clip, true, timeout, asset_id).await?;
    let has_packets = |stream: &ProbeStream| {
        stream
            .nb_read_packets
            .as_deref()
            .and_then(|count| count.parse::<u64>().ok())
            .is_some_and(|count| count > 0)
    };
    if !probe
        .streams
        .iter()
        .any(|stream| stream.codec_type == "video" && has_packets(stream))
    {
        return Err(processing_error(
            asset_id,
            "Encoded segment contains no video packets",
        ));
    }
    Ok(probe
        .streams
        .iter()
        .any(|stream| stream.codec_type == "audio" && has_packets(stream)))
}

async fn read_probe_output(mut reader: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(output);
        }
        if output
            .len()
            .checked_add(count)
            .is_none_or(|length| length > MAX_PROBE_BYTES)
        {
            return Err(io::Error::other("Media command stdout exceeded 1 MiB"));
        }
        let chunk = buffer.get(..count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Invalid stdout read length")
        })?;
        output.extend_from_slice(chunk);
    }
}

async fn read_diagnostics(mut reader: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut tail = VecDeque::new();
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(Vec::from(tail));
        }
        let excess = tail
            .len()
            .saturating_add(count)
            .saturating_sub(MAX_DIAGNOSTIC_BYTES);
        tail.drain(..excess);
        let chunk = buffer.get(..count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Invalid stderr read length")
        })?;
        tail.extend(chunk);
    }
}

async fn run_media_command(
    mut command: Command,
    timeout: Duration,
    asset_id: Uuid,
) -> Result<Vec<u8>, ImageAnalysisError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| processing_error(asset_id, format_error_chain(&err)))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| processing_error(asset_id, "Media command stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| processing_error(asset_id, "Media command stderr is unavailable"))?;
    let operation = async {
        tokio::try_join!(
            read_probe_output(stdout),
            read_diagnostics(stderr),
            child.wait()
        )
    };
    let completed = tokio::time::timeout(timeout, operation)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Media command timed out",
            ))
        });
    match completed {
        Ok((output, diagnostics, status)) => {
            if status.success() {
                return Ok(output);
            }
            Err(processing_error(
                asset_id,
                format!(
                    "{} exited with {status}: {}",
                    command.as_std().get_program().to_string_lossy(),
                    String::from_utf8_lossy(&diagnostics)
                ),
            ))
        }
        Err(error) => {
            // On timeout/read failure the child may still be running. Kill and
            // reap it before its caller can drop the media workspace.
            if let Err(err) = child.kill().await {
                log::debug!("Media child termination: {err}");
            }
            if let Err(err) = child.wait().await {
                log::debug!("Media child reaping: {err}");
            }
            Err(processing_error(asset_id, format_error_chain(&error)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Probe, duration_millis, probe_media, run_media_command, select_streams, timestamp,
        transcode_segment,
    };
    use std::{error::Error, time::Duration};
    use tokio::process::Command;
    use uuid::Uuid;

    #[test]
    fn durations_round_only_real_fractional_milliseconds_and_reject_invalid_values() {
        assert_eq!(duration_millis("30.000000"), Some(30_000));
        assert_eq!(duration_millis("30.000001"), Some(30_001));
        assert_eq!(duration_millis("0.000001"), Some(1));
        assert_eq!(duration_millis("18446744073709551616"), None);
        for value in ["0", "-1", "NaN", "inf", "N/A", "", "1.bad"] {
            assert_eq!(duration_millis(value), None);
        }
        assert_eq!(timestamp(3_661_001), "01:01:01.001");
    }

    #[test]
    fn attached_picture_is_not_the_video_timeline() -> Result<(), Box<dyn Error>> {
        let probe: Probe = serde_json::from_str(
            r#"{"streams":[
          {"index":0,"codec_type":"video","duration":"1","disposition":{"attached_pic":1}},
          {"index":1,"codec_type":"video","duration":"31.000000"},
          {"index":2,"codec_type":"audio"}],"format":{"duration":"32"}}"#,
        )?;
        let selection = select_streams(&probe, Uuid::nil())?;
        assert_eq!(selection.video_index, 1);
        assert_eq!(selection.duration_ms, 31_000);
        Ok(())
    }

    #[tokio::test]
    async fn real_segments_preserve_audio_and_cover_a_short_final_interval()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source.mp4");
        let clip = directory.path().join("segment.mp4");
        let timeout = Duration::from_secs(30);
        let id = Uuid::from_u128(123);
        let mut generate = Command::new("ffmpeg");
        generate
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x64:r=10:d=31",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=16000:duration=0.5",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(&source);
        run_media_command(generate, timeout, id).await?;
        let probe = probe_media(&source, false, timeout, id).await?;
        let selection = select_streams(&probe, id)?;
        assert_eq!(selection.duration_ms, 31_000);
        assert!(transcode_segment(&source, &clip, &selection, 0, 30_000, timeout, id).await?);
        let first = select_streams(&probe_media(&clip, false, timeout, id).await?, id)?;
        assert_eq!(first.duration_ms, 30_000);
        assert!(!transcode_segment(&source, &clip, &selection, 30_000, 31_000, timeout, id).await?);
        let last = select_streams(&probe_media(&clip, false, timeout, id).await?, id)?;
        assert_eq!(last.duration_ms, 1000);
        assert!(source.exists());
        Ok(())
    }

    #[tokio::test]
    async fn delayed_video_keeps_its_final_visual_event() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("delayed.mp4");
        let clip = directory.path().join("last.mp4");
        let timeout = Duration::from_secs(30);
        let id = Uuid::from_u128(124);
        let mut generate = Command::new("ffmpeg");
        generate.args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=64x64:r=10:d=31",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=16000:duration=36",
            "-filter_complex", "[0:v]drawbox=x=0:y=0:w=iw:h=ih:color=blue:t=fill:enable='gte(t,30)',setpts=PTS+5/TB[v]",
            "-map", "[v]", "-map", "1:a", "-fps_mode", "passthrough",
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"]).arg(&source);
        run_media_command(generate, timeout, id).await?;
        let selection = select_streams(&probe_media(&source, false, timeout, id).await?, id)?;
        assert_eq!(selection.duration_ms, 31_000);
        transcode_segment(&source, &clip, &selection, 30_000, 31_000, timeout, id).await?;
        let mut pixel = Command::new("ffmpeg");
        pixel
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&clip)
            .args([
                "-vf",
                "scale=1:1",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ]);
        let rgb = run_media_command(pixel, timeout, id).await?;
        let [red, _, blue] = rgb.as_slice() else {
            return Err("Expected one RGB pixel".into());
        };
        assert!(
            blue > red,
            "Final event must be blue, not an earlier red frame: {rgb:?}"
        );
        Ok(())
    }
}
