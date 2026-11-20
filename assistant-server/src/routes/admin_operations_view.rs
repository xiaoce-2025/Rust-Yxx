//! Extend the existing administration shell without a second frontend runtime.
pub(super) fn page(html: String) -> String {
    html.replace("    </nav>","<section class=\"nav-group\"><h2>运行记录与功能</h2><a href=\"#ai_jobs\" data-model=\"ai_jobs\">助教 AI 批改任务</a><a href=\"#note_files\" data-model=\"note_files\">全部笔记</a><a href=\"#activity\" data-model=\"activity\">登录与操作记录</a><a href=\"#credits\" data-model=\"credits\">希元流水</a><a href=\"#features\" data-model=\"features\">页面功能开关</a><a href=\"#acceleration\" data-model=\"acceleration\">加速券</a></section>    </nav>")
        .replace("['notes','replay','translation'].includes(key)","['notes','replay','translation','ai_jobs','note_files','activity','credits'].includes(key)")
        .replace("const models = {","const models = {ai_jobs:{name:'助教 AI 批改任务',app:'运行记录与功能'},note_files:{name:'全部笔记',app:'运行记录与功能'},activity:{name:'登录与操作记录',app:'运行记录与功能'},credits:{name:'希元流水',app:'运行记录与功能'},features:{name:'页面功能开关',app:'运行记录与功能'},acceleration:{name:'加速券',app:'运行记录与功能'},")
        .replace("'课程回放','助教批改'].map","'课程回放','助教批改','运行记录与功能'].map")
        .replace("else if (model==='ta_ai') renderTaAi();","else if (['ai_jobs','note_files','activity','credits','features','acceleration'].includes(model)) renderOperations(); else if (model==='ta_ai') renderTaAi();")
        .replace("+sortHeading('created_at','创建时间'):","+sortHeading('created_at','创建时间')+sortHeading('last_login_at','最后登录'):")
        .replace("${escapeHTML(new Date(record.created_at).toLocaleDateString('zh-CN'))}</td>`:","${escapeHTML(new Date(record.created_at).toLocaleDateString('zh-CN'))}</td><td>${escapeHTML(date(record.last_login_at))}</td>`:")
        .replace("${readonlyRow('创建时间',date(user.created_at))}","${readonlyRow('创建时间',date(user.created_at))}${readonlyRow('最后登录时间',user.last_login_at?date(user.last_login_at):'暂无记录')}${readonlyRow('账户更新时间',date(user.updated_at))}")
        .replace("location.hash.slice(1).split('/')","location.hash.slice(1).split('?')[0].split('/')")
        .replace("      if (data) { if (model==='index')", "      applyOperationsLink(); if (data) { if (model==='index')")
        .replace("date(user.updated_at))}", "date(user.updated_at))}${userAuditLinks(user.id)}")
        .replace("readonlyRow('失败码',job.failure_code||'—')","readonlyRow('失败码',job.failure_code||'—')+jobAuditLinks(job.id)")
        .replace("    function recordLabel()",&format!("{SCRIPT}\n    function recordLabel()"))
}
const SCRIPT: &str = r##"
    let operationsGeneration=0;
    const operationsState={};
    const operationKinds={'login':'登录成功','auth.request':'认证请求','ta.attachment':'查看作业附件','admin.read':'后台查看','note.read':'笔记查看 / 下载','admin.change':'后台修改','admin.settings':'后台设置修改','notes.reprocess':'重新处理笔记','notes.batch_item':'批量提交课节','ta.action':'助教操作','ta.ai_start':'启动 AI 批改','ta.ai_cancel':'取消 AI 批改','ta.commit':'同步评分草稿','ta.discard':'丢弃评分草稿','ta.save_bulk':'保存批量评分','ta.save_single':'保存单份评分','user.action':'用户操作'};
    const creditKinds={ADMIN_GRANT:'管理员发放',JOB_CHARGE:'笔记扣费',ADMIN_ADJUST:'管理员调整',RECHARGE:'希元充值',ACCELERATION_PURCHASE:'购买加速券'};
    function operationsLink(key, params){
      return '#'+key+'?'+new URLSearchParams(params);
    }
    function userAuditLinks(id){
      return `<div class="form-row"><span>账户记录</span><div><a href="${operationsLink('activity',{user:id})}">登录与操作记录</a>　<a href="${operationsLink('credits',{user:id})}">希元流水</a></div></div>`;
    }
    function jobAuditLinks(id){
      return `<div class="form-row"><span>任务记录</span><div><a href="${operationsLink('activity',{q:id})}">操作记录</a>　<a href="${operationsLink('credits',{q:id})}">希元流水</a></div></div>`;
    }
    function operationUser(id, users, fallback='—'){
      if(!id)return fallback;
      const user=users[id];
      return user?`<a href="#users/${encodeURIComponent(id)}">${escapeHTML(user)}</a><br><small class="monospace">${escapeHTML(id)}</small>`:`<a href="#users/${encodeURIComponent(id)}">${escapeHTML(id)}</a>`;
    }
    function operationsQuery(){return operationsState[model]||(operationsState[model]={q:'',status:'',user:'',event:'',from:'',before:'',page:1});}
    let appliedOperationsHash;
    function applyOperationsLink(){
      const hash=location.hash;
      if(hash===appliedOperationsHash)return;
      appliedOperationsHash=hash;
      if(!['activity','credits'].includes(model))return;
      const params=new URLSearchParams(hash.split('?')[1]);
      if(!params.has('q')&&!params.has('user'))return;
      delete operationsState[model];
      Object.assign(operationsQuery(),{q:params.get('q')||'',user:params.get('user')||''});
    }
    function renderAccelerationForm(settings){
      const fields=[['min','下界（分钟）',0,10080,1],['max','上界（分钟）',0,10080,1],
        ['mean','均值（分钟）',0,10080,'any'],['variance','方差（分钟²）',0,100000000,'any']];
      const row=(name,label,value,min,max,step)=>`<div class="form-row"><label for="${name}">${label}</label><input id="${name}" name="${name}" type="number" min="${min}" max="${max}" step="${step}" value="${escapeHTML(value)}" required></div>`;
      $('view').innerHTML=`<form id="acceleration-form" class="change-form">
        <p class="errornote" role="alert" hidden></p>
        <fieldset><h2 class="fieldset-title">商品</h2>${row('price','每张售价（希元点）',settings.price,1,1000000,1)}</fieldset>
        <fieldset><h2 class="fieldset-title">单个任务处理耗时</h2>
        ${fields.map(([name,title,min,max,step])=>row(name,title,settings.processing_time[name],min,max,step)).join('')}
        <p class="help">每个任务按设定区间抽取一次处理耗时。加速前后按各自的队列顺序和可用处理位计算等待时间。</p></fieldset>
        <div class="submit-row"><button class="button default" type="submit">保存</button></div>
      </form>`;
      const form=$('acceleration-form');
      form.addEventListener('input',()=>formDirty=true);
      form.addEventListener('submit',async event=>{
        event.preventDefault();
        if(submitting)return;
        const button=form.querySelector('button');
        const errorMessage=form.querySelector('[role="alert"]');
        const read=key=>form.elements.namedItem(key).valueAsNumber;
        submitting=true;
        button.disabled=true;
        errorMessage.hidden=true;
        try{
          const processing_time=Object.fromEntries(fields.map(([name])=>[name,read(name)]));
          await api('/api/admin/acceleration',{price:read('price'),processing_time});
          if(form.isConnected){formDirty=false;notice('加速券设置已保存。','success');}
        }catch(error){
          errorMessage.textContent=error.message;
          errorMessage.hidden=false;
        }finally{
          submitting=false;
          button.disabled=false;
        }
      });
    }
    async function renderOperations(){
      const current=++operationsGeneration,key=model;
      $('page-title').textContent=models[key].name;
      $('view').textContent='正在加载…';
      try{
        if(key==='acceleration'){
          const settings=await api('/api/admin/acceleration');
          if(model===key&&current===operationsGeneration)renderAccelerationForm(settings);
          return;
        }
        if(key==='features'){
          const flags=await api('/api/admin/features');if(model!==key||current!==operationsGeneration)return;
          $('view').innerHTML=`<form id="feature-form" class="change-form"><p id="feature-error" class="errornote" role="alert" hidden></p><fieldset><h2 class="fieldset-title">课程回放批量处理</h2><div class="form-row"><label for="show-reprocess">显示“重新处理”按钮：</label><div><input id="show-reprocess" type="checkbox" ${flags.show_reprocess?'checked':''}><p class="help">关闭后，所有用户的课节列表隐藏此按钮，服务端也拒绝重新处理请求。保存后刷新课程回放页面生效；不影响查看和下载已有笔记。</p></div></div></fieldset><div class="submit-row"><button class="button default" type="submit">保存</button></div></form>`;
          const form=$('feature-form');form.addEventListener('change',()=>formDirty=true);
          form.addEventListener('submit',async event=>{event.preventDefault();if(submitting)return;submitting=true;const button=form.querySelector('button');button.disabled=true;try{await api('/api/admin/features',{show_reprocess:$('show-reprocess').checked});formDirty=false;notice('页面功能设置已保存。','success');$('feature-error').hidden=true;}catch(error){$('feature-error').textContent=error.message;$('feature-error').hidden=false;}finally{submitting=false;button.disabled=false;}});return;
        }
        const state=operationsQuery();
        const params=new URLSearchParams({kind:key,q:state.q,status:state.status,user:state.user,page:String(state.page)});
        if(state.event)params.set('event',state.event);
        for(const field of ['from','before']){
          if(state[field])params.set(field,new Date(state[field]).toISOString());
        }
        const result=await api((key==='note_files'?'/api/admin/notes?':'/api/admin/records?')+params);
        if(model!==key||current!==operationsGeneration)return;
        state.page=result.page;
        const statuses=key==='ai_jobs'?[['queued','排队中'],['running','批改中'],['completed','已完成'],['cancelled','已取消'],['interrupted','已中断'],['failed','失败']]:key==='credits'?Object.entries(creditKinds):[['0','待核对'],['200','200 成功'],['201','201 已创建'],['202','202 已接受'],['204','204 成功'],['206','206 部分内容'],['303','303 跳转'],['400','400 请求错误'],['401','401 未认证'],['403','403 拒绝'],['404','404 不存在'],['409','409 冲突'],['422','422 参数无效'],['423','423 操作锁定'],['429','429 限流'],['500','500 服务错误'],['503','503 暂不可用']];
        const optionList=(pairs,selected)=>pairs.map(([value,label])=>`<option value="${escapeHTML(value)}" ${selected===value?'selected':''}>${escapeHTML(label)}</option>`).join('');
        const userIds=new Set(),jobIds=new Set();
        for (const record of result.records) {
          for (const id of [record.user_id,record.subject_id,record.actor_id]) if (id) userIds.add(id);
          for (const id of [record.job_id,record.detail?.job_id,record.detail?.result_job_id]) if (id) jobIds.add(id);
        }
        const refs=await api('/api/admin/related?'+new URLSearchParams({users:[...userIds].join(','),jobs:[...jobIds].join(',')}));
        if(model!==key||current!==operationsGeneration)return;
        const activity=key==='activity',credits=key==='credits';
        const timeFilter=(name,label)=>`<label for="operations-${name}">${label}</label><input id="operations-${name}" type="datetime-local" value="${escapeHTML(state[name])}">`;
        const filter=`<form id="operations-search" class="toolbar" style="flex-wrap:wrap"><label for="operations-q">搜索</label><input id="operations-q" maxlength="300" value="${escapeHTML(state.q)}" placeholder="${activity?'记录编号（#123）、任务 ID 或操作':credits?'流水 ID、任务 ID 或发放原因':'ID 或关键词'}"><label for="operations-user">相关用户</label><input id="operations-user" list="operations-users" maxlength="36" value="${escapeHTML(state.user)}" placeholder="用户 ID"><datalist id="operations-users"></datalist>${activity?`<select id="operations-event" aria-label="操作类型"><option value="">全部操作</option>${optionList(Object.entries(operationKinds),state.event)}</select>`:''}${key==='note_files'?'':`<select id="operations-status" aria-label="${credits?'流水类型':'状态'}"><option value="">${credits?'全部类型':'全部状态'}</option>${optionList(statuses,state.status)}</select>`}${activity||credits?timeFilter('from','起始时间')+timeFilter('before','截止时间'):''}<button class="button" type="submit">搜索</button></form>`;
        const heading=key==='ai_jobs'?'<th>任务 / 用户</th><th>课程 / 作业</th><th>模式 / 状态</th><th>进度与结果</th><th>时间</th>':key==='note_files'?'<th>所属用户</th><th>课程 / 笔记</th><th>课节 / 时间</th><th>查看笔记</th>':credits?'<th>流水 / 时间</th><th>账户 / 经办人</th><th>类型</th><th>变动 / 余额</th><th>计费依据</th>':'<th>记录 / 时间</th><th>操作者 / 涉及账户</th><th>操作</th><th>目标</th><th>HTTP 状态</th>';
        const rows=result.records.map(record=>{
          if(key==='ai_jobs')return `<tr><td><span class="monospace">${escapeHTML(record.id)}</span><br>${operationUser(record.user_id,refs.users)}</td><td>${escapeHTML(record.course)}<br>${escapeHTML(record.column)}</td><td>${record.mode==='symbolic'?'象征性评分':'严格评分'}<br>${escapeHTML(statusNames[record.status]||({'completed':'已完成','interrupted':'已中断'})[record.status]||record.status)}</td><td>已处理 ${record.processed}/${record.total}<br>草稿 ${record.drafted} · 跳过 ${record.skipped} · 失败 ${record.failed}<details data-ai-detail="${record.id}"><summary>运行详情</summary><p>${escapeHTML(record.message)}</p><div data-ai-failures></div></details></td><td>创建 ${escapeHTML(date(record.created_at))}<br>更新 ${escapeHTML(date(record.updated_at))}<br>结束 ${escapeHTML(date(record.completed_at))}</td></tr>`;
          if(key==='note_files'){
            const query={owner:record.user_id,...(record.kind==='job'?{job:record.job}:{course:record.course,lesson:record.lesson})};
            const link=format=>'/api/admin/note?'+new URLSearchParams({...query,format});
            return `<tr><td>${escapeHTML(record.username)}</td><td>${escapeHTML(record.title)}<br><small>${escapeHTML(record.course||record.job)}</small></td><td>${escapeHTML(record.lesson||'独立任务')}<br>${escapeHTML(record.date||'—')}</td><td>${record.kind==='course'||record.has_tex?`<a href="${escapeHTML(link('tex'))}" target="_blank" rel="noopener">查看 TeX 源文</a>　`:''}${record.kind==='course'||record.has_pdf?`<a href="${escapeHTML(link('pdf'))}" target="_blank" rel="noopener">查看 PDF</a>`:''}</td></tr>`;
          }
          if(credits){
            const job=record.job_id;
            const existingJob=refs.jobs[job]==='notes';
            const reference=existingJob?`<a href="#notes/${encodeURIComponent(job)}">${escapeHTML(job)}</a>`:escapeHTML(job||'');
            return `<tr><td>${escapeHTML(date(record.created_at))}<br><small class="monospace">${escapeHTML(record.id)}</small></td><td>${operationUser(record.user_id,refs.users)}<br>经办：${operationUser(record.actor_id,refs.users,'系统')}</td><td>${escapeHTML(creditKinds[record.event_type]||record.event_type)}</td><td>${record.delta>0?'+':''}${escapeHTML(record.delta)} 点<br>余额 ${escapeHTML(record.balance_after)} 点</td><td style="overflow-wrap:anywhere;max-width:420px">${job?`任务 ${reference}<br><a href="${operationsLink('activity',{q:job})}">查操作记录</a><br>`:''}${record.duration_seconds!=null?`时长 ${escapeHTML(record.duration_seconds)} 秒 · ${escapeHTML(record.cost_units)} 点<br>`:''}${escapeHTML(record.reason||'')}${record.actor_id?`<br><a href="${operationsLink('activity',{q:record.id})}">查发放操作</a>`:''}</td></tr>`;
          }
          const detail=record.detail||{};
          const jobId=detail.result_job_id||detail.job_id;
          const jobKind=refs.jobs[jobId];
          const jobLinks=jobId?`<br>任务 ${jobKind?`<a href="#${jobKind}/${encodeURIComponent(jobId)}">${escapeHTML(jobId)}</a>`:escapeHTML(jobId)}${jobKind==='notes'?`　<a href="${operationsLink('credits',{q:jobId})}">查希元流水</a>`:''}`:'';
          return `<tr><td><a class="monospace" href="${operationsLink('activity',{q:'#'+record.id})}">#${escapeHTML(record.id)}</a><br>${escapeHTML(date(record.created_at))}</td><td>${operationUser(record.user_id,refs.users,'未确认')}${record.subject_id?`<br>涉及：${operationUser(record.subject_id,refs.users)}`:''}</td><td>${escapeHTML(operationKinds[record.kind]||record.kind)}</td><td class="monospace" style="overflow-wrap:anywhere;max-width:450px">${escapeHTML(record.target)}${jobLinks}${detail.ledger_id?`<br><a href="${operationsLink('credits',{q:detail.ledger_id})}">查关联流水</a>`:''}${detail.parent_audit_id?`<br><a href="${operationsLink('activity',{q:'#'+detail.parent_audit_id})}">查看批次请求</a>`:''}${Object.values(detail).some(v=>v!==null)?`<details><summary>详情</summary><pre style="white-space:pre-wrap">${escapeHTML(JSON.stringify(detail,null,2))}</pre></details>`:''}</td><td>${record.status===0?'待核对':escapeHTML(record.status)}</td></tr>`;
        }).join('');
        const totalPages=Math.max(1,Math.ceil(result.total/result.page_size));
        $('view').innerHTML=filter+(key==='note_files'?'<p class="help">汇总所有用户已生成的课程笔记及独立笔记任务。PDF 按需使用现有编译流程；查看与下载均记录操作者和结果。</p>':key==='activity'?'<p class="help">待核对表示已登记请求，执行结果仍未写入。时间筛选按本地时间，截止时间不包含该时刻。</p>':key==='credits'?'<p class="help">按账户或任务 ID 查询实际入账的希元变动。</p>':'<p class="help">每次 AI 批改独立保存，重新批改不会覆盖旧记录。只展示运行信息，不展示教师题目或学生作业正文。</p>')+(result.warnings?.length?`<p class="errornote">${escapeHTML(result.warnings.join('；'))}</p>`:'')+`<div style="overflow:auto"><table><thead><tr>${heading}</tr></thead><tbody>${rows||'<tr><td colspan="5">暂无记录</td></tr>'}</tbody></table></div><div class="toolbar"><span>共 ${result.total} 条，第 ${state.page}/${totalPages} 页</span><button class="button" id="operations-prev" ${state.page<=1?'disabled':''}>上一页</button><button class="button" id="operations-next" ${state.page>=totalPages?'disabled':''}>下一页</button></div>`;
        document.querySelectorAll('[data-ai-detail]').forEach(details=>details.addEventListener('toggle',async()=>{if(!details.open||details.dataset.loaded||details.dataset.pending)return;details.dataset.pending='true';const content=details.querySelector('[data-ai-failures]');content.textContent='正在加载…';try{const record=await api('/api/admin/records/'+encodeURIComponent(details.dataset.aiDetail));content.innerHTML='<p>模型：'+escapeHTML(record.model||'deepseek-flash')+'；包含已评分提交：'+(record.include_graded?'是':'否')+'</p>'+(record.failures?.length?'<ul>'+record.failures.map(f=>'<li>'+escapeHTML(f.attempt)+'：'+escapeHTML(f.message)+'</li>').join('')+'</ul>':'<p>无失败项。</p>');details.dataset.loaded='true';}catch(error){content.textContent=error.message;}finally{delete details.dataset.pending;}}));
        $('operations-search').addEventListener('submit',event=>{event.preventDefault();state.q=$('operations-q').value.trim();state.status=$('operations-status')?.value||'';state.user=$('operations-user').value;state.event=activity?$('operations-event').value:'';state.from=activity||credits?$('operations-from').value:'';state.before=activity||credits?$('operations-before').value:'';state.page=1;renderOperations();});
        $('operations-prev').addEventListener('click',()=>{state.page--;renderOperations();});$('operations-next').addEventListener('click',()=>{state.page++;renderOperations();});
      }catch(error){if(model===key&&current===operationsGeneration)$('view').textContent=error.message;}
    }
"##;
