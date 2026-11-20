//! HTML/CSS rendered by Rust; no frontend build or separate grading runtime.
use super::{visible_attempts, Selection, Workspace};
use crate::services::ta::{Attempt, Draft};
use std::fmt::Write;

fn e(value: &str) -> String {
    html_escape::encode_double_quoted_attribute(value).into_owned()
}
fn hidden(name: &str, value: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
        e(name),
        e(value)
    )
}
fn context(s: &Selection, csrf: &str) -> String {
    [
        ("csrf", csrf),
        ("course", &s.course),
        ("column", &s.column),
        ("filter", &s.filter),
        ("search", &s.search),
        ("review", &s.review),
    ]
    .iter()
    .map(|(k, v)| hidden(k, v))
    .collect()
}
fn refresh(s: &Selection, csrf: &str, action: &str, label: &str) -> String {
    format!("<form method=\"post\" action=\"/ta/action\">{}{}<button class=\"button ghost\" name=\"action\" value=\"{}\" type=\"submit\">{}</button></form>",
        context(s, csrf), hidden("attempt", &s.attempt), e(action), e(label))
}
fn selected(a: &str, b: &str) -> &'static str {
    if a == b {
        " selected"
    } else {
        ""
    }
}
fn draft_for<'a>(w: &'a Workspace, a: &Attempt) -> Option<&'a Draft> {
    w.drafts.iter().find(|d| d.attempt_id == a.id && !d.synced)
}
fn date(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|d| {
            d.with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap())
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or(value.into())
}
pub(super) fn render(s: &Selection, w: &Workspace, csrf: &str) -> String {
    wrap(&fragment(s, w, csrf), false)
}
pub(super) fn shell() -> String {
    wrap(
        r#"<div class="ta-workspace"><aside class="course-panel"><h1>助教作业批改</h1><p class="muted">正在加载助教工作台</p><div class="ta-skeleton"></div></aside><section class="ta-content" aria-label="作业选择"></section></div>"#,
        true,
    )
}
fn wrap(content: &str, loading: bool) -> String {
    let mut html = String::from("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>「严小希」小助手-助教批改</title><style>");
    html.push_str(include_str!("../../templates/replay-hero.css"));
    html.push_str(CSS);
    html.push_str(ASYNC_CSS);
    html.push_str("</style></head><body><main>");
    html.push_str("<div id=\"ta-load-status\" class=\"ta-load-status\" role=\"status\" aria-live=\"polite\"></div>");
    let _ = write!(html, "<div id=\"ta-workspace-region\" data-loading=\"{loading}\" aria-busy=\"{loading}\">{content}</div>");
    html.push_str("<noscript>请启用 JavaScript，以在页面内加载课程和批改作业。</noscript></main>");
    html.push_str(SCRIPT);
    html.push_str("<!-- site-footer --></body></html>");
    html
}
pub(super) fn fragment(s: &Selection, w: &Workspace, csrf: &str) -> String {
    let mut html = String::new();
    if s.column.is_empty() {
        course_picker(&mut html, s, w, csrf);
    } else {
        assignment_workspace(&mut html, s, w, csrf);
    }
    html
}

fn workspace_messages(html: &mut String, s: &Selection, w: &Workspace) {
    if let Some(error) = &w.error {
        let _ = write!(html, r#"<div class="alert error" role="alert">{}</div>"#, e(error));
    }
    if !s.notice.is_empty() {
        let _ = write!(html, r#"<div class="alert success" role="status">{}</div>"#, e(&s.notice));
    }
}

fn course_picker(html: &mut String, s: &Selection, w: &Workspace, csrf: &str) {
    html.push_str(r#"<div class="ta-workspace"><aside class="course-panel"><div class="panel-heading"><h2>我的助教课程</h2>"#);
    html.push_str(&refresh(s, csrf, "refresh_courses", "刷新"));
    html.push_str(r#"</div><nav class="course-list" aria-label="助教课程">"#);
    for c in &w.courses {
        let target = Selection {
            course: c.id.clone(),
            ..Default::default()
        }
        .url();
        let _ = write!(html,"<a class=\"course-item{}\" href=\"{}\"{}><span class=\"course-symbol\" aria-hidden=\"true\">▤</span><span><strong>{}</strong><small>{}</small></span></a>",
            if c.id == s.course {" active"} else {""},e(&target),if c.id == s.course {" aria-current=\"page\""} else {""},e(&c.name),e(&c.term));
    }
    if w.courses.is_empty() {
        html.push_str("<div class=\"empty compact\">暂无助教课程<br><small>可点击刷新重新读取教学网课程。</small></div>");
    }
    if w.courses.is_empty() {
        html.push_str(r#"<p class="muted">暂无助教课程</p>"#);
    }
    html.push_str("</nav>");
    if s.course.is_empty() {
        workspace_messages(html, s, w);
    }
    html.push_str(r#"</aside><section class="ta-content" aria-label="作业选择">"#);
    if !s.course.is_empty() {
        workspace_messages(html, s, w);
        if let Some(course) = w.courses.iter().find(|c| c.id == s.course) {
            let _ = write!(html, r#"<div class="section-header"><h2>{}</h2>{}</div>"#,
                e(&course.name), refresh(s, csrf, "refresh", "刷新作业"));
            if !w.queue_cached {
                html.push_str(r#"<p class="muted">点击右上角「刷新作业」，从教学网读取这门课的待评分队列。</p>"#);
            } else if w.assignments.is_empty() {
                html.push_str(r#"<p class="muted">上次刷新时，这门课程没有待评分作业。</p>"#);
            }
            if !w.assignments.is_empty() {
                html.push_str(r#"<div class="assignment-list">"#);
                for assignment in &w.assignments {
                    let target = Selection {
                        course: s.course.clone(), column: assignment.id.clone(), ..Default::default()
                    };
                    let (count, label) = if assignment.attempts.is_empty() {
                        (String::new(), "核对草稿")
                    } else {
                        (format!("{} 份待评分", assignment.attempts.len()), "进入批改")
                    };
                    let _ = write!(html, r#"<div class="assignment-row card"><div><h3>{}</h3><small>{}</small></div>{}</div>"#,
                        e(&assignment.name), e(&count), refresh(&target, csrf, "enter_assignment", label));
                }
                html.push_str("</div>");
            }
        }
    }
    html.push_str("</section></div>");
}

fn assignment_workspace(html: &mut String, s: &Selection, w: &Workspace, csrf: &str) {
    let course = Selection { course: s.course.clone(), ..Default::default() };
    let course_name = w.courses.iter().find(|c| c.id == s.course).map(|c| c.name.as_str()).unwrap_or("助教课程");
    let assignment = w.assignments.iter().find(|a| a.id == s.column);
    let name = assignment.map(|a| a.name.as_str()).unwrap_or("作业批改");
    let pending = w.drafts.iter().filter(|d| !d.synced).count();
    let _ = write!(html, r#"<section class="assignment-workspace"><header class="assignment-header"><a class="button ghost" href="{}">← 作业选择</a><div><h1>{}</h1><p>{}</p></div><div class="assignment-counts"><span class="badge">{} 待评分</span><span class="badge">{pending} 待同步</span>{}</div></header>"#,
        e(&course.url()), e(course_name), e(name), w.attempts.len(), refresh(s, csrf, "refresh", "刷新"));
    workspace_messages(html, s, w);
    if assignment.is_none() {
        html.push_str(r#"<p class="muted">这项作业当前没有待评分提交或待核对草稿。</p></section>"#);
        return;
    }
    let mut review = s.clone();
    review.review = "1".into();
    review.notice.clear();
    let mut work = s.clone();
    work.review.clear();
    work.notice.clear();
    let _ = write!(html, r#"<nav class="workspace-tabs" aria-label="批改步骤"><a href="{}"{}>批改与草稿</a><a href="{}"{}>核对并同步</a></nav>"#,
        e(&work.url()), if s.review.is_empty() { r#" aria-current="page""# } else { "" },
        e(&review.url()), if s.review == "1" { r#" aria-current="page""# } else { "" });
    if s.review == "1" {
        review_panel(html, s, w, csrf);
    } else {
        grading_panel(html, s, w, csrf);
    }
    html.push_str("</section>");
}

pub(super) fn ai_status(job: Option<&crate::services::ta_ai::Job>, s: &Selection) -> String {
    let Some(job)=job else { return "<p class=\"muted\">尚未开始 AI 批改。</p>".into(); };
    let state=match job.status.as_str() { "queued"=>"排队中", "running"=>"批改中", "completed"=>"已完成", "cancelled"=>"已取消", "interrupted"=>"已中断", _=>"失败" };
    let mut review=s.clone();review.review="1".into();review.notice.clear();
    let mut html=format!("<div data-job-id=\"{}\"><p><strong>{} · {state}</strong>　已处理 {}/{}，草稿 {}，跳过 {}，失败 {}</p><p class=\"muted\">{}</p><a class=\"text-link\" href=\"{}\">刷新并核对评分草稿</a>",job.id,e(job.mode.label()),job.processed,job.total,job.drafted,job.skipped,job.failures.len(),e(&job.message),e(&review.url()));
    if !job.reviews.is_empty() {
        let _ = write!(html, "<details open><summary>待人工复核 {} 份：AI 参考结果</summary><ul>", job.reviews.len());
        for result in &job.reviews {
            let mut target = s.clone();
            target.attempt = result.attempt.clone();
            target.review.clear();
            target.filter.clear();
            target.search.clear();
            target.notice.clear();
            let score = result.score.map(|v| v.to_string()).unwrap_or_else(|| "未提供有效分数".into());
            let _ = write!(html, "<li><a class=\"text-link\" href=\"{}\">{}</a><p>需要人工复核：{}</p><p>AI 参考分数：{}</p><p>AI 参考评语：{}</p></li>", e(&target.url()), e(&result.label), e(&result.reason), e(&score), e(&result.feedback));
        }
        html.push_str("</ul><p class=\"muted\">参考结果尚未保存为评分草稿，请查看原始提交，核对后保存。</p></details>");
    }
    if !job.failures.is_empty() {
        html.push_str("<details><summary>查看需人工处理的提交</summary><ul>");
        for failure in &job.failures { let _=write!(html,"<li>{}：{}</li>",e(&failure.label),e(&failure.message)); }
        html.push_str("</ul></details>");
    }
    html.push_str("</div>");html
}
fn ai_panel(html: &mut String, s: &Selection, w: &Workspace, csrf: &str) {
    let job=w.ai_job.as_ref(); let active=job.is_some_and(|j|j.active());
    let question=job.map(|j|j.question.as_str()).unwrap_or("");
    let rubric=job.map(|j|j.rubric.as_str()).unwrap_or("");
    let symbolic=job.is_some_and(|j|j.mode==crate::services::ta_ai::Mode::Symbolic);
    if let Some(maximum) = w.maximum {
        let _=write!(html,"<p class=\"muted\">本作业满分：{:.2}（严小希设置）；象征性模式每处扣 {:.2}，最低 {:.2}。</p>",maximum,maximum*0.01,maximum*0.9);
    }
    let _=write!(html,r#"<details class="quick card" id="ta-ai-panel" {}><summary>AI 自动批改 <span>DeepSeek · 支持带图 PDF · 先生成草稿</span></summary>
<p class="muted">将本作业待评分提交的正文和附件发送至已配置的 DeepSeek API，产生 API 用量。范围为整份作业的待评分队列，已有草稿会跳过。请核对后手动同步成绩。</p>
<form action="/ta/action" method="post">{}
<label>题目（必填）<textarea name="question" rows="4" required maxlength="30000" placeholder="请键入本次作业的完整题目">{}</textarea></label>
<label style="margin-top:12px">评分标准（可选）<textarea name="rubric" rows="2" maxlength="10000" placeholder="可补充参考答案、各题分值与扣分要求">{}</textarea></label>
<label style="margin-top:12px">评分模式<select name="ai_mode"><option value="strict"{}>严格评分</option><option value="symbolic"{}>象征性评分</option></select></label>
<p class="muted">严格：按正确性和评分标准打分。象征性：有实质作答即给分，每处错误扣满分的 1%，最低保留 90%（10 分制：错一处扣 0.1，最低 9 分）。空白、无关或无法读取的提交需人工处理。两种模式的评语均为简洁、具体的 2—4 句。</p>
<div class="form-bottom" style="margin-top:14px"><button name="action" value="ai_start" data-ai-start{}>开始批改整份作业</button></div></form>
<div id="ta-ai-status" role="status" aria-live="polite" data-course="{}" data-column="{}" data-active="{}" style="margin-top:16px">{}</div>
<form action="/ta/action" method="post" data-ai-cancel {}>{}<input type="hidden" name="job_id" value="{}"><button class="ghost small" name="action" value="ai_cancel">取消后台批改</button></form>
</details>"#,if active{"open"}else{""},context(s,csrf),e(question),e(rubric),if symbolic{""}else{" selected"},if symbolic{" selected"}else{""},if active{" disabled"}else{""},e(&s.course),e(&s.column),active,ai_status(job,s),if active{""}else{"hidden"},context(s,csrf),job.map(|j|j.id.to_string()).unwrap_or_default());
}
fn filters(html: &mut String, s: &Selection) {
    let _ = write!(html, r#"<form class="filters student-filters" method="get" action="/ta/grading">{}{}<label>评分状态<select name="filter">"#,
        hidden("course", &s.course), hidden("column", &s.column));
    for (value, label) in [
        ("", "全部待评分提交"),
        ("draft", "有待同步草稿"),
    ] {
        let _ = write!(
            html,
            "<option value=\"{value}\"{}>{label}</option>",
            selected(&s.filter, value)
        );
    }
    let _ = write!(html, r#"</select></label><label class="search">查找学生<input name="search" value="{}" placeholder="学生姓名" maxlength="100"></label><button>应用筛选</button></form>"#, e(&s.search));
}

fn grading_panel(html: &mut String, s: &Selection, w: &Workspace, csrf: &str) {
    let visible = visible_attempts(s, w);
    if let Some(possible) = w.maximum {
        let _ = write!(html,"<details class=\"quick card\"><summary><strong>快速批改</strong><span>选中提交，统一填写分数与评语</span></summary><form id=\"bulk-form\" method=\"post\" action=\"/ta/action\">{}<div class=\"quick-fields\"><label>统一分数 / {possible}<input type=\"number\" name=\"score\" min=\"0\" max=\"{possible}\" step=\"0.01\" required placeholder=\"请输入分数\"></label><label>统一评语<textarea name=\"feedback\" rows=\"2\" maxlength=\"10000\" placeholder=\"可选，应用到选中的提交\"></textarea></label></div><label class=\"check\"><input type=\"checkbox\" name=\"select_visible\" value=\"yes\">选择当前筛选下的全部可评分提交</label><div class=\"form-bottom\"><button class=\"ghost\" name=\"action\" value=\"full_bulk\" formnovalidate>选中项满分草稿</button><button name=\"action\" value=\"save_bulk\">保存选中项为草稿</button></div><p class=\"muted\">在下方列表勾选提交，每次最多 200 份。</p></form></details>",context(s,csrf));
        for a in &visible {
            let _ = write!(html, r#"<input type="hidden" name="visible" value="{}:{}" form="bulk-form">"#,
                e(&a.id), e(&a.revision));
        }
    }
    html.push_str(r#"<div class="grading-layout"><section class="submission-list card"><div class="list-heading"><h3>学生提交</h3></div>"#);
    filters(html, s);
    html.push_str(r#"<div class="student-scroll">"#);
    for a in &visible {
        let mut target = s.clone();
        target.attempt = a.id.clone();
        target.notice.clear();
        let _ = write!(html, r#"<div class="student-row{}">"#, if a.id == s.attempt { " active" } else { "" });
        if w.maximum.is_some() {
            let _ = write!(html, r#"<input type="checkbox" name="selected" value="{}:{}" form="bulk-form" aria-label="选择 {} 的这次提交">"#,
                e(&a.id), e(&a.revision), e(&a.student_name));
        }
        let _ = write!(html, r#"<a href="{}"{}><div class="row-title"><strong>{}</strong><span class="badge">{}</span></div><small>{}</small></a></div>"#,
            e(&target.url()), if a.id == s.attempt { r#" aria-current="true""# } else { "" },
            e(&a.student_name), if draft_for(w, a).is_some() { "有草稿" } else { "待评分" }, e(&date(&a.date)));
    }
    if visible.is_empty() {
        html.push_str("<div class=\"empty compact\">没有符合筛选条件的提交</div>");
    }
    html.push_str("</div></section><section class=\"reviewer card\" aria-label=\"当前提交批改\">");
    let current = visible.iter().position(|a| a.id == s.attempt);
    if let Some(index) = current {
        let a = visible[index];
        let draft = draft_for(w, a);
        let ai_review = w.ai_job.as_ref().and_then(|job| job.reviews.iter().find(|r| r.attempt == a.id && r.revision == a.revision));
        if let Some(result) = ai_review {
            let _ = write!(html, "<div class=\"alert\">需要人工复核：{}。以下为 AI 参考结果，请核对后保存。</div>", e(&result.reason));
        }
        let _ = write!(html, r#"<div class="reviewer-heading"><div><h3>{}</h3><p class="muted">提交于 {} · 第 {} / {} 份</p></div></div><nav class="attempt-nav" aria-label="切换学生">"#,
            e(&a.student_name), e(&date(&a.date)), index + 1, visible.len());
        for (label, position) in [
            ("← 上一份", index.checked_sub(1)),
            (
                "下一份 →",
                if index + 1 < visible.len() {
                    Some(index + 1)
                } else {
                    None
                },
            ),
        ] {
            if let Some(position) = position {
                let mut target = s.clone();
                target.attempt = visible[position].id.clone();
                target.notice.clear();
                let _ = write!(
                    html,
                    "<a class=\"button ghost small\" href=\"{}\">{label}</a>",
                    e(&target.url())
                );
            }
        }
        html.push_str("</nav>");
        if let Some(submission) = &w.submission {
            if !submission.text.is_empty() {
                let _ = write!(
                    html,
                    "<h4>提交正文</h4><pre class=\"submission-text\">{}</pre>",
                    e(&submission.text)
                );
            }
            html.push_str("<div class=\"attachment-list\"><h4>作业附件</h4>");
            for file in &submission.files {
                let mut url = reqwest::Url::parse("http://localhost/ta/attachment").unwrap();
                url.query_pairs_mut().extend_pairs([
                    ("course", s.course.as_str()),
                    ("column", s.column.as_str()),
                    ("attempt", a.id.as_str()),
                    ("file", file.id.as_str()),
                ]);
                let path = format!("/ta/attachment?{}", url.query().unwrap_or(""));
                let _=write!(html,"<div class=\"attachment\"><span>▧ {}</span><a class=\"text-link\" href=\"{}\">下载</a></div>",e(&file.name),e(&path));
                if file.name.to_ascii_lowercase().ends_with(".pdf") {
                    let _=write!(html,"<details class=\"pdf-details\"><summary>在线预览 PDF</summary><iframe title=\"{}\" data-src=\"{}&amp;preview=true\" loading=\"lazy\" referrerpolicy=\"same-origin\"></iframe><a class=\"text-link\" href=\"{}\">无法预览时下载附件</a></details>",e(&file.name),e(&path),e(&path));
                }
            }
            if submission.files.is_empty() {
                html.push_str("<p class=\"muted\">未识别到可下载附件。如页面内容不完整，请在教学网核对原始提交。</p>");
            }
            html.push_str("</div>");
        } else {
            html.push_str(&refresh(s, csrf, "load_submission", "加载提交内容"));
        }
        let _=write!(html,"<a class=\"text-link source-link\" href=\"https://course.pku.edu.cn/webapps/assignment/gradeAssignmentRedirector?course_id={}&amp;attempt_id={}\" target=\"_blank\" rel=\"noopener noreferrer\">在教学网查看原始提交 ↗</a>",e(&s.course),e(&a.id));
        html.push_str(r#"</section><section class="reviewer grading-editor card" aria-label="批改与评分"><h3>批改与评分</h3>"#);
        let _ = write!(html, r#"<p class="muted">{} · {}</p>"#, e(&a.student_name), e(&date(&a.date)));
        grading_settings(html, s, w, csrf);
        if let Some(error) = draft.and_then(|d| d.error.as_ref()) {
            let _ = write!(
                html,
                "<div class=\"alert error\">{} 请先在教学网核对，再到确认页丢弃旧草稿。</div>",
                e(error)
            );
        }
        if let Some(possible) = w.maximum {
            let score = draft
                .map(|d| d.score)
                .or_else(|| ai_review.and_then(|r| r.score))
                .map(|v| v.to_string())
                .unwrap_or_default();
            let feedback = draft.map(|d| d.feedback.as_str()).or_else(|| ai_review.map(|r| r.feedback.as_str())).unwrap_or("");
            let next = visible.get(index + 1).map(|a| a.id.as_str()).unwrap_or("");
            let _=write!(html,"<form class=\"score-form\" method=\"post\" action=\"/ta/action\">{}{}{}{}<div class=\"score-heading\"><h4>评分与反馈</h4><span class=\"muted\">{}</span></div><label>得分 <span class=\"muted\">/ {possible}</span><input type=\"number\" name=\"score\" value=\"{}\" min=\"0\" max=\"{possible}\" step=\"0.01\" required></label><label>给学生的评语<textarea name=\"feedback\" rows=\"5\" maxlength=\"10000\" placeholder=\"写下扣分理由、改进建议或鼓励…\">{}</textarea></label>",context(s,csrf),hidden("attempt",&a.id),hidden("revision",&a.revision),hidden("next",next),if draft.is_some(){"已载入本地草稿"}else{"保存后可在确认页同步"},e(&score),e(feedback));
            let blocked = draft.is_some_and(|d| d.error.is_some());
            let _=write!(html,"<div class=\"form-bottom\"><button class=\"ghost\" name=\"action\" value=\"save_single\"{}>保存草稿</button><button name=\"action\" value=\"save_single_next\"{}>{}</button></div><p class=\"muted\">草稿仅自己可见。同步时会把此分数和评语写入教学网当前这一次提交。</p></form>",if blocked{" disabled"}else{""},if blocked{" disabled"}else{""},if next.is_empty(){"保存本份草稿"}else{"保存并批改下一份 →"});
        }
    } else {
        html.push_str(r#"<p class="muted">暂时没有可查看的提交。</p>"#);
    }
    html.push_str("</section></div>");
}

fn grading_settings(html: &mut String, s: &Selection, w: &Workspace, csrf: &str) {
    let maximum = w.maximum.map(|value| value.to_string()).unwrap_or_default();
    let _ = write!(html, r#"<form class="maximum-form" method="post" action="/ta/action">{}{}<label>本作业满分<input type="number" name="maximum" min="0.01" step="0.01" value="{}" required></label><button class="ghost" name="action" value="save_maximum">保存满分</button></form>"#,
        context(s, csrf), hidden("attempt", &s.attempt), e(&maximum));
    if w.maximum.is_some() {
        ai_panel(html, s, w, csrf);
    }
}

fn review_panel(html: &mut String, s: &Selection, w: &Workspace, csrf: &str) {
    let drafts: Vec<_> = w
        .drafts
        .iter()
        .filter(|d| !d.synced)
        .collect();
    if drafts.is_empty() {
        html.push_str("<div class=\"empty card\"><h3>所有草稿均已处理</h3><p>先在批改页保存评分，这里会显示待同步的分数和评语。</p></div>");
        return;
    }
    let _=write!(html,"<section class=\"card sync-panel\"><h3>核对评分后，再同步教学网</h3><p class=\"muted\">仅同步勾选的草稿。分数和评语会写入对应的具体提交，不会自动覆盖其他历史提交。</p><form method=\"post\" action=\"/ta/action\">{}<div class=\"table-scroll\"><table><thead><tr><th>选择</th><th>学生 / 提交</th><th>分数</th><th>给学生的评语</th><th>状态</th></tr></thead><tbody>",context(s,csrf));
    for d in drafts {
        let attempt = w.attempts.iter().find(|a| a.id == d.attempt_id);
        let current = attempt.filter(|a| a.revision == d.revision);
        let stale = current.is_none();
        let name = attempt.map(|a| a.student_name.as_str()).unwrap_or(&d.student_name);
        let name = if name.is_empty() { d.attempt_id.as_str() } else { name };
        let detail = current.map(|a| a.date.as_str()).unwrap_or("提交已不在队列中或版本已变化");
        let label = if attempt.is_some() {
            let mut target = s.clone();
            target.attempt = d.attempt_id.clone();
            target.filter.clear();
            target.search.clear();
            target.review.clear();
            target.notice.clear();
            format!(r#"<a class="text-link" href="{}">{}</a>"#, e(&target.url()), e(name))
        } else {
            e(name)
        };
        let status = if d.error.is_some() { "需核对" } else if stale { "状态已变化" } else { "待同步" };
        let error = d.error.as_ref()
            .map(|error| format!(r#"<small class="error-text">{}</small>"#, e(error))).unwrap_or_default();
        let _ = write!(html, r#"<tr><td><input type="checkbox" name="draft" value="{}" aria-label="选择 {} 的草稿"></td><td>{label}<small>{}</small></td><td class="score-number">{}</td><td class="feedback-cell">{}</td><td><span class="badge">{status}</span>{error}</td></tr>"#,
            e(&d.attempt_id), e(name), e(&date(detail)), d.score, e(&d.feedback));
    }
    html.push_str("</tbody></table></div><label class=\"check confirm-check\"><input type=\"checkbox\" name=\"confirmed\" value=\"yes\">我已核对选中的分数、评语和提交记录，确认写入教学网</label><div class=\"form-bottom\"><button class=\"ghost\" name=\"action\" value=\"discard\" formnovalidate>丢弃选中草稿</button><button name=\"action\" value=\"commit\">确认同步选中评分</button></div><p class=\"muted\">同步后会回读核验。遇到提交变更或未确认结果时停止，已保存的草稿会保留。曾登分但结果未确认的记录，请先在教学网核对。</p></form></section>");
}

const ASYNC_CSS: &str = r#"
.assignment-workspace{padding:16px var(--replay-gutter) 40px}.assignment-header,.assignment-counts,.assignment-row,.maximum-form{display:flex;align-items:center;gap:12px;flex-wrap:wrap}.assignment-header{padding-bottom:12px;border-bottom:1px solid var(--line)}.assignment-header h1,.assignment-header p{margin:0}.assignment-header h1{font-size:clamp(20px,2vw,27px)}.assignment-counts{margin-left:auto}.assignment-list{display:grid;gap:10px}.assignment-row{justify-content:space-between;padding:14px 16px}.assignment-row h3{margin:0 0 4px}.grading-layout{grid-template-columns:minmax(200px,23%) minmax(0,1fr) minmax(260px,29%)}.student-filters{padding:12px;margin:0}.student-filters select,.student-filters label{min-width:0;width:100%}.grading-editor .quick{margin:12px 0}.grading-editor input[type=number]{width:100%}.maximum-form{align-items:end}.maximum-form label{flex:1;min-width:90px}.course-panel .course-list{margin-top:12px}.workspace-tabs a[aria-current=page]{border-bottom:2px solid var(--accent)}
@media(max-width:1100px){.grading-layout{grid-template-columns:minmax(190px,26%) minmax(0,1fr)}.grading-editor{grid-column:2}}@media(max-width:720px){.grading-layout{grid-template-columns:minmax(0,1fr)}.grading-editor{grid-column:auto}.assignment-counts{margin-left:0}.assignment-header>div{width:100%}}

.ta-load-status{min-height:24px;margin:12px 0;color:#64748b;font-size:14px}.ta-load-status.error{color:#b91c1c}.ta-load-status button{margin-left:12px}.ta-loading{padding:28px}.ta-skeleton{height:22px;border-radius:8px;margin:18px 0;background:#e9eef5;animation:ta-pulse 1.5s ease-in-out infinite}.ta-skeleton:nth-child(even){width:72%}#ta-workspace-region[aria-busy=true]{opacity:.65;transition:opacity .15s}@keyframes ta-pulse{50%{opacity:.4}}@media(prefers-reduced-motion:reduce){.ta-skeleton{animation:none}}
"#;

// Small browser bridge only; workspace rendering, authentication and grading stay in Rust.
const SCRIPT: &str = r#"<script>
(() => {
  const region = document.getElementById('ta-workspace-region');
  const status = document.getElementById('ta-load-status');
  if (!region || !status) return;
  let generation = 0, controller = null, writing = false, reading = false;
  let aiTimer = null;
  function aiUpdate(payload) {
    const box = region.querySelector('#ta-ai-status'); if (!box) return;
    box.innerHTML = payload.html; box.dataset.active = String(payload.active);
    const button = region.querySelector('[data-ai-start]'); if (button) button.disabled = payload.active;
    const cancel = region.querySelector('[data-ai-cancel]');
    if (cancel) { cancel.hidden = !payload.active; cancel.elements.job_id.value = box.querySelector('[data-job-id]')?.dataset.jobId || ''; }
  }
  function aiPoll() {
    clearTimeout(aiTimer);
    const box = region.querySelector('#ta-ai-status');
    if (!box || box.dataset.active !== 'true') return;
    aiTimer = setTimeout(async () => {
      if (reading || writing) { aiPoll(); return; }
      const current = generation;
      try {
        const query = new URLSearchParams({course:box.dataset.course,column:box.dataset.column});
        const response = await fetch('/api/ta/ai/status?' + query, {credentials:'same-origin',cache:'no-store'});
        if (!response.ok) throw new Error('AI 进度暂时无法获取，请刷新查看。');
        const payload = await response.json();
        if (current === generation && region.contains(box) && !reading && !writing) aiUpdate(payload);
      } catch (error) { if (current === generation) message(error.message, true); }
      aiPoll();
    }, 2500);
  }
  function message(text, error = false) {
    status.classList.toggle('error', error);
    status.textContent = text;
  }
  const workspacePaths = ['/ta', '/ta/grading'];
  async function load(target, mode = 'push', form = null) {
    if (writing) { message('操作正在处理，请等待结果，勿重复提交。'); return; }
    const url = new URL(target, location.href);
    if (url.origin !== location.origin || !workspacePaths.includes(url.pathname)) return;
    if (controller) controller.abort();
    controller = new AbortController();
    const current = ++generation;
    writing = !!form; reading = !form;
    region.inert = writing;
    region.setAttribute('aria-busy', 'true');
    message(form?.get('action') === 'enter_assignment' ? '正在加载这项作业的待评分提交……' :
      form ? '正在处理，请稍候；请勿重复提交……' : '正在读取本地课程和作业……');
    try {
      const options = {credentials:'same-origin', cache:'no-store', signal:controller.signal,
        headers:{'Accept':'application/json', 'X-TA-Partial':'1'}};
      if (form) { options.method = 'POST'; options.body = form; }
      const response = await fetch(form ? '/ta/action' : '/api/ta/workspace' + url.search, options);
      if (response.status === 401) {
        location.assign('/auth?next=' + encodeURIComponent(url.pathname + url.search)); return;
      }
      const payload = await response.json();
      if (current !== generation) return;
      if (payload.ai === true) { aiUpdate(payload); message(payload.message || ''); aiPoll(); return; }
      if (typeof payload.html !== 'string' || typeof payload.url !== 'string') {
        if (form) { message(payload.error || '操作未完成，请检查输入。', true); return; }
        throw new Error(payload.error || '加载失败（HTTP ' + response.status + '）');
      }
      const destination = new URL(payload.url, location.origin);
      if (destination.origin !== location.origin || !workspacePaths.includes(destination.pathname)) throw new Error('页面地址无效');
      region.innerHTML = payload.html;
      void window.TeachingSession.handleError(region.textContent);
      region.dataset.loading = 'false';
      aiPoll();
      if (mode === 'push') history.pushState(null, '', destination.pathname + destination.search);
      else history.replaceState(null, '', destination.pathname + destination.search);
      message(response.ok ? '' : '操作未完成，请查看下方提示并核对输入。', !response.ok);
    } catch (error) {
      if (error.name === 'AbortError' || current !== generation) return;
      // Keep the existing form and feedback on network failures. Never retry a POST.
      message(form ? '未能确认操作结果，请先刷新并核对草稿或教学网成绩，勿重复提交。' : (error.message || '加载失败，请稍后重试。'), true);
      const retry = document.createElement('button'); retry.type = 'button';
      retry.textContent = form ? '刷新核对结果' : '重新加载';
      retry.addEventListener('click', () => load(url.href, 'replace'));
      status.append(retry);
    } finally {
      if (current === generation) {
        writing = false; reading = false; region.inert = false; region.setAttribute('aria-busy', 'false');
      }
    }
  }
  region.addEventListener('toggle', event => {
    const details = event.target;
    if (!details.matches('.pdf-details[open]')) return;
    const frame = details.querySelector('iframe[data-src]');
    if (frame) {
      frame.src = frame.dataset.src;
      delete frame.dataset.src;
    }
  }, true);
  region.addEventListener('click', event => {
    const link = event.target.closest('a[href]');
    if (!link || event.defaultPrevented || event.button !== 0 || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey || link.target || link.hasAttribute('download')) return;
    const url = new URL(link.href, location.href);
    if (url.origin === location.origin && workspacePaths.includes(url.pathname)) {
      event.preventDefault(); load(url.href);
    }
  });
  region.addEventListener('submit', event => {
    // Buttons named "action" shadow HTMLFormElement.action; read the attribute.
    const form = event.target, url = new URL(form.getAttribute('action') || location.href, location.href);
    if (url.origin !== location.origin || !workspacePaths.includes(url.pathname) && url.pathname !== '/ta/action') return;
    event.preventDefault();
    if (writing || (reading && form.method.toLowerCase() === 'post')) {
      message('数据正在更新，请等待完成后再操作。'); return;
    }
    const data = new FormData(form);
    if (event.submitter && event.submitter.name) data.append(event.submitter.name, event.submitter.value);
    if (form.method.toLowerCase() === 'post') load(location.href, 'push', new URLSearchParams(data));
    else { url.search = new URLSearchParams(data).toString(); load(url.href); }
  });
  window.addEventListener('popstate', () => load(location.href, 'replace'));
  window.addEventListener('beforeunload', event => { if (writing) { event.preventDefault(); event.returnValue = ''; } });
  if (region.dataset.loading === 'true') load(location.href, 'replace');
  else aiPoll();
})();
</script>"#;

const CSS: &str = r#"
button,.button{display:inline-flex;align-items:center;justify-content:center;gap:6px;border:1px solid transparent;border-radius:10px;padding:10px 14px;background:var(--accent);color:white;font:inherit;text-decoration:none;cursor:pointer;font-weight:600}button:hover,.button:hover{filter:brightness(.94)}button:disabled{opacity:.45;cursor:not-allowed}.ghost{background:#fffafa;color:var(--ink);border-color:var(--line)}.small{padding:6px 10px;font-size:13px}a{color:inherit}.text-link{color:var(--accent);text-decoration:none;font-weight:600}.text-link:hover{text-decoration:underline}:focus-visible{outline:2px solid var(--accent);outline-offset:3px}h2,h3,h4,p{margin-top:0}h2{font-size:21px;margin-bottom:0}h3{font-size:17px;margin-bottom:10px}h4{font-size:15px;margin-bottom:10px}.muted,small{color:var(--muted);font-size:13px}.ta-workspace{display:grid;grid-template-columns:minmax(220px,23%) minmax(0,1fr);gap:clamp(12px,1.6vw,28px);padding:16px var(--replay-gutter) 40px}.course-panel{background:linear-gradient(150deg,#fcf5f5,#fcf0f0);border-radius:12px;padding:20px 18px;min-width:0;align-self:start}.panel-heading,.section-header,.reviewer-heading,.form-bottom,.list-heading,.score-heading{display:flex;justify-content:space-between;align-items:center;gap:12px}.panel-heading h2{font-size:18px}.course-panel>.muted{margin:10px 0 18px}.course-list{display:grid;gap:6px}.course-item{display:flex;align-items:flex-start;gap:10px;padding:13px 10px;border-radius:10px;text-decoration:none;overflow-wrap:anywhere}.course-item.active,.course-item:hover{background:#fce5e5;color:var(--accent)}.course-item strong{display:block;font-size:14px}.course-item small{display:block;margin-top:5px}.course-symbol{font-size:22px;line-height:1.2;color:var(--accent)}.sidebar-note{border-top:1px solid var(--line);margin-top:28px;padding-top:18px;font-size:13px}.sidebar-note p{color:var(--muted);margin:9px 0 14px}.ta-content{min-width:0}.section-header{padding:12px 0 18px;border-bottom:1px solid var(--line);margin-bottom:16px}.section-header h2{margin-top:5px}.card{background:#fffdfd;border:1px solid var(--line);border-radius:12px}.filters{display:flex;flex-wrap:wrap;gap:12px;align-items:end;padding:16px;margin-bottom:16px}label{display:grid;gap:6px;font-size:13px;font-weight:600}input,select,textarea{font:inherit;color:var(--ink);border:1px solid #ddc8ca;background:white;border-radius:8px;padding:9px 10px;max-width:100%}select{min-width:130px}input[type=number]{width:145px}textarea{width:100%;resize:vertical}input[type=checkbox]{width:17px;height:17px;accent-color:var(--accent);flex:none;margin:0}.check{display:flex;gap:8px;align-items:center;font-weight:400;line-height:1.5}.filters .check{padding:10px 0}.filters .search{flex:1;min-width:140px}.filters input:not([type=checkbox]){width:100%;min-width:0}.stats{display:grid;grid-template-columns:repeat(2,1fr);border:1px solid var(--line);border-radius:12px;background:linear-gradient(120deg,#fffafa,#fdf1f1);padding:18px 0;margin:16px 0}.stats>div{display:grid;gap:2px;padding:0 20px;border-right:1px solid var(--line)}.stats>div:last-child{border:0}.stats strong{font-size:26px;color:var(--accent);font-weight:600}.stats span{font-size:13px;color:var(--muted)}.workspace-tabs{display:flex;gap:24px;border-bottom:1px solid var(--line);margin:22px 0 18px}.workspace-tabs a{padding:10px 0;text-decoration:none;color:var(--muted);font-weight:600}.workspace-tabs a[aria-current=page]{color:var(--accent);border-bottom:2px solid var(--accent)}.badge{display:inline-block;padding:3px 8px;border-radius:6px;background:#f8e8e8;color:#963b42;font-size:12px;white-space:nowrap;font-weight:500}.badge.done{background:#edf4ee;color:#386b43}.quick{padding:16px 18px;margin-bottom:18px}.quick summary{cursor:pointer;color:var(--accent)}.quick summary span{font-size:13px;color:var(--muted);margin-left:12px}.quick form{margin-top:18px}.quick-fields{display:grid;grid-template-columns:160px minmax(0,1fr);gap:16px;margin-bottom:14px}.quick p,.score-form>p{margin:12px 0 0}.grading-layout{display:grid;grid-template-columns:minmax(215px,30%) minmax(0,1fr);gap:16px;align-items:start}.submission-list{overflow:hidden}.list-heading{padding:16px 14px;border-bottom:1px solid var(--line);flex-wrap:wrap;gap:4px}.list-heading h3{margin:0}.list-heading span{font-size:12px}.student-scroll{max-height:780px;overflow-y:auto}.student-row{display:flex;align-items:center;gap:10px;padding:14px 12px;border-bottom:1px solid #f1e5e5}.student-row:last-child{border-bottom:0}.student-row.active{background:#fbeaea;box-shadow:inset 3px 0 var(--accent)}.student-row>a{flex:1;text-decoration:none;min-width:0}.student-row small{display:block;font-size:12px;margin-top:5px;overflow-wrap:anywhere}.row-title{display:flex;gap:6px;justify-content:space-between;align-items:center;flex-wrap:wrap}.reviewer{padding:22px;min-width:0}.reviewer-heading{align-items:start}.reviewer-heading p{margin-bottom:0}.reviewer-heading h3 .muted{margin-left:6px;font-weight:400}.attempt-nav{display:flex;justify-content:space-between;gap:8px;margin:16px 0 22px}.attachment{display:flex;justify-content:space-between;gap:14px;padding:12px;background:#fcf4f4;border-radius:8px;margin-bottom:8px;overflow-wrap:anywhere}.attachment-list{margin-bottom:12px}.submission-text{white-space:pre-wrap;overflow-wrap:anywhere;background:#fcf6f6;padding:14px;border-radius:8px;font:inherit;max-height:400px;overflow:auto}.source-link{display:inline-block;margin-bottom:20px;font-size:13px}.pdf-details{margin:8px 0 18px}.pdf-details summary{cursor:pointer;color:var(--accent);font-size:13px;padding:5px 0}.pdf-details iframe{width:100%;height:620px;border:1px solid var(--line);border-radius:8px;margin:10px 0}.score-form{border-top:1px solid var(--line);padding-top:20px;display:grid;gap:16px}.score-heading h4{margin:0}.form-bottom{justify-content:flex-end;flex-wrap:wrap}.form-bottom>.check{margin-right:auto}.alert{padding:13px 16px;border-radius:10px;margin-bottom:16px;overflow-wrap:anywhere}.alert a{font-weight:600}.error{background:#fff0ed;border:1px solid #ebc0b8;color:#9b302b}.success{background:#eff6ef;border:1px solid #c7ddc8;color:#386b43}.empty{text-align:center;padding:45px 24px;color:var(--muted)}.empty h2,.empty h3{color:var(--ink);margin-bottom:10px}.empty p{max-width:500px;margin:0 auto 20px}.empty.compact{padding:24px 8px}.welcome{margin-top:12px;min-height:330px;display:flex;flex-direction:column;align-items:center;justify-content:center}.empty-mark{font-size:38px;color:var(--accent);margin-bottom:16px}.sync-panel{padding:22px}.table-scroll{overflow-x:auto}table{width:100%;border-collapse:collapse;text-align:left;margin:16px 0}th{font-size:12px;color:var(--muted);background:#fcf3f3}th,td{padding:13px 12px;border-bottom:1px solid var(--line);vertical-align:top}td small{display:block;margin-top:5px}.score-number{font-size:18px;font-weight:600;color:var(--accent)}.feedback-cell{white-space:pre-wrap;overflow-wrap:anywhere;min-width:160px;max-width:350px;font-size:13px}.error-text{max-width:220px;color:#9b302b;white-space:normal}.confirm-check{padding:16px 0}.sync-panel>form>p{margin:14px 0 0}@media(min-width:1600px){.grading-layout{grid-template-columns:300px minmax(0,1fr)}.reviewer{padding:28px}}@media(max-width:1100px){.ta-workspace{grid-template-columns:210px minmax(0,1fr)}.grading-layout{grid-template-columns:1fr}.student-scroll{max-height:300px}.quick-fields{grid-template-columns:1fr}.quick summary span{display:block;margin:5px 0 0}}@media(max-width:720px){.ta-workspace{grid-template-columns:1fr;padding-top:12px}.course-panel{padding:16px}.course-list{grid-template-columns:repeat(auto-fit,minmax(180px,1fr))}.sidebar-note{display:none}.course-panel>.muted{margin-bottom:8px}.filters label{flex:1;min-width:130px}.filters select{width:100%}.stats>div{padding:0 10px}.stats strong{font-size:22px}.stats span{font-size:12px}.reviewer{padding:16px}.reviewer-heading{flex-wrap:wrap}.section-header{align-items:flex-start}.workspace-tabs{gap:16px}.pdf-details iframe{height:450px}.hero-copy{max-width:85%}}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::ta::{Assignment, Attachment, Course, Submission};

    #[test]
    fn refresh_is_an_explicit_csrf_protected_post_and_keeps_selection() {
        let selection = Selection { course: "_c_1".into(), column: "_h_1".into(), attempt: "_a_1".into(), ..Default::default() };
        for (action, label) in [("refresh_courses", "刷新"), ("refresh", "刷新作业"), ("load_submission", "加载提交内容")] {
            let html = refresh(&selection, "csrf-value", action, label);
            assert!(html.contains("method=\"post\" action=\"/ta/action\""));
            for value in ["csrf-value", "_c_1", "_h_1", "_a_1"] { assert!(html.contains(value)); }
            assert!(html.contains(&format!("name=\"action\" value=\"{action}\"")));
        }
        assert!(!selection.url().contains("refresh"));
    }
    pub(crate) fn fixture() -> (Selection, Workspace) {
        let s = Selection {
            course: "_c_1".into(),
            column: "_h_1".into(),
            attempt: "_a_1".into(),
            ..Default::default()
        };
        let mut w = Workspace::default();
        w.courses.push(Course {
            id: s.course.clone(),
            name: "程序设计与算法".into(),
            term: "26-27学年第1学期".into(),
        });
        w.assignments.push(Assignment {
            id: s.column.clone(),
            name: "作业一 · 基础练习".into(),
            attempts: Vec::new(),
        });
        w.queue_cached = true;
        w.maximum = Some(100.0);
        w.attempts.push(Attempt {
            id: s.attempt.clone(),
            student_name: "同学甲".into(),
            date: "2026-11-13T12:30:00Z".into(),
            revision: "abc".into(),
        });
        w.submission = Some(Submission {
            text: "<script>alert('x')</script>这是一份作业".into(),
            files: vec![Attachment {
                id: "_f_1".into(),
                name: "第一次作业.pdf".into(),
                url: "/file".into(),
            }],
        });
        (s, w)
    }
    #[test]
    fn course_selection_keeps_the_right_side_empty_until_a_course_is_selected() {
        let (mut selection, mut workspace) = fixture();
        selection.column.clear();
        workspace.assignments[0].attempts = workspace.attempts.clone();
        let blank = fragment(&Selection::default(), &workspace, "csrf");
        assert!(blank.contains(r#"<section class="ta-content" aria-label="作业选择"></section>"#));
        assert!(!blank.contains("作业一 · 基础练习"));
        let course = fragment(&selection, &workspace, "csrf");
        assert!(course.contains("作业一 · 基础练习"));
        assert!(course.contains("进入批改"));
        assert!(!course.contains(r#"class="grading-layout""#));
    }

    #[test]
    fn renders_real_forms_and_escapes_untrusted_submission() {
        let (s, w) = fixture();
        let html = render(&s, &w, "csrf");
        assert!(html.contains("&lt;script&gt;") && !html.contains("<script>alert"));
        assert!(html.contains("aria-label=\"批改与评分\""));
        assert!(html.contains("href=\"/ta?course=_c_1\""));
        assert!(html.contains("/ta/grading?course=_c_1"));
        assert!(html.contains("form=\"bulk-form\"") && html.contains("name=\"revision\""));
        assert!(html.contains("/ta/action") && !html.contains("type=\"password\""));
        assert!(html.contains("value=\"strict\"") && html.contains("value=\"symbolic\""));
        assert!(html.contains("本作业满分：100.00") && html.contains("name=\"question\""));
        assert!(html.contains("data-ai-start") && html.contains("/api/ta/ai/status"));
        assert!(html.contains("name=\"maximum\""));
        assert!(html.contains("gradeAssignmentRedirector?course_id=_c_1&amp;attempt_id=_a_1"));
        assert!(!html.contains("courseMembershipId"));
    }
    #[test]
    fn review_displays_exact_attempt_and_requires_confirmation() {
        let (mut s, mut w) = fixture();
        s.review = "1".into();
        w.drafts.push(Draft {
            attempt_id: s.attempt.clone(),
            student_name: "同学甲".into(),
            score: 90.0,
            feedback: "</textarea><script>".into(),
            revision: "abc".into(),
            updated_at: String::new(),
            synced: false,
            error: None,
        });
        let html = render(&s, &w, "csrf");
        assert!(html.contains("name=\"confirmed\""));
        assert!(html.contains("value=\"_a_1\""));
        assert!(!html.contains("</textarea><script>"));
    }

    /// Local visual QA only. Never uses real accounts or sends grades.
    #[tokio::test]
    #[ignore]
    async fn ta_visual_preview() {
        use axum::{extract::Query, routing::get, Router};
        let app=Router::new().route("/ta",get(||async { super::super::super::ui::site_page(&shell(),"/ta") }))
            .route("/api/ta/workspace",get(|Query(mut s):Query<Selection>|async move {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            let (initial,mut w)=fixture();
            if s.course.is_empty(){s.course=initial.course;}
            if s.column.is_empty(){s.column=initial.column;}
            if s.attempt.is_empty(){s.attempt=initial.attempt;}
            w.courses.push(Course{id:"_c_2".into(),name:"数据结构与算法".into(),term:"26-27学年第1学期".into()});
            w.submission.as_mut().unwrap().text="第一题：使用双指针遍历数组，每个元素只访问一次，时间复杂度 O(n)，额外空间复杂度 O(1)。\n第二题：递归终止条件为区间长度小于 2，具体实现见附件。".into();
            for i in 2..=12 {
                let student_name = format!("同学{}", ["乙","丙","丁","戊","己","庚","辛","壬","癸","子","丑"][i-2]);
                w.attempts.push(Attempt {
                    id: format!("_a_{i}"), student_name, date: "2026-11-13T14:15:00Z".into(), revision: "abc".into(),
                });
            }
            for i in 1..=3 {w.drafts.push(Draft{attempt_id:format!("_a_{i}"),student_name:format!("同学 {i}"),score:90.0,feedback:"思路正确，请补充边界条件说明。".into(),revision:"abc".into(),updated_at:String::new(),synced:false,error:None});}
            axum::Json(serde_json::json!({"html":fragment(&s,&w,"preview-only"),"url":s.url()}))
        })).route("/ta/action",axum::routing::post(|axum::Form(form):axum::Form<Vec<(String,String)>>|async move {
            let (mut s,mut w)=fixture();
            s.notice="预览：草稿已保存".into();
            w.drafts.push(Draft{attempt_id:s.attempt.clone(),student_name:"同学甲".into(),score:super::super::field(&form,"score").parse().unwrap_or(100.0),feedback:super::super::field(&form,"feedback").into(),revision:"abc".into(),updated_at:String::new(),synced:false,error:None});
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            axum::Json(serde_json::json!({"html":fragment(&s,&w,"preview-only"),"url":s.url()}))
        })).route("/course-replay-hero-v3.webp",get(||async {
            ([(axum::http::header::CONTENT_TYPE,"image/webp")],include_bytes!("../../assest/course-replay-hero-v3.webp").as_slice())
        })).route("/api/auth/me",get(||async {axum::Json(serde_json::json!({"username":"助教预览"}))}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:18192")
            .await
            .unwrap();
        println!("TA visual preview: http://127.0.0.1:18192/ta (fixture data only)");
        tokio::time::timeout(std::time::Duration::from_secs(900), async move {
            axum::serve(listener, app).await.unwrap();
        })
        .await
        .ok();
    }
}
