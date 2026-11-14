//! Administrator-controlled AI disclosure; stored globally, never accepted from a TA form.
use super::*;
use crate::services::ta_ai::{Annotation, ANNOTATION_PATH};

pub(super) async fn get(State(state): State<AppState>) -> Result<Json<Annotation>, ApiError> {
    Ok(Json(
        state
            .store
            .load_sidecar::<Annotation>(std::path::Path::new(ANNOTATION_PATH))
            .await
            .map_err(ApiError::Internal)?
            .unwrap_or_default(),
    ))
}
pub(super) async fn save(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(config): Json<Annotation>,
) -> Result<Json<Value>, ApiError> {
    config.validate().map_err(ApiError::BadRequest)?;
    let data = serde_json::to_string(&config)
        .map_err(|_| ApiError::Internal("无法保存 AI 标记设置".into()))?;
    state
        .store
        .save_admin_setting(user.id,std::path::Path::new(ANNOTATION_PATH), &data)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(json!({"success":true})))
}
pub(super) fn page() -> String {
    include_str!("../../templates/admin.html")
        .replace("    </nav>", "<section class=\"nav-group\"><h2>助教批改</h2><a href=\"#ta_ai\" data-model=\"ta_ai\">AI 评语标记</a></section>    </nav>")
        .replace("const models = {", "const models = {ta_ai:{name:'AI 评语标记',app:'助教批改'},")
        .replace("['认证和授权','课程回放'].map", "['认证和授权','课程回放','助教批改'].map")
        .replace("else if (recordId) renderRecord();", "else if (model==='ta_ai') renderTaAi(); else if (recordId) renderRecord();")
        .replace("    function recordLabel()", &format!("{SCRIPT}\n    function recordLabel()"))
}
const SCRIPT: &str = r#"
    let aiSettingsGeneration=0;
    async function renderTaAi() {
      const current=++aiSettingsGeneration;
      $('page-title').textContent='AI 评语标记';
      $('view').textContent='正在加载设置…';
      try {
        const config=await api('/api/admin/ta-ai');
        if (model!=='ta_ai' || current!==aiSettingsGeneration) return;
        $('view').innerHTML=`<form id="ai-config-form" class="change-form"><p id="ai-error" class="errornote" role="alert" hidden></p><fieldset><h2 class="fieldset-title">AI 评语末尾标记</h2>
          <div class="form-row"><label for="ai-enabled">启用标记：</label><div><input id="ai-enabled" type="checkbox" ${config.enabled?'checked':''}><p class="help">适用于严格评分和象征性评分。默认在评语末尾另起一行追加“本评语由 AI 生成。”。</p></div></div>
          <div class="form-row"><label for="ai-format">标记格式：</label><div><textarea id="ai-format" rows="3" maxlength="300">${escapeHTML(config.format)}</textarea><p class="help">纯文本，最多 300 字。支持 {model}（模型）和 {mode}（评分模式）。保存后用于新启动任务，不修改已有草稿。</p><p class="help">预览：</p><pre id="ai-preview" style="white-space:pre-wrap"></pre></div></div></fieldset>
          <div class="submit-row"><button type="submit" class="button default">保存</button></div></form>`;
        const form=$('ai-config-form');
        function preview() { $('ai-preview').textContent='关键步骤正确，请补充边界条件。'+($('ai-enabled').checked?'\n'+$('ai-format').value.trim().replaceAll('{model}','deepseek-flash').replaceAll('{mode}','严格评分'):''); }
        form.addEventListener('input',()=>{formDirty=true;preview();}); form.addEventListener('change',()=>{formDirty=true;preview();}); preview();
        form.addEventListener('submit',async event=>{
          event.preventDefault();if(submitting)return;submitting=true;const button=form.querySelector('button');button.disabled=true;
          try { await api('/api/admin/ta-ai',{enabled:$('ai-enabled').checked,format:$('ai-format').value});formDirty=false;notice('AI 评语标记设置已保存。','success');$('ai-error').hidden=true; }
          catch(error){$('ai-error').textContent=error.message;$('ai-error').hidden=false;}
          finally{submitting=false;button.disabled=false;}
        });
      } catch(error){ if(model==='ta_ai'&&current===aiSettingsGeneration) $('view').textContent=error.message; }
    }
"#;
