use crate::{app_state::AppState, error::ApiError};
use axum::{
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Default, Deserialize)]
pub(super) struct CacheQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: u32,
}

pub(super) async fn list(
    State(state): State<AppState>,
    Query(q): Query<CacheQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        state
            .store
            .tex_cache_records(&q.q, q.page)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    ))
}

pub(super) async fn detail(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (entry, bytes) = state
        .store
        .verified_tex(&key)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(
        json!({"url_key":key,"tex_sha256":entry.tex_sha256,"revision":entry.revision,
        "size":bytes.len(),"source_job_id":entry.source_job_id,"created_at":entry.created_at,
        "updated_at":entry.updated_at,"observations":state.store.tex_cache_observations(&key).await.map_err(|e|ApiError::Internal(e.to_string()))?}),
    ))
}

pub(super) async fn tex(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let (_, bytes) = state
        .store
        .verified_tex(&key)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::NotFound)?;
    Ok((
        [
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Content-Disposition", "inline; filename=global-notes.tex"),
            ("X-Content-Type-Options", "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

pub(super) fn page(html: String) -> String {
    html.replace("<section class=\"nav-group\"><h2>运行记录与功能</h2>","<section class=\"nav-group\"><h2>运行记录与功能</h2><a href=\"#global_tex\" data-model=\"global_tex\">全局 TeX 缓存</a>")
        .replace("const models = {","const models = {global_tex:{name:'全局 TeX 缓存',app:'运行记录与功能'},")
        .replace("else if (['ai_jobs'", "else if (model==='global_tex') renderGlobalTex(); else if (['ai_jobs'")
        .replace("    function recordLabel()", &format!("{SCRIPT}\n    function recordLabel()"))
}

const SCRIPT: &str = r#"
    let cachePage=1, cacheSearch='', cacheGeneration=0;
    async function renderGlobalTex(){
      const generation=++cacheGeneration;
      $('page-title').textContent='全局 TeX 缓存';$('view').textContent='正在加载…';
      try{
        const result=await api('/api/admin/tex-cache?'+new URLSearchParams({page:cachePage,q:cacheSearch}));
        if(model!=='global_tex'||generation!==cacheGeneration)return;
        const rows=result.records.map(r=>`<tr><td style="overflow-wrap:anywhere;max-width:260px">${escapeHTML(r.url_key)}</td><td>${escapeHTML(r.tex_sha256.slice(0,16))}<br>版本 ${r.revision}</td><td>${r.hit_count} 次<br>关联课程 ${r.observations} 条</td><td>${escapeHTML(date(r.created_at))}<br>${escapeHTML(date(r.updated_at))}<br>来源 ${escapeHTML(r.source_job_id)}</td><td><a href="/api/admin/tex-cache/${r.url_key}/tex" target="_blank" rel="noopener">查看 TeX</a><details data-cache="${r.url_key}"><summary>课程关联详情</summary><pre style="white-space:pre-wrap;max-width:450px"></pre></details></td></tr>`).join('');
        $('view').innerHTML=`<p class="help">仅保存最终 TeX。用户副本独立保存，缓存更新不会同步覆盖历史用户笔记。</p><form id="cache-search" class="toolbar"><input id="cache-q" placeholder="URL 哈希、课程名称或日期" value="${escapeHTML(cacheSearch)}"><button class="button">搜索</button></form><div style="overflow:auto"><table><thead><tr><th>URL 哈希</th><th>内容哈希 / 版本</th><th>复用</th><th>创建 / 更新时间 / 来源</th><th>详情</th></tr></thead><tbody>${rows||'<tr><td colspan="5">暂无缓存</td></tr>'}</tbody></table></div><div class="toolbar"><span>共 ${result.total} 条，第 ${cachePage} 页</span><button id="cache-prev" class="button" ${cachePage<=1?'disabled':''}>上一页</button><button id="cache-next" class="button" ${cachePage*50>=result.total?'disabled':''}>下一页</button></div>`;
        $('cache-search').onsubmit=e=>{e.preventDefault();cacheSearch=$('cache-q').value.trim();cachePage=1;renderGlobalTex();};
        $('cache-prev').onclick=()=>{cachePage--;renderGlobalTex();};$('cache-next').onclick=()=>{cachePage++;renderGlobalTex();};
        document.querySelectorAll('[data-cache]').forEach(el=>el.addEventListener('toggle',async()=>{if(!el.open||el.dataset.loaded)return;const pre=el.querySelector('pre');pre.textContent='正在加载…';try{const detail=await api('/api/admin/tex-cache/'+el.dataset.cache);pre.textContent=JSON.stringify(detail,null,2);el.dataset.loaded='1';}catch(e){pre.textContent=e.message;}}));
      }catch(e){if(model==='global_tex'&&generation===cacheGeneration)$('view').textContent=e.message;}
    }
"#;
