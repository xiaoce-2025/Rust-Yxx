(() => {
  let pending = null;
  function choose() {
    if (pending) return pending;
    pending = new Promise(resolve => {
      const dialog = document.createElement('dialog');
      dialog.setAttribute('aria-labelledby', 'teaching-login-title');
      dialog.style.cssText = 'width:min(420px,calc(100% - 48px));padding:28px;border:1px solid #e7ded4;border-radius:18px;background:#fffdf8;color:#302b28;box-shadow:0 20px 80px #0003;font-family:inherit';
      dialog.innerHTML = '<h2 id="teaching-login-title" style="margin:0 0 12px;font-size:22px">登录教学网</h2><p style="line-height:1.8">当前没有可用的教学网登录会话。可以绑定账号以便自动登录，或使用一次性登录，本次不保存账号密码。</p><div style="display:flex;flex-wrap:wrap;gap:10px;margin-top:24px"><a data-bind>绑定账号</a><a data-once>一次性登录</a><button type="button" data-cancel>取消</button></div>';
      const next = location.pathname + location.search + location.hash;
      dialog.querySelector('[data-bind]').href = '/pkuvideofullclass?callback=' + encodeURIComponent(next);
      dialog.querySelector('[data-once]').href = '/teaching-login?next=' + encodeURIComponent(next);
      for (const button of dialog.querySelectorAll('a,button')) {
        button.style.cssText = 'display:inline-block;padding:10px 14px;border:1px solid #d8cbbf;border-radius:9px;color:#71372c;background:#fff;text-decoration:none;font:inherit;cursor:pointer';
      }
      dialog.querySelector('[data-bind]').style.background = '#f5e9db';
      dialog.querySelector('[data-cancel]').addEventListener('click', () => dialog.close());
      dialog.addEventListener('close', () => { dialog.remove(); pending = null; resolve(false); }, {once:true});
      document.body.append(dialog);
      dialog.showModal();
    });
    return pending;
  }
  function handleError(message) {
    if (/尚未绑定教学网|未绑定IAAA账号|教学网登录已失效|一次性登录已失效/.test(message || '')) return choose();
    return Promise.resolve(false);
  }
  async function ensure() {
    const response = await fetch('/api/teaching-session', {credentials:'same-origin',cache:'no-store'});
    if (response.status === 401) {
      location.href = '/auth?next=' + encodeURIComponent(location.pathname + location.search);
      return false;
    }
    if (!response.ok) throw new Error('无法检查教学网登录状态，请稍后重试');
    const status = await response.json();
    return status.is_bound || status.has_session ? true : choose();
  }
  window.TeachingSession = {ensure, choose, handleError};
})();
