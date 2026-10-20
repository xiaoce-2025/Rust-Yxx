use super::Block;
use serde_json::Value;

pub fn blocks(raw: &Value, duration: u64) -> Result<Vec<Block>, String> {
    let transcripts = raw["transcripts"]
        .as_array()
        .ok_or("ASR 缺少 transcripts")?;
    let mut units: Vec<(u64, u64, String)> = Vec::new();
    // Audio is extracted as mono, so only the first transcript is expected.
    let transcript = transcripts.first().ok_or("未识别到英文语音")?;
    for sentence in transcript["sentences"]
        .as_array()
        .ok_or("ASR 缺少句级时间戳")?
    {
        let start = sentence["begin_time"].as_u64().ok_or("ASR 起始时间无效")?;
        let end = sentence["end_time"]
            .as_u64()
            .ok_or("ASR 结束时间无效")?
            .min(duration);
        let text = sentence["text"].as_str().unwrap_or("").trim();
        if text.is_empty() {
            continue;
        }
        if start >= end {
            return Err("ASR 时间戳越界或时长为零".into());
        }
        if end - start > 15000 {
            if let Some(words) = sentence["words"].as_array().filter(|v| !v.is_empty()) {
                for word in words {
                    let s = word["begin_time"].as_u64().ok_or("ASR 词起始时间无效")?;
                    let e = word["end_time"]
                        .as_u64()
                        .ok_or("ASR 词结束时间无效")?
                        .min(duration);
                    let t = word["text"].as_str().unwrap_or("");
                    let punctuation = word["punctuation"].as_str().unwrap_or("");
                    if !t.trim().is_empty() && s < e {
                        units.push((s, e, format!("{t}{punctuation}")));
                    }
                }
                continue;
            }
        }
        units.push((start, end, text.into()));
    }
    units.sort_by_key(|v| v.0);
    let mut result: Vec<Block> = Vec::new();
    for (start, end, text) in units {
        if let Some(last) = result.last_mut() {
            // Keep pauses; merge overlapping speech into one semantic window.
            if start < last.end_ms
                || (end - last.start_ms <= 12000 && start.saturating_sub(last.end_ms) < 800)
            {
                last.end_ms = last.end_ms.max(end);
                last.source.push(' ');
                last.source.push_str(&text);
                continue;
            }
        }
        result.push(Block {
            id: result.len(),
            start_ms: start,
            end_ms: end,
            source: text,
            translation: String::new(),
            spoken: String::new(),
            revision: 0,
            done: false,
            audio_ms: 0,
        });
    }
    if result.is_empty() {
        return Err("未识别到可翻译的英文语音".into());
    }
    Ok(result)
}

pub fn window(blocks: &[Block], index: usize, duration: u64) -> Result<u64, String> {
    let b = blocks.get(index).ok_or("配音块不存在")?;
    let next = blocks
        .get(index + 1)
        .map(|b| b.start_ms)
        .unwrap_or(duration);
    if b.start_ms >= b.end_ms || b.end_ms > next || next > duration {
        return Err("配音时间轴重叠或越界".into());
    }
    let borrowed = next.saturating_sub(b.end_ms).saturating_sub(100).min(1000);
    Ok(b.end_ms + borrowed - b.start_ms)
}

pub fn fit_speed(audio_ms: u64, window_ms: u64, maximum: f64) -> Option<f64> {
    if audio_ms == 0 || window_ms == 0 || !maximum.is_finite() || maximum < 1.0 {
        return None;
    }
    if audio_ms <= window_ms {
        return Some(1.0);
    }
    // Reserve a small rounding margin before measuring FFmpeg's actual result again.
    let speed = audio_ms as f64 / window_ms.saturating_sub(30).max(1) as f64;
    if speed <= maximum {
        Some(speed)
    } else {
        None
    }
}

// Automatic repairs may use more real trailing silence, but never the next block's speech.
pub fn repair_window(blocks: &[Block], index: usize, duration: u64) -> Result<u64, String> {
    let normal = window(blocks, index, duration)?;
    let b = &blocks[index];
    let next = blocks.get(index + 1).map_or(duration, |b| b.start_ms);
    let extra = next.saturating_sub(b.end_ms).saturating_sub(100).min(3000);
    Ok(normal.max(b.end_ms + extra - b.start_ms))
}

pub const MAX_REPAIRS: u8 = 8;
pub const REPAIR_MAX_SPEED: f64 = 2.0;

// Split only for synthesis; source block IDs, text and absolute anchors remain stable.
pub fn speech_chunks(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut part = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (index, &ch) in chars.iter().enumerate() {
        part.push(ch);
        let word_boundary = !ch.is_ascii_alphanumeric()
            || chars
                .get(index + 1)
                .is_none_or(|c| !c.is_ascii_alphanumeric());
        if pieces.len() < 7
            && part.chars().count() >= 8
            && ("，。！？；：,!?;:".contains(ch) || (part.chars().count() >= 48 && word_boundary))
        {
            pieces.push(std::mem::take(&mut part));
        }
    }
    if !part.trim().is_empty() {
        pieces.push(part);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repair_uses_available_silence_without_moving_neighbors() {
        let blocks = blocks(
            &serde_json::json!({"transcripts":[{"sentences":[
                {"begin_time":0,"end_time":1000,"text":"First."},
                {"begin_time":5000,"end_time":6000,"text":"Next."}
            ]}]}),
            8000,
        )
        .unwrap();
        assert_eq!(window(&blocks, 0, 8000).unwrap(), 2000);
        assert_eq!(repair_window(&blocks, 0, 8000).unwrap(), 4000);
        assert_eq!(blocks[1].start_ms, 5000);
        assert!(repair_window(&blocks, 1, 8000).unwrap() <= 8000 - blocks[1].start_ms);
        assert!(fit_speed(
            6500,
            repair_window(&blocks, 0, 8000).unwrap(),
            REPAIR_MAX_SPEED
        )
        .is_some());
        assert!(fit_speed(6500, 2000, 1.15).is_none());
    }
    #[test]
    fn semantic_resynthesis_preserves_text_words_and_bounds_requests() {
        let text = "这是第一个较长的中文分句，需要保持所有知识点。这是第二个较长的中文分句，也需要完整朗读。";
        let chunks = speech_chunks(text);
        assert!(chunks.len() > 1 && chunks.len() <= 8);
        assert_eq!(chunks.concat(), text);
        let english = "Supercalifragilisticexpialidocious".repeat(5);
        assert_eq!(speech_chunks(&english), vec![english]);
        assert_eq!(speech_chunks(&text.repeat(100)).len(), 8);
    }
    #[test]
    fn keeps_absolute_anchors_and_borrows_only_silence() {
        let b = blocks(
            &serde_json::json!({"transcripts":[{"sentences":[
                {"begin_time":10000,"end_time":18000,"text":"First."},
                {"begin_time":19000,"end_time":22000,"text":"Second."}
            ]}]}),
            24000,
        )
        .unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(window(&b, 0, 24000).unwrap(), 8900);
        assert_eq!(b[1].start_ms, 19000);
        assert_eq!(window(&b, 1, 24000).unwrap(), 4000);
    }
    #[test]
    fn rejects_invalid_and_empty_asr() {
        assert!(blocks(&serde_json::json!({"transcripts":[]}), 1000).is_err());
        assert!(blocks(
            &serde_json::json!({"transcripts":[{"sentences":[
                {"begin_time":2000,"end_time":3000,"text":"Outside"}
            ]}]}),
            1000
        )
        .is_err());
    }
    #[test]
    fn limits_speed_without_stretching_short_audio_or_forcing_overlong_speech() {
        assert_eq!(fit_speed(5000, 8000, 1.15), Some(1.0));
        assert!(fit_speed(9500, 8900, 1.15).is_some_and(|r| r > 1.06 && r < 1.08));
        assert_eq!(fit_speed(12000, 8900, 1.15), None);
        assert_eq!(fit_speed(1000, 0, 1.15), None);
        assert_eq!(fit_speed(0, 1000, 1.15), None);
    }
}
