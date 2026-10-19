use super::Block;
use serde_json::Value;
use std::path::Path;
use tokio::fs;

fn timestamp(ms: u64, separator: char) -> String {
    format!(
        "{:02}:{:02}:{:02}{separator}{:03}",
        ms / 3600000,
        ms / 60000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn clean(text: &str) -> String {
    // Text is data, never WebVTT cue markup or HTML.
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn render(blocks: &[Block], kind: &str, srt: bool) -> String {
    let mut out = if srt {
        String::new()
    } else {
        "WEBVTT\n\n".into()
    };
    for b in blocks {
        let dub = matches!(kind, "zh.dub" | "en.dub" | "bilingual.dub");
        let end = if dub {
            b.start_ms + b.audio_ms
        } else {
            b.end_ms
        };
        let text = match kind {
            "en.source" | "en.dub" => clean(&b.source),
            "zh.source" => clean(&b.translation),
            "zh.dub" => clean(&b.spoken),
            "bilingual.source" => format!("{}\n{}", clean(&b.translation), clean(&b.source)),
            _ => format!("{}\n{}", clean(&b.spoken), clean(&b.source)),
        };
        let separator = if srt { ',' } else { '.' };
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            b.id + 1,
            timestamp(b.start_ms, separator),
            timestamp(end, separator),
            text
        ));
    }
    out
}

pub const KINDS: &[&str] = &[
    "en.source",
    "en.dub",
    "zh.source",
    "zh.dub",
    "bilingual.source",
    "bilingual.dub",
];

#[derive(Debug, Clone)]
struct Cue {
    start: u64,
    end: u64,
    text: String,
}

fn normalized(text: &str) -> Vec<char> {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

// Match known spoken text to ASR words, using ASR for timing only, never for wording.
// A bounded LCS rejects low coverage; it is not advertised as forced alignment.
fn refine(block: &Block, raw: &Value) -> Option<Vec<Cue>> {
    let end = block.start_ms + block.audio_ms;
    let mut heard: Vec<(char, u64, u64)> = Vec::new();
    for transcript in raw["transcripts"].as_array()? {
        for sentence in transcript["sentences"].as_array()? {
            for word in sentence["words"].as_array()? {
                let start = word["begin_time"].as_u64()?;
                let stop = word["end_time"].as_u64()?;
                if start < block.start_ms.saturating_sub(50) || stop > end + 50 || start >= stop {
                    continue;
                }
                for ch in normalized(word["text"].as_str()?) {
                    heard.push((ch, start.max(block.start_ms), stop.min(end)));
                }
            }
        }
    }
    let known = normalized(&block.spoken);
    let (n, m) = (known.len(), heard.len());
    if n == 0 || m == 0 || n > 512 || m > 768 {
        return None;
    }
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in 0..n {
        for j in 0..m {
            dp[i + 1][j + 1] = if known[i] == heard[j].0 {
                dp[i][j] + 1
            } else {
                dp[i][j + 1].max(dp[i + 1][j])
            };
        }
    }
    let matches = dp[n][m] as usize;
    if matches * 100 < n * 85 || matches * 100 < m * 85 {
        return None;
    }
    let mut mapping = vec![None; n];
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        if known[i - 1] == heard[j - 1].0 {
            mapping[i - 1] = Some((heard[j - 1].1, heard[j - 1].2));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    let mut cues: Vec<Cue> = Vec::new();
    let mut text = String::new();
    let mut offset = 0;
    let mut part_start = 0;
    for ch in block.spoken.chars() {
        text.push(ch);
        offset += normalized(&ch.to_string()).len();
        if "，。！？；!?;".contains(ch) || text.chars().count() >= 28 {
            if offset > part_start {
                let stamps: Vec<_> = mapping[part_start..offset]
                    .iter()
                    .flatten()
                    .copied()
                    .collect();
                let first = stamps.first()?;
                let last = stamps.last()?;
                let cue = Cue {
                    start: first.0,
                    end: last.1,
                    text: std::mem::take(&mut text),
                };
                if cue.start >= cue.end {
                    return None;
                }
                if let Some(previous) = cues.last_mut().filter(|c| c.end > cue.start) {
                    previous.end = previous.end.max(cue.end);
                    previous.text.push_str(&cue.text);
                } else {
                    cues.push(cue);
                }
                part_start = offset;
            }
        }
    }
    if !text.trim().is_empty() {
        if offset > part_start {
            let stamps: Vec<_> = mapping[part_start..offset]
                .iter()
                .flatten()
                .copied()
                .collect();
            let first = stamps.first()?;
            let last = stamps.last()?;
            if first.0 >= last.1 {
                return None;
            }
            if let Some(previous) = cues.last_mut().filter(|c| c.end > first.0) {
                previous.end = previous.end.max(last.1);
                previous.text.push_str(&text);
            } else {
                cues.push(Cue {
                    start: first.0,
                    end: last.1,
                    text,
                });
            }
        } else if let Some(last) = cues.last_mut() {
            last.text.push_str(&text);
        }
    }
    if cues.is_empty() {
        None
    } else {
        Some(cues)
    }
}

fn render_cues(cues: &[Cue], srt: bool) -> String {
    let mut out = if srt {
        String::new()
    } else {
        "WEBVTT\n\n".into()
    };
    for (i, cue) in cues.iter().enumerate() {
        let separator = if srt { ',' } else { '.' };
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            timestamp(cue.start, separator),
            timestamp(cue.end, separator),
            clean(&cue.text)
        ));
    }
    out
}

pub async fn write(
    root: &Path,
    blocks: &[Block],
    raw: Option<&Value>,
    alignment_error: Option<&str>,
) -> Result<String, String> {
    for kind in KINDS {
        for (extension, srt) in [("vtt", false), ("srt", true)] {
            fs::write(
                root.join(format!("{kind}.{extension}")),
                render(blocks, kind, srt),
            )
            .await
            .map_err(|e| e.to_string())?;
        }
    }
    let mut refined_blocks = 0;
    let mut cues = Vec::new();
    for block in blocks {
        if let Some(refined) = raw.and_then(|raw| refine(block, raw)) {
            cues.extend(refined);
            refined_blocks += 1;
        } else {
            cues.push(Cue {
                start: block.start_ms,
                end: block.start_ms + block.audio_ms,
                text: block.spoken.clone(),
            });
        }
    }
    for (extension, srt) in [("vtt", false), ("srt", true)] {
        fs::write(
            root.join(format!("zh.dub.{extension}")),
            render_cues(&cues, srt),
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    let precision = if refined_blocks == blocks.len() {
        "asr"
    } else if refined_blocks > 0 {
        "mixed"
    } else {
        "block"
    };
    super::atomic_json(&root.join("quality-report.json"), &serde_json::json!({
        "subtitle_precision": precision, "note": "使用已知配音文本匹配ASR时间戳，低覆盖块回退实际配音块边界；不是强制对齐",
        "refined_blocks":refined_blocks,"alignment_error":alignment_error,
        "blocks": blocks.len(), "compressed_blocks": blocks.iter().filter(|b| b.revision > 0).count(),
        "maximum_duration_ms": blocks.iter().map(|b| b.audio_ms).max().unwrap_or(0)
    })).await?;
    Ok(precision.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dub_subtitles_use_spoken_revision_and_measured_duration() {
        let b = Block {
            id: 0,
            start_ms: 1000,
            end_ms: 6000,
            source: "<English>".into(),
            translation: "忠实长译文".into(),
            spoken: "精简配音".into(),
            revision: 1,
            done: true,
            audio_ms: 3200,
        };
        let vtt = render(&[b.clone()], "zh.dub", false);
        assert!(vtt.contains("00:00:01.000 --> 00:00:04.200\n精简配音"));
        assert!(!vtt.contains("忠实"));
        assert!(render(&[b], "en.source", false).contains("&lt;English&gt;"));
    }
    #[test]
    fn timing_refinement_retains_known_text_and_rejects_mismatches() {
        let block = Block {
            id: 0,
            start_ms: 1000,
            end_ms: 6000,
            source: "source".into(),
            translation: "你好世界。再见。".into(),
            spoken: "你好世界。再见。".into(),
            revision: 0,
            done: true,
            audio_ms: 4000,
        };
        let raw = serde_json::json!({"transcripts":[{"sentences":[{"words":[
            {"begin_time":1100,"end_time":1500,"text":"你好"},{"begin_time":1600,"end_time":2200,"text":"世界"},
            {"begin_time":3000,"end_time":3400,"text":"再见"}
        ]}]}]});
        let cues = refine(&block, &raw).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].end), (1100, 2200));
        assert_eq!(cues[0].text, "你好世界。");
        let mut wrong = block.clone();
        wrong.spoken = "这是一段完全不同的内容。".into();
        assert!(refine(&wrong, &raw).is_none());
    }
}
