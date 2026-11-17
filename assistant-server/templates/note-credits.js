// Shared, advisory submission check. Worker admission remains authoritative.
window.NoteCredits = (() => {
  let dialog;
  function show(title, message) {
    if (!dialog) {
      dialog = document.createElement('dialog');
      dialog.setAttribute('aria-labelledby','credit-dialog-title');
      dialog.style.cssText='max-width:420px;width:calc(100% - 48px);border:1px solid #ead4d8;border-radius:20px;padding:28px;background:#fffaf5;color:#752e42;box-shadow:0 20px 80px #752e4233';
      const heading=document.createElement('h2'); heading.id='credit-dialog-title';
      const detail=document.createElement('p'); detail.style.cssText='line-height:1.8;color:#765e64';
      const button=document.createElement('button'); button.type='button';button.textContent='我知道了';
      button.style.cssText='background:#8b3a50;color:white;border:0;border-radius:12px;padding:10px 24px;cursor:pointer';
      button.addEventListener('click',()=>dialog.close());
      dialog.append(heading,detail,button);document.body.append(dialog);
    }
    dialog.querySelector('h2').textContent=title;dialog.querySelector('p').textContent=message;
    if (!dialog.open) dialog.showModal();
  }
  async function balance() {
    const response=await fetch('/api/note-credits/me',{cache:'no-store',headers:{'Cache-Control':'no-store'},credentials:'same-origin'});
    if (!response.ok) throw new Error('额度查询失败');
    const value=await response.json();
    if (typeof value.balance!=='number' || !Number.isFinite(value.balance)) throw new Error('额度响应无效');
    document.querySelectorAll('[data-note-credit-balance]').forEach(el=>el.textContent=`笔记处理额度：${value.balance} 小时`);
    return value;
  }
  async function check() {
    try {
      const value=await balance();
      if (value.balance<=0) {
        show('课堂笔记处理额度不足',`当前剩余额度：${value.balance} 小时。请联系管理员添加额度后再提交任务。`);
        return false;
      }
      return true;
    } catch {
      show('暂时无法获取额度','暂时无法获取额度，请稍后重试');return false;
    }
  }
  document.addEventListener('DOMContentLoaded',()=>balance().catch(()=>{}));
  return {check,balance};
})();
