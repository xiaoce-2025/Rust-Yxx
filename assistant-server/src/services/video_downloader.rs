use serde_json::Value;

pub fn get_video_download_url(play_url: &str, login: &super::pkulogin::PkuLoginResult) -> Result<String, String> {
    let cookie_file = login.cookie_file.to_string_lossy();
    let user_agent = &login.user_agent;
    let _guard = login.lock_cookie();
    let result = (|| {
        tracing::info!("开始获取视频下载链接: {}", play_url);

        let first_page = fetch_player_page(play_url, &cookie_file, user_agent)?;

    
        let iframe_src = extract_iframe_src_raw(&first_page);
        let one_time_url = match iframe_src {
            Some(src) => {
                tracing::debug!("找到回放鉴权iframe");
                src
            }
            None => {
                return Err("无法找到iframe链接".to_string());
            }
        };
    
        let one_time_url = reqwest::Url::parse(play_url).and_then(|base|base.join(&one_time_url))
            .map_err(|_|"回放iframe地址无效".to_string())?;
        let redirect_url = follow_redirect(one_time_url.as_str(), &cookie_file, user_agent)?;
        tracing::debug!("已完成回放重定向");
    
        let params = extract_player_params(&redirect_url);

    
        let download_url = fetch_video_download_url(&params, &cookie_file, user_agent)?;
        tracing::info!("最终下载链接: {}", download_url);
    
        Ok(download_url)
    })();
    login.checked(result)
}

fn follow_redirect(url: &str, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    use super::pkulogin::fetch_with_cookie_redirect;
    fetch_with_cookie_redirect(cookie_file, user_agent, url)
}

fn fetch_player_page(url: &str, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    use super::pkulogin::fetch_with_cookie;
    let page = fetch_with_cookie(cookie_file, user_agent, url)?;
    super::pkulogin::validate_page(&page)?;
    Ok(page)
}

fn extract_iframe_src_raw(html: &str) -> Option<String> {
    let document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("iframe[src]").ok()?;
    let frames: Vec<_> = document.select(&selector).collect();
    frames.iter().find(|frame|frame.value().attr("allowfullscreen").is_some())
        .or_else(||frames.iter().find(|frame|frame.value().attr("src").is_some_and(|src|
            src.contains("yjapise.pku.edu.cn") || src.contains("onlineroomse.pku.edu.cn") || src.contains("player.pku.edu.cn"))))
        .and_then(|frame|frame.value().attr("src")).map(str::to_string)
}

fn extract_player_params(url: &str) -> PlayerParams {
    let pairs: std::collections::HashMap<String,String> = reqwest::Url::parse(url).ok()
        .map(|u|u.query_pairs().into_owned().collect()).unwrap_or_default();
    let get = |name: &str| pairs.get(name).map(|s|urlencoding::encode(s).into_owned()).unwrap_or_default();
    PlayerParams { course_id:get("course_id"), sub_id:get("sub_id"), app_id:get("app_id"), auth_data:get("auth_data") }
}

fn fetch_video_download_url(params: &PlayerParams, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    if params.course_id.is_empty() || params.sub_id.is_empty() || params.app_id.is_empty() || params.auth_data.is_empty() {
        return Err("缺少必要参数".to_string());
    }
    
    let api_url = format!(
        "https://yjapise.pku.edu.cn/courseapi/v2/schedule/get-sub-info-by-auth-data?all=1&course_id={}&sub_id={}&with_sub_data=1&app_id={}&auth_data={}",
        params.course_id, params.sub_id, params.app_id, params.auth_data
    );
    
    tracing::debug!("调用视频信息API");
    
    let response = fetch_with_cookie_direct(api_url.as_str(), cookie_file, user_agent)?;

    
    extract_download_url_from_api_response(&response)
}

fn fetch_with_cookie_direct(url: &str, cookie_file: &str, user_agent: &str) -> Result<String, String> {
    use super::pkulogin::fetch_with_cookie;
    let page = fetch_with_cookie(cookie_file, user_agent, url)?;
    super::pkulogin::validate_page(&page)?;
    Ok(page)
}

fn extract_download_url_from_api_response(response: &str) -> Result<String, String> {
    let json: Result<Value, _> = serde_json::from_str(response);

    if let Ok(data) = json {
        if matches!(data.get("code").and_then(Value::as_i64), Some(401 | 403)) {
            return Err(super::pkulogin::AUTH_EXPIRED.into());
        }
        if let Some(list) = data.get("list").and_then(|v| v.as_array()) {
            if let Some(first_item) = list.first() {
                if let Some(sub_content) = first_item.get("sub_content").and_then(|v| v.as_str()) {
                    let film_content: Result<Value, _> = serde_json::from_str(sub_content);

                    if let Ok(film) = film_content {
                        if let Some(playback) = film.get("save_playback") {
                            if let Some(contents) = playback.get("contents").and_then(|v| v.as_str()) {
                                if !contents.is_empty() {
                                    tracing::info!("提取到下载链接: {}", contents);
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
                            if let Some(contents) = playback.get("contents").and_then(|v| v.as_str()) {
                                if !contents.is_empty() {
                                    tracing::info!("提取到下载链接: {}", contents);
                                    return Some(contents.to_string());
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

#[cfg(test)]
mod playback_tests {
    use super::*;
    #[test]
    fn parses_reordered_iframe_attributes_and_html_entities() {
        let html = "<iframe hidden src='/storage'></iframe><iframe src='https://yjapise.pku.edu.cn/casapi/?r=x&amp;sign=y' allowfullscreen></iframe>";
        assert_eq!(extract_iframe_src_raw(html).unwrap(), "https://yjapise.pku.edu.cn/casapi/?r=x&sign=y");
        let params = extract_player_params("https://onlineroomse.pku.edu.cn/player?course_id=1&sub_id=2&app_id=3&auth_data=a%2Bb%3D");
        assert_eq!(params.auth_data,"a%2Bb%3D");
    }
}
