//! All external media operations use only the already deployed `ffmpeg` executable.
use super::Block;
use std::{
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    process::Command,
};

const RATE: u64 = 24000;

async fn command(args: Vec<String>) -> Result<Output, String> {
    let operation = if args.iter().any(|arg| arg == "-c:v") {
        "译制视频封装"
    } else if args.iter().any(|arg| arg.starts_with("atempo=")) {
        "配音变速对轴"
    } else if args.iter().any(|arg| arg == "pcm_s16le") {
        "音频提取或格式规范化"
    } else {
        "媒体时长探测"
    };
    let started = tokio::time::Instant::now();
    tracing::info!(target: "video_translation", operation, "开始执行FFmpeg");
    let pending = tokio::time::timeout(
        Duration::from_secs(7200),
        Command::new("ffmpeg")
            .args(["-hide_banner", "-nostdin"])
            .args(args)
            .kill_on_drop(true)
            .output(),
    );
    tokio::pin!(pending);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    heartbeat.tick().await;
    loop {
        tokio::select! {
            result = &mut pending => {
                let output = result.map_err(|_| "FFmpeg 处理超时".to_string())?
                    .map_err(|e| format!("无法运行服务器 FFmpeg：{e}"))?;
                // A probe deliberately exits without an output file; its nonzero status is expected.
                tracing::info!(target: "video_translation", operation, elapsed_ms=started.elapsed().as_millis() as u64,
                    exit_code=output.status.code(), "FFmpeg执行结束");
                return Ok(output);
            }
            _ = heartbeat.tick() => {
                tracing::info!(target: "video_translation", operation, elapsed_seconds=started.elapsed().as_secs(), "FFmpeg仍在处理");
            }
        }
    }
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
fn checked(output: Output) -> Result<(), String> {
    if output.status.success() {
        Ok(())
    } else {
        // Keep useful diagnostics, bounded and without media URLs or credentials.
        let error = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "FFmpeg 处理失败：{}",
            error
                .chars()
                .rev()
                .take(1600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        ))
    }
}

pub fn parse_duration(stderr: &str) -> Result<u64, String> {
    let value = stderr
        .split("Duration: ")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .ok_or("FFmpeg 未返回有效视频时长")?;
    let parts: Vec<_> = value.trim().split(':').collect();
    if parts.len() != 3 {
        return Err("视频时长格式无效".into());
    }
    let hours: u64 = parts[0].parse().map_err(|_| "视频时长无效")?;
    let minutes: u64 = parts[1].parse().map_err(|_| "视频时长无效")?;
    let seconds: f64 = parts[2].parse().map_err(|_| "视频时长无效")?;
    if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) || minutes >= 60 || hours > 12 {
        return Err("视频时长无效或超过12小时".into());
    }
    let ms = (hours * 3600 + minutes * 60) * 1000 + (seconds * 1000.0).round() as u64;
    if ms == 0 || ms > 43200000 {
        return Err("视频时长为零或超过12小时".into());
    }
    Ok(ms)
}

pub async fn duration(source: &Path) -> Result<u64, String> {
    // ffmpeg -i is a metadata probe; its expected exit status is nonzero without an output.
    let output = command(vec!["-i".into(), path(source)]).await?;
    parse_duration(&String::from_utf8_lossy(&output.stderr))
}

pub async fn extract(source: &Path, destination: &Path) -> Result<(), String> {
    let tmp = destination.with_extension("tmp.wav");
    checked(
        command(vec![
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-i".into(),
            path(source),
            "-map".into(),
            "0:a:0".into(),
            "-vn".into(),
            "-af".into(),
            "aresample=async=1:first_pts=0".into(),
            "-ac".into(),
            "1".into(),
            "-ar".into(),
            RATE.to_string(),
            "-c:a".into(),
            "pcm_s16le".into(),
            path(&tmp),
        ])
        .await?,
    )?;
    wav_info(&tmp).await?;
    fs::rename(tmp, destination)
        .await
        .map_err(|e| e.to_string())
}

pub async fn align(source: &Path, destination: &Path, speed: f64) -> Result<(), String> {
    let tmp = destination.with_extension("tmp.wav");
    checked(
        command(vec![
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-i".into(),
            path(source),
            "-af".into(),
            format!("atempo={speed:.8}"),
            "-ac".into(),
            "1".into(),
            "-ar".into(),
            RATE.to_string(),
            "-c:a".into(),
            "pcm_s16le".into(),
            path(&tmp),
        ])
        .await?,
    )?;
    wav_info(&tmp).await?;
    fs::rename(tmp, destination)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Debug)]
pub struct WavInfo {
    pub offset: u64,
    pub bytes: u64,
}
impl WavInfo {
    pub fn duration_ms(&self) -> u64 {
        (self.bytes * 1000).div_ceil(RATE * 2)
    }
}

pub async fn wav_info(file: &Path) -> Result<WavInfo, String> {
    let mut file = fs::File::open(file).await.map_err(|e| e.to_string())?;
    let length = file.metadata().await.map_err(|e| e.to_string())?.len();
    let mut header = [0; 12];
    file.read_exact(&mut header)
        .await
        .map_err(|_| "WAV 文件不完整")?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return Err("无效的 PCM WAV 文件".into());
    }
    let mut valid_format = false;
    loop {
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk)
            .await
            .map_err(|_| "WAV 缺少有效 data 区块")?;
        let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as u64;
        let offset = file.stream_position().await.map_err(|e| e.to_string())?;
        if size > length.saturating_sub(offset) {
            return Err("WAV 区块长度超出文件".into());
        }
        if &chunk[..4] == b"fmt " {
            if size < 16 {
                return Err("WAV 格式区块无效".into());
            }
            let mut format = [0; 16];
            file.read_exact(&mut format)
                .await
                .map_err(|e| e.to_string())?;
            valid_format = u16::from_le_bytes(format[0..2].try_into().unwrap()) == 1
                && u16::from_le_bytes(format[2..4].try_into().unwrap()) == 1
                && u32::from_le_bytes(format[4..8].try_into().unwrap()) == RATE as u32
                && u16::from_le_bytes(format[12..14].try_into().unwrap()) == 2
                && u16::from_le_bytes(format[14..16].try_into().unwrap()) == 16;
        } else if &chunk[..4] == b"data" {
            if !valid_format || size < 2 || size % 2 != 0 {
                return Err("需要24kHz单声道PCM16 WAV".into());
            }
            return Ok(WavInfo {
                offset,
                bytes: size,
            });
        }
        file.seek(SeekFrom::Start(offset + size + size % 2))
            .await
            .map_err(|e| e.to_string())?;
    }
}

pub fn wav_header(bytes: u32) -> [u8; 44] {
    let mut h = [0; 44];
    h[..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(bytes + 36).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&(RATE as u32).to_le_bytes());
    h[28..32].copy_from_slice(&((RATE * 2) as u32).to_le_bytes());
    h[32..34].copy_from_slice(&2u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&bytes.to_le_bytes());
    h
}

async fn silence(file: &mut fs::File, mut bytes: u64) -> Result<(), String> {
    let zero = [0; 65536];
    while bytes > 0 {
        let count = bytes.min(zero.len() as u64) as usize;
        file.write_all(&zero[..count])
            .await
            .map_err(|e| e.to_string())?;
        bytes -= count as u64;
    }
    Ok(())
}

pub async fn assemble(
    root: &Path,
    blocks: &[Block],
    duration_ms: u64,
    output: &Path,
) -> Result<(), String> {
    let total = duration_ms * RATE / 1000 * 2;
    if total > u32::MAX as u64 - 36 {
        return Err("完整配音超过WAV容量限制".into());
    }
    let temp = output.with_extension("tmp.wav");
    let mut destination = fs::File::create(&temp).await.map_err(|e| e.to_string())?;
    destination
        .write_all(&wav_header(total as u32))
        .await
        .map_err(|e| e.to_string())?;
    let mut cursor = 0;
    for b in blocks {
        tracing::info!(target: "video_translation", block=b.id+1, total_blocks=blocks.len(), start_ms=b.start_ms,
            "开始将配音块写入固定时间轴");
        let start = b.start_ms * RATE / 1000 * 2;
        let info = wav_info(&root.join(format!("aligned-{}.wav", b.id))).await?;
        if start < cursor || start + info.bytes > total {
            return Err("配音音轨重叠或超过原视频时长".into());
        }
        silence(&mut destination, start - cursor).await?;
        let mut file = fs::File::open(root.join(format!("aligned-{}.wav", b.id)))
            .await
            .map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(info.offset))
            .await
            .map_err(|e| e.to_string())?;
        let copied = tokio::io::copy(&mut file.take(info.bytes), &mut destination)
            .await
            .map_err(|e| e.to_string())?;
        if copied != info.bytes {
            return Err("配音音频读取不完整".into());
        }
        cursor = start + copied;
        tracing::info!(target: "video_translation", block=b.id+1, total_blocks=blocks.len(), copied_bytes=copied, "配音块时间轴写入完成");
    }
    silence(&mut destination, total - cursor).await?;
    destination.flush().await.map_err(|e| e.to_string())?;
    drop(destination);
    fs::rename(temp, output).await.map_err(|e| e.to_string())
}

pub async fn mux(
    source: &Path,
    dub: &Path,
    root: &Path,
    output: &Path,
    duration_ms: u64,
) -> Result<(), String> {
    let tmp: PathBuf = output.with_extension("tmp.mp4");
    checked(
        command(vec![
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-i".into(),
            path(source),
            "-i".into(),
            path(dub),
            "-i".into(),
            path(&root.join("zh.dub.srt")),
            "-i".into(),
            path(&root.join("en.source.srt")),
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "1:a:0".into(),
            "-map".into(),
            "0:a:0".into(),
            "-map".into(),
            "2:0".into(),
            "-map".into(),
            "3:0".into(),
            "-c:v".into(),
            "copy".into(),
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "128k".into(),
            "-c:s".into(),
            "mov_text".into(),
            "-disposition:a:0".into(),
            "default".into(),
            "-disposition:a:1".into(),
            "0".into(),
            "-metadata:s:a:0".into(),
            "language=zho".into(),
            "-metadata:s:a:0".into(),
            "title=中文配音".into(),
            "-metadata:s:a:1".into(),
            "language=eng".into(),
            "-metadata:s:a:1".into(),
            "title=英文原声".into(),
            "-metadata:s:s:0".into(),
            "language=zho".into(),
            "-metadata:s:s:1".into(),
            "language=eng".into(),
            "-movflags".into(),
            "+faststart".into(),
            path(&tmp),
        ])
        .await?,
    )?;
    let actual = duration(&tmp).await?;
    if actual.abs_diff(duration_ms) > 100 {
        return Err("译制视频时长校验失败".into());
    }
    fs::rename(tmp, output).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_ffmpeg_duration_without_external_programs() {
        assert_eq!(
            parse_duration("Duration: 01:02:03.45, start: 0.000000").unwrap(),
            3723450
        );
        for invalid in [
            "Duration: N/A,",
            "Duration: 00:00:NaN,",
            "Duration: 13:00:00.00,",
            "",
        ] {
            assert!(parse_duration(invalid).is_err());
        }
    }
    #[tokio::test]
    async fn streaming_assembly_retains_absolute_positions_and_rejects_overlap() {
        let temp = tempdir::TempDir::new_in("target", "translation-wav").unwrap();
        let root = temp.path();
        let mut bytes = wav_header(4800).to_vec();
        bytes.extend(vec![1; 4800]);
        fs::write(root.join("aligned-0.wav"), &bytes).await.unwrap();
        let b = Block {
            id: 0,
            start_ms: 200,
            end_ms: 400,
            source: "x".into(),
            translation: "文".into(),
            spoken: "文".into(),
            revision: 0,
            done: true,
            audio_ms: 100,
        };
        assemble(root, &[b.clone()], 1000, &root.join("dub.wav"))
            .await
            .unwrap();
        assert_eq!(
            wav_info(&root.join("dub.wav")).await.unwrap().duration_ms(),
            1000
        );
        let output = fs::read(root.join("dub.wav")).await.unwrap();
        assert!(output[44..44 + 9600].iter().all(|v| *v == 0));
        assert!(output[44 + 9600..44 + 14400].iter().all(|v| *v == 1));
        assert!(assemble(root, &[b.clone(), b], 1000, &root.join("bad.wav"))
            .await
            .is_err());
        fs::write(root.join("broken.wav"), &bytes[..30])
            .await
            .unwrap();
        assert!(wav_info(&root.join("broken.wav")).await.is_err());
    }
}
