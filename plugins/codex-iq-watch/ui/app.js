/* 降智监控管理页：仅通过 window.codexProxyPlugin 桥与宿主交互。 */
(function () {
  'use strict';

  var bridge = window.codexProxyPlugin;

  var accountsBody = document.getElementById('accounts-body');
  var statusSub = document.getElementById('status-sub');
  var detailCard = document.getElementById('detail-card');
  var detailTitle = document.getElementById('detail-title');
  var detailSub = document.getElementById('detail-sub');
  var eventsBody = document.getElementById('events-body');
  var alertsBody = document.getElementById('alerts-body');
  var configBody = document.getElementById('config-body');
  var toast = document.getElementById('toast');
  var testBtn = document.getElementById('test-btn');
  var refreshBtn = document.getElementById('refresh-btn');
  var detailClose = document.getElementById('detail-close');
  var saveBtn = document.getElementById('save-settings');
  var resetBtn = document.getElementById('reset-settings');
  var reloadSettingsBtn = document.getElementById('reload-settings');
  var settingsHint = document.getElementById('settings-hint');

  var STATUS_LABELS = {
    unknown: '未知',
    healthy: '正常',
    suspect: '疑似',
    degraded: '降智',
  };

  var OUTCOME_LABELS = {
    succeeded: '成功',
    failed: '失败',
    rejected: '拒绝',
    cancelled: '取消',
    incomplete: '不完整',
    unknown: '未知',
  };

  var SIGNAL_LABELS = {
    upstream_overload: '上游过载',
    cache_collapse: '缓存骤降',
    slow_response: '响应变慢',
  };

  function request(input) {
    if (!bridge || typeof bridge.request !== 'function') {
      return Promise.reject(new Error('插件管理桥不可用'));
    }
    return bridge.request(input).then(function (response) {
      var text = new TextDecoder().decode(response.body || new ArrayBuffer(0));
      var data = {};
      if (text) {
        try { data = JSON.parse(text); } catch (error) { data = { raw: text }; }
      }
      if (response.status >= 400) {
        throw new Error(data.error || ('请求失败 HTTP ' + response.status));
      }
      return data;
    });
  }

  function formatMs(ms) {
    if (!ms) return '—';
    var date = new Date(ms);
    if (Number.isNaN(date.getTime())) return '—';
    return date.toLocaleString();
  }

  function formatTokens(value) {
    if (value == null) return '—';
    if (value >= 1000) return (value / 1000).toFixed(1) + 'k';
    return String(value);
  }

  function esc(text) {
    return String(text == null ? '' : text)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/\"/g, '&quot;');
  }

  function field(id) { return document.getElementById(id); }

  function showToast(message, ok) {
    toast.textContent = message;
    toast.className = 'toast ' + (ok ? 'ok' : 'fail');
    toast.hidden = false;
    window.clearTimeout(showToast.timer);
    showToast.timer = window.setTimeout(function () { toast.hidden = true; }, 8000);
  }

  function renderAccounts(accounts) {
    if (!accounts.length) {
      accountsBody.innerHTML = '<tr><td colspan="8" class="empty">暂无账号观察数据</td></tr>';
      return;
    }
    accountsBody.innerHTML = accounts.map(function (account) {
      var status = STATUS_LABELS[account.status] || account.status;
      var cls = account.status === 'degraded' ? 'degraded'
        : account.status === 'suspect' ? 'suspect'
        : account.status === 'healthy' ? 'healthy' : '';
      return '<tr>'
        + '<td class="mono">' + esc(account.account_id) + '</td>'
        + '<td>' + esc(account.provider || '—') + '</td>'
        + '<td>' + esc(account.model || '—') + '</td>'
        + '<td><span class="tag ' + cls + '">' + esc(status) + '</span></td>'
        + '<td>' + (account.signaled_events || 0) + ' / ' + (account.window_events || 0) + '</td>'
        + '<td>' + esc(formatMs(account.last_observed_at_ms)) + '</td>'
        + '<td>' + esc(account.last_alert_at_ms ? formatMs(account.last_alert_at_ms) : '—') + '</td>'
        + '<td><button class="btn link" data-account="' + esc(account.account_id) + '" type="button">详情</button></td>'
        + '</tr>';
    }).join('');
    accountsBody.querySelectorAll('button[data-account]').forEach(function (button) {
      button.addEventListener('click', function () {
        loadDetail(button.getAttribute('data-account'));
      });
    });
  }

  function renderConfig(config) {
    var items = [
      ['启用通知', config.enabled ? '是' : '否'],
      ['监控 Provider', (config.watch_providers || []).join(', ') || '全部'],
      ['统计窗口', Math.round((config.window_ms || 0) / 60000) + ' 分钟'],
      ['告警条件', '≥' + config.min_signaled_requests + ' 个信号请求且 ≥' + config.min_signal_kinds + ' 种信号，连续 ' + config.consecutive_triggers + ' 次'],
      ['冷却时间', Math.round((config.cooldown_ms || 0) / 60000) + ' 分钟'],
      ['慢响应阈值', (config.latency_ms || 0) / 1000 + 's / 首 token ' + (config.first_token_ms || 0) / 1000 + 's'],
      ['缓存判定输入下限', formatTokens(config.cache_min_input_tokens) + ' tokens'],
      ['Webhook', config.webhook_url ? config.webhook_url : '未配置'],
      ['Webhook 格式', config.webhook_format || 'generic'],
      ['邮件', config.email_url ? config.email_url : '未配置'],
      ['邮件格式', config.email_format || 'generic'],
      ['收件人', (config.email_to || []).join(', ') || '未配置'],
    ];
    configBody.innerHTML = items.map(function (item) {
      return '<div><dt>' + esc(item[0]) + '</dt><dd>' + esc(item[1]) + '</dd></div>';
    }).join('');
  }

  function renderEvents(events) {
    var list = (events || []).slice().reverse();
    if (!list.length) {
      eventsBody.innerHTML = '<tr><td colspan="6" class="empty">暂无信号记录</td></tr>';
      return;
    }
    eventsBody.innerHTML = list.map(function (event) {
      var signals = (event.signals || []).map(function (signal) {
        return '<span class="tag signal">' + esc(SIGNAL_LABELS[signal] || signal) + '</span>';
      }).join('') || '<span class="muted">—</span>';
      var cache = (event.cached_tokens == null ? '—' : formatTokens(event.cached_tokens))
        + ' / ' + (event.input_tokens == null ? '—' : formatTokens(event.input_tokens));
      var latency = event.latency_ms != null ? (event.latency_ms / 1000).toFixed(1) + 's' : '—';
      return '<tr>'
        + '<td>' + esc(formatMs(event.observed_at_ms)) + '</td>'
        + '<td>' + esc(OUTCOME_LABELS[event.outcome] || event.outcome) + '</td>'
        + '<td>' + signals + '</td>'
        + '<td>' + esc(event.upstream_status || event.client_status || '—') + '</td>'
        + '<td>' + esc(cache) + '</td>'
        + '<td>' + esc(latency) + '</td>'
        + '</tr>';
    }).join('');
  }

  function renderAlerts(alerts) {
    var list = (alerts || []).slice().reverse();
    if (!list.length) {
      alertsBody.innerHTML = '<tr><td colspan="3" class="empty">暂无告警</td></tr>';
      return;
    }
    alertsBody.innerHTML = list.map(function (alert) {
      var verdict = alert.verdict || {};
      var kinds = (verdict.signal_kinds || []).map(function (kind) {
        return SIGNAL_LABELS[kind] || kind;
      }).join('、');
      var deliveries = (alert.deliveries || []).map(function (d) {
        return esc(d.channel) + (d.ok ? ' ✓' : ' ✗ ' + esc(d.detail));
      }).join('<br>') || '<span class="muted">未配置渠道</span>';
      return '<tr>'
        + '<td>' + esc(formatMs(alert.at_ms)) + '</td>'
        + '<td>' + esc((verdict.signaled_requests || 0) + ' 个信号：' + kinds) + '</td>'
        + '<td>' + deliveries + '</td>'
        + '</tr>';
    }).join('');
  }

  function loadStatus() {
    statusSub.textContent = '正在加载…';
    request({ method: 'GET', path: 'status' }).then(function (data) {
      renderAccounts(data.accounts || []);
      renderConfig(data.config || {});
      var total = (data.accounts || []).length;
      var degraded = (data.accounts || []).filter(function (a) { return a.status === 'degraded'; }).length;
      statusSub.textContent = total
        ? ('共 ' + total + ' 个账号，' + degraded + ' 个降智；观察窗口内持续判定')
        : '暂无账号观察数据；插件在后台持续统计请求终态';
    }).catch(function (error) {
      statusSub.textContent = '加载失败：' + error.message;
    });
  }

  function loadDetail(accountId) {
    if (!accountId) return;
    detailCard.hidden = false;
    detailTitle.textContent = '账号详情';
    detailSub.textContent = accountId;
    eventsBody.innerHTML = '<tr><td colspan="6" class="empty">正在加载…</td></tr>';
    alertsBody.innerHTML = '<tr><td colspan="3" class="empty">正在加载…</td></tr>';
    var query = 'account=' + encodeURIComponent(accountId);
    Promise.all([
      request({ method: 'GET', path: 'events', query: query }),
      request({ method: 'GET', path: 'alerts', query: query }),
    ]).then(function (results) {
      renderEvents(results[0].events || []);
      renderAlerts(results[1].alerts || []);
    }).catch(function (error) {
      eventsBody.innerHTML = '<tr><td colspan="6" class="empty">加载失败：' + esc(error.message) + '</td></tr>';
      alertsBody.innerHTML = '<tr><td colspan="3" class="empty">加载失败</td></tr>';
    });
  }

  function read(id) { return field(id).value.trim(); }

  function loadSettings() {
    settingsHint.className = 'form-hint';
    settingsHint.textContent = '正在读取…';
    request({ method: 'GET', path: 'settings' }).then(function (data) {
      var settings = data.settings || {};
      fillSettings(settings);
      settingsHint.textContent = '认证头不回显；其余字段保存后立即生效，覆盖宿主配置中的同名项。';
    }).catch(function (error) {
      settingsHint.className = 'form-hint fail';
      settingsHint.textContent = '读取设置失败：' + error.message;
    });
  }

  function fillSettings(settings) {
    field('f-webhook-url').value = settings.webhook_url || '';
    field('f-webhook-format').value = settings.webhook_format || 'generic';
    field('f-webhook-auth').value = '';
    field('f-webhook-auth').placeholder = settings.webhook_auth_header_configured
      ? '已配置（留空保持，勾选清空后删除）' : 'Header-Name: value';
    field('f-webhook-auth-clear').checked = false;
    field('f-email-url').value = settings.email_url || '';
    field('f-email-format').value = settings.email_format || 'generic';
    field('f-email-from').value = settings.email_from || '';
    field('f-email-to').value = (settings.email_to || []).join(', ');
    field('f-email-subject').value = settings.email_subject_template || '';
    field('f-email-body').value = settings.email_body_template || '';
    field('f-email-auth').value = '';
    field('f-email-auth').placeholder = settings.email_auth_header_configured
      ? '已配置（留空保持，勾选清空后删除）' : 'Header-Name: value';
    field('f-email-auth-clear').checked = false;
    field('f-alert-message').value = settings.alert_message || '';
  }

  function saveSettings() {
    var to = read('f-email-to');
    var payload = {
      webhook_url: read('f-webhook-url'),
      webhook_format: field('f-webhook-format').value,
      webhook_auth_header: read('f-webhook-auth'),
      clear_webhook_auth: field('f-webhook-auth-clear').checked,
      email_url: read('f-email-url'),
      email_format: field('f-email-format').value,
      email_from: read('f-email-from'),
      email_to: to ? to.split(/[,\n]/).map(function (item) { return item.trim(); }).filter(Boolean) : [],
      email_subject_template: read('f-email-subject'),
      email_body_template: read('f-email-body'),
      email_auth_header: read('f-email-auth'),
      clear_email_auth: field('f-email-auth-clear').checked,
      alert_message: read('f-alert-message'),
    };
    saveBtn.disabled = true;
    settingsHint.className = 'form-hint';
    settingsHint.textContent = '正在保存…';
    request({
      method: 'POST',
      path: 'settings',
      contentType: 'application/json',
      body: JSON.stringify(payload),
    }).then(function (data) {
      if (data.ok === false) {
        throw new Error(data.error || '保存被拒绝');
      }
      settingsHint.className = 'form-hint ok';
      settingsHint.textContent = '已保存，立即生效。';
      loadSettings();
      loadStatus();
    }).catch(function (error) {
      settingsHint.className = 'form-hint fail';
      settingsHint.textContent = '保存失败：' + error.message;
    }).finally(function () {
      saveBtn.disabled = false;
    });
  }

  function deliveryLine(d) {
    return esc(d.channel) + (d.ok ? ' ✓' : ' ✗ ' + esc(d.detail || ''));
  }

  refreshBtn.addEventListener('click', loadStatus);
  reloadSettingsBtn.addEventListener('click', loadSettings);
  resetBtn.addEventListener('click', function () {
    if (!window.confirm('恢复默认将删除本页保存的全部通知设置（含认证头），改回宿主配置。继续吗？')) {
      return;
    }
    resetBtn.disabled = true;
    request({
      method: 'POST',
      path: 'settings-reset',
      contentType: 'application/json',
      body: '{}',
    }).then(function () {
      fillSettings({});
      settingsHint.className = 'form-hint ok';
      settingsHint.textContent = '已恢复默认，改回宿主配置。';
      loadStatus();
    }).catch(function (error) {
      settingsHint.className = 'form-hint fail';
      settingsHint.textContent = '恢复失败：' + error.message;
    }).finally(function () {
      resetBtn.disabled = false;
    });
  });
  detailClose.addEventListener('click', function () { detailCard.hidden = true; });
  saveBtn.addEventListener('click', function (event) {
    event.preventDefault();
    saveSettings();
  });
  testBtn.addEventListener('click', function () {
    testBtn.disabled = true;
    request({
      method: 'POST',
      path: 'test-notify',
      contentType: 'application/json',
      body: '{}',
    }).then(function (data) {
      var deliveries = data.deliveries || [];
      var lines = deliveries.map(deliveryLine).join('<br>');
      if (!deliveries.length) {
        showToast('未配置通知渠道：先在下方「通知设置」填写 Webhook 或邮件地址', false);
      } else if (deliveries.every(function (d) { return d.ok; })) {
        toast.innerHTML = '测试通知已发送：<br>' + lines;
        toast.className = 'toast ok';
        toast.hidden = false;
      } else {
        toast.innerHTML = '部分渠道发送失败：<br>' + lines;
        toast.className = 'toast fail';
        toast.hidden = false;
      }
      window.clearTimeout(testBtn.timer);
      testBtn.timer = window.setTimeout(function () { toast.hidden = true; }, 10000);
    }).catch(function (error) {
      showToast('发送失败：' + error.message, false);
    }).finally(function () {
      testBtn.disabled = false;
    });
  });

  loadStatus();
  loadSettings();
})();
