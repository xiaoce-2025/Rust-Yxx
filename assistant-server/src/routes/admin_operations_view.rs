//! Extend the existing administration shell without a second frontend runtime.
pub(super) fn page(html: String) -> String {
    html.replace("    </nav>","<section class=\"nav-group\"><h2>运行记录与功能</h2><a href=\"#ai_jobs\" data-model=\"ai_jobs\">助教 AI 批改任务</a><a href=\"#note_files\" data-model=\"note_files\">全部笔记</a><a href=\"#activity\" data-model=\"activity\">登录与操作记录</a><a href=\"#features\" data-model=\"features\">页面功能开关</a></section>    </nav>")
        .replace("['notes','replay','translation'].includes(key)","['notes','replay','translation','ai_jobs','note_files','activity'].includes(key)")
        .replace("const models = {","const models = {ai_jobs:{name:'助教 AI 批改任务',app:'运行记录与功能'},note_files:{name:'全部笔记',app:'运行记录与功能'},activity:{name:'登录与操作记录',app:'运行记录与功能'},features:{name:'页面功能开关',app:'运行记录与功能'},")
        .replace("'课程回放','助教批改'].map","'课程回放','助教批改','运行记录与功能'].map")
        .replace("else if (model==='ta_ai') renderTaAi();","else if (['ai_jobs','note_files','activity','features'].includes(model)) renderOperations(); else if (model==='ta_ai') renderTaAi();")
        .replace("+sortHeading('created_at','创建时间'):","+sortHeading('created_at','创建时间')+sortHeading('last_login_at','最后登录'):")
        .replace("${escapeHTML(new Date(record.created_at).toLocaleDateString('zh-CN'))}</td>`:","${escapeHTML(new Date(record.created_at).toLocaleDateString('zh-CN'))}</td><td>${escapeHTML(date(record.last_login_at))}</td>`:")
        .replace("${readonlyRow('创建时间',date(user.created_at))}","${readonlyRow('创建时间',date(user.created_at))}${readonlyRow('最后登录时间',user.last_login_at?date(user.last_login_at):'暂无记录')}${readonlyRow('账户更新时间',date(user.updated_at))}")
        .replace("    function recordLabel()",&format!("{SCRIPT}\n    function recordLabel()"))
}
const SCRIPT: &str = r#"
    let operationsGeneration=0;
    const operationsState={};
    const operationKinds={'login':'登录成功','note.read':'笔记查看 / 下载','admin.change':'后台修改','admin.settings':'后台设置修改','notes.reprocess':'重新处理笔记','ta.action':'助教操作','ta.ai_start':'启动 AI 批改','ta.ai_cancel':'取消 AI 批改','ta.commit':'同步评分草稿','ta.discard':'丢弃评分草稿','ta.save_bulk':'保存批量评分','ta.save_single':'保存单份评分','user.action':'用户操作'};
    function operationUser(id){return data.users.find(u=>u.id===id)?.username||id||'—';}
    function operationsQuery(){return operationsState[model]||(operationsState[model]={q:'',status:'',user:'',page:1});}
    async function renderOperations(){
      const current=++operationsGeneration,key=model;
      $('page-title').textContent=models[key].name;
      $('view').textContent='正在加载…';
      try{
        if(key==='features'){
          const flags=await api('/api/admin/features');if(model!==key||current!==operationsGeneration)return;
          $('view').innerHTML=`<form id="feature-form" class="change-form"><p id="feature-error" class="errornote" role="alert" hidden></p><fieldset><h2 class="fieldset-title">课程回放批量处理</h2><div class="form-row"><label for="show-reprocess">显示“重新处理”按钮：</label><div><input id="show-reprocess" type="checkbox" ${flags.show_reprocess?'checked':''}><p class="help">关闭后，所有用户的课节列表隐藏此按钮，服务端也拒绝重新处理请求。保存后刷新课程回放页面生效；不影响查看和下载已有笔记。</p></div></div></fieldset><div class="submit-row"><button class="button default" type="submit">保存</button></div></form>`;
          const form=$('feature-form');form.addEventListener('change',()=>formDirty=true);
          form.addEventListener('submit',async event=>{event.preventDefault();if(submitting)return;submitting=true;const button=form.querySelector('button');button.disabled=true;try{await api('/api/admin/features',{show_reprocess:$('show-reprocess').checked});formDirty=false;notice('页面功能设置已保存。','success');$('feature-error').hidden=true;}catch(error){$('feature-error').textContent=error.message;$('feature-error').hidden=false;}finally{submitting=false;button.disabled=false;}});return;
        }
        const state=operationsQuery();
        const params=new URLSearchParams({kind:key,q:state.q,status:state.status,user:state.user,page:String(state.page)});
        const result=await api((key==='note_files'?'/api/admin/notes?':'/api/admin/records?')+params);
        if(model!==key||current!==operationsGeneration)return;
        const statuses=key==='ai_jobs'?[['queued','排队中'],['running','批改中'],['completed','已完成'],['cancelled','已取消'],['interrupted','已中断'],['failed','失败']]:[['200','成功（200）'],['202','已接受（202）'],['400','请求错误（400）'],['401','未登录（401）'],['403','拒绝（403）'],['404','不存在（404）'],['409','冲突（409）'],['423','操作锁定（423）'],['429','限流（429）'],['500','失败（500）']];
        const options=statuses.map(([value,label])=>`<option value="${value}" ${state.status===value?'selected':''}>${label}</option>`).join('');
        const users=data.users.map(u=>`<option value="${u.id}" ${state.user===u.id?'selected':''}>${escapeHTML(u.username)} · ${escapeHTML(u.email)}</option>`).join('');
        const filter=`<form id="operations-search" class="toolbar"><label for="operations-q">搜索：</label><input id="operations-q" maxlength="300" value="${escapeHTML(state.q)}" placeholder="用户 ID、课程、状态或操作"><label for="operations-user">用户：</label><select id="operations-user"><option value="">全部用户</option>${users}</select>${key==='note_files'?'':`<select id="operations-status" aria-label="状态"><option value="">全部状态</option>${options}</select>`}<button class="button" type="submit">搜索</button></form>`;
        const heading=key==='ai_jobs'?'<th>任务 / 用户</th><th>课程 / 作业</th><th>模式 / 状态</th><th>进度与结果</th><th>时间</th>':key==='note_files'?'<th>所属用户</th><th>课程 / 笔记</th><th>课节 / 时间</th><th>查看笔记</th>':'<th>时间</th><th>用户</th><th>操作</th><th>目标</th><th>HTTP 状态</th>';
        const rows=result.records.map(record=>{
          if(key==='ai_jobs')return `<tr><td><span class="monospace">${escapeHTML(record.id)}</span><br>${escapeHTML(operationUser(record.user_id))}</td><td>${escapeHTML(record.course)}<br>${escapeHTML(record.column)}</td><td>${record.mode==='symbolic'?'象征性评分':'严格评分'}<br>${escapeHTML(statusNames[record.status]||({'completed':'已完成','interrupted':'已中断'})[record.status]||record.status)}</td><td>已处理 ${record.processed}/${record.total}<br>草稿 ${record.drafted} · 跳过 ${record.skipped} · 失败 ${record.failed}<details data-ai-detail="${record.id}"><summary>运行详情</summary><p>${escapeHTML(record.message)}</p><div data-ai-failures></div></details></td><td>创建 ${escapeHTML(date(record.created_at))}<br>更新 ${escapeHTML(date(record.updated_at))}<br>结束 ${escapeHTML(date(record.completed_at))}</td></tr>`;
          if(key==='note_files'){
            const query={owner:record.user_id,...(record.kind==='job'?{job:record.job}:{course:record.course,lesson:record.lesson})};
            const link=format=>'/api/admin/note?'+new URLSearchParams({...query,format});
            return `<tr><td>${escapeHTML(record.username)}</td><td>${escapeHTML(record.title)}<br><small>${escapeHTML(record.course||record.job)}</small></td><td>${escapeHTML(record.lesson||'独立任务')}<br>${escapeHTML(record.date||'—')}</td><td>${record.kind==='course'||record.has_tex?`<a href="${escapeHTML(link('tex'))}" target="_blank" rel="noopener">查看 TeX 源文</a>　`:''}${record.kind==='course'||record.has_pdf?`<a href="${escapeHTML(link('pdf'))}" target="_blank" rel="noopener">查看 PDF</a>`:''}</td></tr>`;
          }
          return `<tr><td>${escapeHTML(date(record.created_at))}</td><td>${escapeHTML(operationUser(record.user_id))}</td><td>${escapeHTML(operationKinds[record.kind]||record.kind)}</td><td class="monospace" style="overflow-wrap:anywhere;max-width:450px">${escapeHTML(record.target)}${record.detail&&Object.values(record.detail).some(v=>v!==null)?`<details><summary>详情</summary><pre style="white-space:pre-wrap">${escapeHTML(JSON.stringify(record.detail,null,2))}</pre></details>`:''}</td><td>${record.status}</td></tr>`;
        }).join('');
        const totalPages=Math.max(1,Math.ceil(result.total/result.page_size));
        $('view').innerHTML=filter+(key==='note_files'?'<p class="help">汇总所有用户已生成的课程笔记及独立笔记任务。PDF 按需使用现有编译流程；查看与下载均记录操作者和结果。</p>':key==='activity'?'<p class="help">记录登录、笔记查看/下载、后台修改与用户写入操作；状态轮询不计入。历史访问无法追溯补录，记录不包含密码、API 密钥或正文。</p>':'<p class="help">每次 AI 批改独立保存，重新批改不会覆盖旧记录。只展示运行信息，不展示教师题目或学生作业正文。</p>')+(result.warnings?.length?`<p class="errornote">${escapeHTML(result.warnings.join('；'))}</p>`:'')+`<div style="overflow:auto"><table><thead><tr>${heading}</tr></thead><tbody>${rows||'<tr><td colspan="5">暂无记录</td></tr>'}</tbody></table></div><div class="toolbar"><span>共 ${result.total} 条，第 ${state.page}/${totalPages} 页</span><button class="button" id="operations-prev" ${state.page<=1?'disabled':''}>上一页</button><button class="button" id="operations-next" ${state.page>=totalPages?'disabled':''}>下一页</button></div>`;
        document.querySelectorAll('[data-ai-detail]').forEach(details=>details.addEventListener('toggle',async()=>{if(!details.open||details.dataset.loaded||details.dataset.pending)return;details.dataset.pending='true';const content=details.querySelector('[data-ai-failures]');content.textContent='正在加载…';try{const record=await api('/api/admin/records/'+encodeURIComponent(details.dataset.aiDetail));content.innerHTML='<p>模型：'+escapeHTML(record.model||'deepseek-flash')+'；包含已评分提交：'+(record.include_graded?'是':'否')+'</p>'+(record.failures?.length?'<ul>'+record.failures.map(f=>'<li>'+escapeHTML(f.attempt)+'：'+escapeHTML(f.message)+'</li>').join('')+'</ul>':'<p>无失败项。</p>');details.dataset.loaded='true';}catch(error){content.textContent=error.message;}finally{delete details.dataset.pending;}}));
        $('operations-search').addEventListener('submit',event=>{event.preventDefault();state.q=$('operations-q').value.trim();state.status=$('operations-status')?.value||'';state.user=$('operations-user').value;state.page=1;renderOperations();});
        $('operations-prev').addEventListener('click',()=>{state.page--;renderOperations();});$('operations-next').addEventListener('click',()=>{state.page++;renderOperations();});
      }catch(error){if(model===key&&current===operationsGeneration)$('view').textContent=error.message;}
    }
"#;
