use regex::Regex;
use serde_json::Value;

pub async fn get_video_download_url(play_url: &str, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    tracing::info!("开始获取视频下载链接: {}", play_url);

    let first_page = fetch_player_page(play_url, cookie_file, user_agent).await?;
    save_to_file("/tmp/first_page.html", &first_page).await;
    
    let iframe_src = extract_iframe_src_raw(&first_page);
    let one_time_url = match iframe_src {
        Some(src) => {
            tracing::info!("找到一次性iframe链接: {}", src);
            src
        }
        None => {
            return Err("无法找到iframe链接".to_string());
        }
    };
    
    let redirect_url = follow_redirect(&one_time_url, cookie_file, user_agent).await?;
    tracing::info!("获取到重定向后的URL: {}", redirect_url);
    
    let params = extract_player_params(&redirect_url);
    tracing::info!("提取的参数: {:?}", params);
    
    let download_url = fetch_video_download_url(&params, cookie_file, user_agent).await?;
    tracing::info!("最终下载链接: {}", download_url);
    
    Ok(download_url)
}

async fn follow_redirect(url: &str, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    use super::pkulogin::fetch_with_cookie_redirect;
    fetch_with_cookie_redirect(cookie_file, user_agent, url)
}

async fn fetch_player_page(url: &str, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    use super::pkulogin::fetch_with_cookie;
    fetch_with_cookie(cookie_file, user_agent, url)
}

async fn save_to_file(path: &str, content: &str) {
    match std::fs::write(path, content) {
        Ok(_) => tracing::info!("页面内容已保存到: {}", path),
        Err(e) => tracing::warn!("保存页面失败: {}", e),
    }
}

fn extract_iframe_src_raw(html: &str) -> Option<String> {
    let iframe_re = Regex::new(r#"<iframe[^>]+allowfullscreen="allowfullscreen"[^>]+src="([^"]+)"[^>]*>"#).unwrap();
    
    if let Some(captures) = iframe_re.captures(html) {
        let src = captures[1].to_string();
        tracing::info!("找到带allowfullscreen的iframe: {}", src);
        return Some(src);
    }
    
    tracing::warn!("未找到带allowfullscreen属性的iframe，尝试其他方式");
    let iframe_re2 = Regex::new(r#"<iframe[^>]+src="([^"]+)"[^>]*>"#).unwrap();
    if let Some(captures) = iframe_re2.captures(html) {
        let src = captures[1].to_string();
        if src.contains("player.pku.edu.cn") || src.contains("yjapise.pku.edu.cn") {
            tracing::info!("找到iframe: {}", src);
            return Some(src);
        }
    }
    
    None
}

fn extract_player_params(url: &str) -> PlayerParams {
    let course_id_re = Regex::new(r"course_id=([^&]+)").unwrap();
    let sub_id_re = Regex::new(r"sub_id=([^&]+)").unwrap();
    let app_id_re = Regex::new(r"app_id=([^&]+)").unwrap();
    let auth_data_re = Regex::new(r"auth_data=([^&]+)").unwrap();
    
    let course_id = course_id_re.captures(url).map(|c| c[1].to_string()).unwrap_or_default();
    let sub_id = sub_id_re.captures(url).map(|c| c[1].to_string()).unwrap_or_default();
    let app_id = app_id_re.captures(url).map(|c| c[1].to_string()).unwrap_or_default();
    let auth_data = auth_data_re.captures(url).map(|c| c[1].to_string()).unwrap_or_default();
    
    PlayerParams {
        course_id,
        sub_id,
        app_id,
        auth_data,
    }
}

async fn fetch_video_download_url(params: &PlayerParams, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    if params.course_id.is_empty() || params.sub_id.is_empty() || params.app_id.is_empty() || params.auth_data.is_empty() {
        return Err("缺少必要参数".to_string());
    }
    
    let api_url = format!(
        "https://yjapise.pku.edu.cn/courseapi/v2/schedule/get-sub-info-by-auth-data?all=1&course_id={}&sub_id={}&with_sub_data=1&app_id={}&auth_data={}",
        params.course_id, params.sub_id, params.app_id, params.auth_data
    );
    
    tracing::info!("调用视频信息API: {}", api_url);
    
    let response = fetch_with_cookie_direct(api_url.as_str(), cookie_file, user_agent).await?;
    save_to_file("/tmp/video_api_response.json", &response).await;
    
    extract_download_url_from_api_response(&response)
}

async fn fetch_with_cookie_direct(url: &str, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    use super::pkulogin::fetch_with_cookie;
    fetch_with_cookie(cookie_file, user_agent, url)
}

fn extract_download_url_from_api_response(response: &str) -> Result<String, String> {
    let json: Result<Value, _> = serde_json::from_str(response);
    
    if let Ok(data) = json {
        if let Some(list) = data.get("list").and_then(|v| v.as_array()) {
            if let Some(first_item) = list.first() {
                if let Some(sub_content) = first_item.get("sub_content").and_then(|v| v.as_str()) {
                    let film_content: Result<Value, _> = serde_json::from_str(sub_content);
                    
                    if let Ok(film) = film_content {
                        if let Some(playback) = film.get("save_playback") {
                            if let Some(is_m3u8) = playback.get("is_m3u8").and_then(|v| v.as_str()) {
                                if is_m3u8 == "yes" || is_m3u8 == "1" {
                                    if let Some(contents) = playback.get("contents").and_then(|v| v.as_str()) {
                                        let m3u8_pattern = Regex::new(r"https://resourcese\.pku\.edu\.cn/play/0/harpocrates/\d+/\d+/\d+/([a-zA-Z0-9]+)(/.+)/playlist\.m3u8.*").unwrap();
                                        if let Some(match_result) = m3u8_pattern.captures(contents) {
                                            let hash = match_result.get(1).map(|m| m.as_str()).unwrap_or("");
                                            let download_url = format!("https://course.pku.edu.cn/webapps/bb-streammedia-hqy-BBLEARN/downloadVideo.action?resourceId={}", hash);
                                            return Ok(download_url);
                                        }
                                        
                                        tracing::info!("m3u8链接未匹配模式，尝试直接使用contents");
                                        return Ok(contents.to_string());
                                    }
                                } else {
                                    if let Some(contents) = playback.get("contents").and_then(|v| v.as_str()) {
                                        return Ok(contents.to_string());
                                    }
                                }
                            } else {
                                if let Some(contents) = playback.get("contents").and_then(|v| v.as_str()) {
                                    return Ok(contents.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    Err("无法从API响应中提取下载链接".to_string())
}

#[derive(Debug)]
struct PlayerParams {
    course_id: String,
    sub_id: String,
    app_id: String,
    auth_data: String,
}

pub async fn extract_download_url_from_sub_info(sub_info_json: &str) -> Option<String> {
    let sub_info: Result<Value, _> = serde_json::from_str(sub_info_json);
    
    if let Ok(info) = sub_info {
        if let Some(list) = info.get("list").and_then(|v| v.as_array()) {
            if let Some(first_item) = list.first() {
                if let Some(sub_content) = first_item.get("sub_content").and_then(|v| v.as_str()) {
                    let film_content: Result<Value, _> = serde_json::from_str(sub_content);
                    
                    if let Ok(film) = film_content {
                        if let Some(playback) = film.get("save_playback") {
                            if let Some(is_m3u8) = playback.get("is_m3u8").and_then(|v| v.as_str()) {
                                if is_m3u8 == "yes" {
                                    if let Some(contents) = playback.get("contents").and_then(|v| v.as_str()) {
                                        let m3u8_pattern = Regex::new(r"https://resourcese\.pku\.edu\.cn/play/0/harpocrates/\d+/\d+/\d+/([a-zA-Z0-9]+)(/.+)/playlist\.m3u8.*").unwrap();
                                        if let Some(match_result) = m3u8_pattern.captures(contents) {
                                            let hash = match_result.get(1).map(|m| m.as_str()).unwrap_or("");
                                            return Some(format!("https://course.pku.edu.cn/webapps/bb-streammedia-hqy-BBLEARN/downloadVideo.action?resourceId={}", hash));
                                        }
                                    }
                                } else {
                                    if let Some(contents) = playback.get("contents").and_then(|v| v.as_str()) {
                                        return Some(contents.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    None
}
