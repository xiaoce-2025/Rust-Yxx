// ==UserScript==
// @name         蕉学网回放2025秋季更新回退插件
// @version      1.7
// @description  复制回放下载链接，自动获取解密密钥并附加 enc_key 参数，无需手动下载 enc.key
// @author       cxxdgc
// @match        https://onlineroomse.pku.edu.cn/player*
// @run-at       document-start
// @grant        GM_setClipboard
// @grant        GM_xmlhttpRequest
// @connect      onlineroomse.pku.edu.cn
// @connect      resourcese.pku.edu.cn
// @connect      course.pku.edu.cn
// ==/UserScript==

(function() {
    'use strict';

    let downloadUrl = '';
    let requestSequence = 0;
    let statusTimer;

    // 在 document-start 安装拦截器，避免漏掉 DOMContentLoaded 前的视频请求。
    const originalOpen = XMLHttpRequest.prototype.open;
    const originalSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function(method, url) {
        this._pkuReplayUrl = String(url);
        return originalOpen.apply(this, arguments);
    };
    XMLHttpRequest.prototype.send = function() {
        if ((this._pkuReplayUrl || '').includes('get-sub-info-by-auth-data')) {
            const sequence = ++requestSequence;
            downloadUrl = '';
            this.addEventListener('load', function() {
                if (sequence !== requestSequence) return;
                try {
                    const response = this.responseType === 'json' ? this.response : JSON.parse(this.responseText || this.response);
                    const item = response.list?.[0];
                    const content = typeof item?.sub_content === 'string' ? JSON.parse(item.sub_content) : item?.sub_content;
                    const url = content?.save_playback?.contents;
                    if (typeof url !== 'string' || !url) throw new Error('视频信息中没有下载地址');
                    downloadUrl = new URL(url, location.href).href;
                } catch (error) {
                    showStatus('获取回放地址失败：' + error.message, true);
                }
            });
        }
        return originalSend.apply(this, arguments);
    };

    function request(url, responseType = 'text') {
        return new Promise((resolve, reject) => {
            GM_xmlhttpRequest({
                method: 'GET', url, responseType, timeout: 30000,
                // 使用现有登录 Cookie 获取密钥，无需输入门户密码。
                anonymous: false,
                headers: {Origin: 'https://onlineroomse.pku.edu.cn', Referer: 'https://onlineroomse.pku.edu.cn/'},
                onload(response) {
                    if (response.status < 200 || response.status >= 300) {
                        reject(new Error('请求失败（HTTP ' + response.status + '），请确认回放可以正常播放'));
                    } else resolve(response);
                },
                onerror() { reject(new Error('请求失败，请确认已登录教学网，并允许插件访问回放域名')); },
                ontimeout() { reject(new Error('请求超时，请稍后重试')); },
                onabort() { reject(new Error('请求已取消')); }
            });
        });
    }

    async function linkWithKey(originalUrl) {
        const result = new URL(originalUrl);
        if (!result.pathname.includes('.m3u8')) return originalUrl;
        // 保留下载地址的其他原始参数，避免改变签名参数的编码。
        const withoutHash = originalUrl.split('#')[0];
        const q = withoutHash.indexOf('?');
        let cleanUrl = withoutHash;
        if (q >= 0) {
            const query = withoutHash.slice(q + 1).split('&')
                .filter(part => decodeURIComponent(part.split('=')[0]) !== 'enc_key').join('&');
            cleanUrl = withoutHash.slice(0, q) + (query ? '?' + query : '');
        }
        let playlistUrl = cleanUrl;
        for (let depth = 0; depth < 6; depth++) {
            const response = await request(playlistUrl);
            const text = String(response.responseText ?? response.response ?? '').trim();
            if (!text.startsWith('#EXTM3U')) throw new Error('返回内容不是 m3u8，请先在本页正常播放回放后重试');
            const base = response.finalUrl || playlistUrl;
            const lines = text.split(/\r?\n/).map(line => line.trim());
            const variant = lines.findIndex(line => line.startsWith('#EXT-X-STREAM-INF:'));
            if (variant >= 0) {
                const path = lines.slice(variant + 1).find(line => line && !line.startsWith('#'));
                if (!path) throw new Error('主播放列表缺少视频地址');
                playlistUrl = new URL(path, base).href;
                continue;
            }
            const keyUrls = new Set();
            for (const line of lines.filter(line => line.startsWith('#EXT-X-KEY:'))) {
                const method = line.match(/(?:^|[:,])\s*METHOD=([^,]+)/)?.[1];
                if (method === 'NONE') continue;
                if (method !== 'AES-128') throw new Error('暂不支持此回放的加密方式：' + (method || '未知'));
                const uri = line.match(/(?:^|[:,])\s*URI="([^"]+)"/)?.[1];
                if (!uri) throw new Error('播放列表没有提供密钥地址');
                keyUrls.add(new URL(uri, base).href);
            }
            if (!keyUrls.size) return cleanUrl;
            if (keyUrls.size > 1) throw new Error('此回放使用多个密钥，暂不能放入单个 enc_key 参数');
            const keyResponse = await request([...keyUrls][0], 'arraybuffer');
            const bytes = new Uint8Array(keyResponse.response);
            if (bytes.length !== 16) throw new Error('密钥不是 16 字节，请确认本页可以正常播放，再重试复制');
            const key = 'hex:' + Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join('');
            return cleanUrl + (cleanUrl.includes('?') ? '&' : '?') + 'enc_key=' + encodeURIComponent(key);
        }
        throw new Error('播放列表嵌套过深');
    }

    function showStatus(message = '√ 已复制下载链接（含密钥）', error = false) {
        const status = document.querySelector('.pku-copy-status');
        if (!status) return;
        clearTimeout(statusTimer);
        status.textContent = message;
        status.dataset.error = String(error);
        status.style.display = 'inline-block';
        statusTimer = setTimeout(() => { status.style.display = 'none'; }, error ? 10000 : 5000);
    }

    async function copyText(text) {
        if (typeof GM_setClipboard === 'function') {
            GM_setClipboard(text, 'text');
            return;
        }
        if (navigator.clipboard?.writeText) {
            try { await navigator.clipboard.writeText(text); return; } catch (_) { /* 继续回退 */ }
        }
        const area = document.createElement('textarea');
        area.value = text;
        area.style.position = 'fixed';
        document.body.appendChild(area);
        area.focus();
        area.select();
        try {
            if (!document.execCommand('copy')) throw new Error('浏览器拒绝复制，请授予插件剪贴板权限');
        } finally { area.remove(); }
    }

    async function handleCopyLink() {
        const button = document.querySelector('.pku-copy-btn');
        if (!downloadUrl) { showStatus('尚未获取到回放地址，请先播放视频后重试', true); return; }
        if (button.disabled) return;
        const originalUrl = downloadUrl;
        const sequence = requestSequence;
        button.disabled = true;
        showStatus('正在获取解密密钥…');
        try {
            const link = await linkWithKey(originalUrl);
            if (sequence !== requestSequence || originalUrl !== downloadUrl) throw new Error('已切换回放，请重新点击复制');
            await copyText(link);
            showStatus(new URL(link).searchParams.has('enc_key') ? '√ 已复制下载链接（含密钥）' : '√ 已复制下载链接');
        } catch (error) { showStatus('复制失败：' + error.message, true); }
        finally { button.disabled = false; }
    }

    function addDownloadButtons() {
        const footer = document.querySelector('.course-info__wrap .course-info__footer');
        if (!footer || document.querySelector('.pku-copy-btn')) return;
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'el-button el-button--default el-button--medium pku-copy-btn';
        button.textContent = '复制下载地址';
        button.style.marginLeft = '10px';
        button.onclick = handleCopyLink;
        const status = document.createElement('span');
        status.className = 'pku-copy-status';
        status.setAttribute('role', 'status');
        status.setAttribute('aria-live', 'polite');
        status.style.cssText = 'display:none;margin-left:10px;color:#67c23a;font-size:14px;overflow-wrap:anywhere';
        footer.append(button, status);
    }

    function init() {
        const style = document.createElement('style');
        style.textContent = '.pku-copy-status[data-error="true"]{color:#c0392b!important}';
        document.head.appendChild(style);
        addDownloadButtons();
        // 页面为动态渲染，切换章节后也需要恢复按钮。
        new MutationObserver(addDownloadButtons).observe(document.body, {childList: true, subtree: true});
    }
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init, {once: true});
    else init();
})();
