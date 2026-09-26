/* 降智监控管理页：仅通过 window.codexProxyPlugin 桥与宿主交互。 */
(function () {
  'use strict';

  var bridge = window.codexProxyPlugin;

  var accountsBody = document.getElementById('accounts-body');
  var historyBody = document.getElementById('history-body');
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
  var settingsModal = document.getElementById('settings-modal');
  var openSettingsBtn = document.getElementById('open-settings');
  var settingsCloseBtn = document.getElementById('settings-close');
  var saveBtn = document.getElementById('save-settings');
  var resetBtn = document.getElementById('reset-settings');
  var reloadSettingsBtn = document.getElementById('reload-settings');
  var settingsHint = document.getElementById('settings-hint');
  var testModal = document.getElementById('test-modal');
  var testCloseBtn = document.getElementById('test-close');
  var testAccountName = document.getElementById('test-account-name');
  var testAccountSelect = document.getElementById('test-account');
  var testModel = document.getElementById('test-model');
  var testEffort = document.getElementById('test-effort');
  var testModelsHint = document.getElementById('test-models-hint');
  var testSend = document.getElementById('test-send');
  var testResult = document.getElementById('test-result');
  var testAllBtn = document.getElementById('test-all-btn');
  var lastAccounts = [];

  var STATUS_LABELS = {
    unknown: '未知',
    healthy: '正常',
    suspect: '疑似',
    degraded: '降智',
    unobserved: '未观察',
  };

  /* 测试可选 reasoning effort；default 表示不指定，交回宿主默认。 */
  var EFFORTS = ['default', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

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

  /* 账号显示名：优先宿主 name/email，缺省显示截断的内部 ID。 */
  function accountLabel(item) {
    var name = item && item.name ? item.name : '';
    var id = item && item.account_id ? item.account_id : '';
    if (!name) return esc(id || '—');
    var idCell = id
      ? '<div class="mono muted small" title="' + esc(id) + '">' + esc(id) + '</div>'
      : '';
    return '<div class="account-name">' + esc(name) + '</div>' + idCell;
  }

  function renderAccounts(accounts) {
    if (!accounts.length) {
      accountsBody.innerHTML = '<tr><td colspan="9" class="empty">暂无账号观察数据</td></tr>';
      return;
    }
    accountsBody.innerHTML = accounts.map(function (account) {
      var status = STATUS_LABELS[account.status] || account.status;
      var cls = account.status === 'degraded' ? 'degraded'
        : account.status === 'suspect' ? 'suspect'
        : account.status === 'healthy' ? 'healthy' : '';
      return '<tr>'
        + '<td>' + accountLabel(account) + '</td>'
        + '<td>' + esc(account.provider || '—') + '</td>'
        + '<td>' + esc(account.model || '—') + '</td>'
        + '<td><span class="tag ' + cls + '">' + esc(status) + '</span>'
        + (account.enabled === false ? '<span class="tag">停用</span>' : '') + '</td>'
        + '<td>' + (account.signaled_events || 0) + ' / ' + (account.window_events || 0) + '</td>'
        + '<td>' + esc(formatMs(account.last_observed_at_ms)) + '</td>'
        + '<td>' + esc(account.last_alert_at_ms ? formatMs(account.last_alert_at_ms) : '—') + '</td>'
        + '<td>' + renderProbeCell(account.last_probe) + '</td>'
        + '<td>'
        + '<button class="btn link" data-test="' + esc(account.account_id) + '" data-name="' + esc(account.name || account.account_id) + '" type="button">测试</button>'
        + '<button class="btn link" data-account="' + esc(account.account_id) + '" data-name="' + esc(account.name || account.account_id) + '" type="button">详情</button>'
        + '<button class="btn link danger" data-clear="' + esc(account.account_id) + '" type="button">清除</button>'
        + '</td>'
        + '</tr>';
    }).join('');
    accountsBody.querySelectorAll('button[data-account]').forEach(function (button) {
      button.addEventListener('click', function () {
        loadDetail(button.getAttribute('data-account'), button.getAttribute('data-name'));
      });
    });
    accountsBody.querySelectorAll('button[data-test]').forEach(function (button) {
      button.addEventListener('click', function () {
        openTest(button.getAttribute('data-test'), button.getAttribute('data-name'));
      });
    });
    accountsBody.querySelectorAll('button[data-clear]').forEach(function (button) {
      button.addEventListener('click', function () {
        clearAccount(button.getAttribute('data-clear'));
      });
    });
  }

  function renderHistory(alerts) {
    var list = (alerts || []).slice().reverse();
    if (!list.length) {
      historyBody.innerHTML = '<tr><td colspan="4" class="empty">暂无告警；账号持续命中降智判定且过了冷却期才会记录</td></tr>';
      return;
    }
    historyBody.innerHTML = list.map(function (alert) {
      var verdict = alert.verdict || {};
      var kinds = (verdict.signal_kinds || []).map(function (kind) {
        return SIGNAL_LABELS[kind] || kind;
      }).join('、');
      var deliveries = (alert.deliveries || []).map(function (d) {
        return '<span class="' + (d.ok ? 'ok' : 'fail') + '">'
          + esc(d.channel) + (d.ok ? ' ✓' : ' ✗ ' + esc(d.detail))
          + '</span>';
      }).join('<br>') || '<span class="muted">未配置渠道</span>';
      return '<tr>'
        + '<td>' + esc(formatMs(alert.at_ms)) + '</td>'
        + '<td>' + accountLabel(alert) + '</td>'
        + '<td>' + esc((verdict.signaled_requests || 0) + ' 个信号：' + kinds) + '</td>'
        + '<td>' + deliveries + '</td>'
        + '</tr>';
    }).join('');
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
      lastAccounts = data.accounts || [];
      renderHistory(data.alerts || []);
      renderConfig(data.config || {});
      var total = (data.accounts || []).length;
      var degraded = (data.accounts || []).filter(function (a) { return a.status === 'degraded'; }).length;
      var observed = (data.accounts || []).filter(function (a) { return a.status !== 'unobserved'; }).length;
      statusSub.textContent = total
        ? ('共 ' + total + ' 个账号（' + observed + ' 个已观察），' + degraded + ' 个降智')
        : '暂无账号；插件在后台持续统计请求终态';
      if (data.accounts_error) {
        statusSub.textContent += '；' + data.accounts_error;
      }
    }).catch(function (error) {
      statusSub.textContent = '加载失败：' + error.message;
    });
  }

  function loadDetail(accountId, label) {
    if (!accountId) return;
    detailCard.hidden = false;
    detailTitle.textContent = '账号详情';
    detailSub.textContent = (label && label !== accountId)
      ? (label + '（' + accountId + '）')
      : accountId;
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

  function clearAccount(accountId) {
    if (!accountId) return;
    if (!window.confirm('清除该账号的窗口事件、判定计数与告警历史？监控会在下一次请求终态后重新开始。')) {
      return;
    }
    request({
      method: 'POST',
      path: 'account-clear',
      contentType: 'application/json',
      body: JSON.stringify({ account: accountId }),
    }).then(function () {
      if (!detailCard.hidden && detailSub.textContent.indexOf(accountId) !== -1) {
        detailCard.hidden = true;
      }
      showToast('已清除账号观察记录', true);
      loadStatus();
    }).catch(function (error) {
      showToast('清除失败：' + error.message, false);
    });
  }

  function read(id) { return field(id).value.trim(); }

  function renderProbeCell(probe) {
    if (!probe || !probe.at_ms) {
      return '<span class="muted">—</span>';
    }
    var cls = probe.result === 'correct' ? 'ok' : 'fail';
    var text = probe.result === 'correct' ? '答对' : (probe.result === 'wrong' ? '答错' : '失败');
    var spec = esc(probe.model || '') + (probe.effort ? ' · effort=' + esc(probe.effort) : '');
    return '<span class="' + cls + '" title="' + esc(probe.detail || '') + '">'
      + esc(text) + '</span> <span class="muted small">' + esc(formatMs(probe.at_ms)) + '</span>'
      + '<div class="muted small">' + spec + '</div>';
  }

  function openTest(accountId, label) {
    testResult.innerHTML = '<span class="muted">尚未发送</span>';
    testModal.hidden = false;
    fillTestAccounts(lastAccounts, accountId);
    if (!lastAccounts.length) {
      // 页面尚未加载账号时兜底拉一次，确保任意时刻都能测试。
      request({ method: 'GET', path: 'status' }).then(function (data) {
        lastAccounts = data.accounts || [];
        fillTestAccounts(lastAccounts, accountId);
      });
    }
    var current = lastAccounts.filter(function (a) { return a.account_id === testAccountSelect.value; })[0];
    testAccountName.textContent = current
      ? ((current.name || current.account_id) + '（' + current.account_id + '）')
      : (label || '选择账号');
    loadModels();
  }

  function fillTestAccounts(accounts, preferId) {
    var options = (accounts || []).map(function (account) {
      var label = account.name || account.account_id;
      var suffix = account.status && STATUS_LABELS[account.status]
        ? ' · ' + STATUS_LABELS[account.status] : '';
      return '<option value="' + esc(account.account_id) + '">' + esc(label + suffix) + '</option>';
    });
    if (!options.length) {
      testAccountSelect.innerHTML = '<option value="">没有可选账号</option>';
      testAccountName.textContent = '没有可选账号';
      return;
    }
    testAccountSelect.innerHTML = options.join('');
    if (preferId) {
      testAccountSelect.value = preferId;
    }
  }

  testAccountSelect.addEventListener('change', function () {
    var current = lastAccounts.filter(function (a) { return a.account_id === testAccountSelect.value; })[0];
    testAccountName.textContent = current
      ? ((current.name || current.account_id) + '（' + current.account_id + '）')
      : testAccountSelect.value;
  });

  function closeTest() {
    testModal.hidden = true;
  }

  function loadModels() {
    if (!testEffort.options.length) {
      testEffort.innerHTML = EFFORTS.map(function (value) {
        return '<option value="' + value + '">'
          + (value === 'default' ? 'default（不指定）' : value) + '</option>';
      }).join('');
    }
    testModelsHint.textContent = '正在加载模型…';
    testModel.disabled = true;
    request({ method: 'GET', path: 'models' }).then(function (data) {
      var models = data.models || [];
      if (!models.length) {
        testModelsHint.textContent = data.error || '没有可用模型；请先在宿主配置客户端 Key 与账号';
        return;
      }
      testModel.innerHTML = models.map(function (item) {
        return '<option value="' + esc(item.model) + '" data-key="' + esc(item.key) + '">'
          + esc(item.model) + '</option>';
      }).join('');
      testModel.disabled = false;
      testModelsHint.textContent = '共 ' + models.length + ' 个模型；测试借用第一个可见 Key 的身份发送';
    }).catch(function (error) {
      testModelsHint.textContent = '模型加载失败：' + error.message;
    });
  }

  function runTest() {
    var option = testModel.selectedOptions && testModel.selectedOptions[0];
    var model = testModel.value;
    var account = testAccountSelect.value;
    if (!account) {
      testResult.innerHTML = '<span class="fail">请先选择账号</span>';
      return;
    }
    if (!model) {
      testResult.innerHTML = '<span class="fail">请先选择模型</span>';
      return;
    }
    var key = option ? option.getAttribute('data-key') : '';
    var effort = testEffort.value;
    var effortHint = effort && effort !== 'default' ? '（effort=' + esc(effort) + '）' : '';
    testSend.disabled = true;
    testResult.innerHTML = '<span class="muted">发送中：经 ' + esc(model) + effortHint + ' 提问糖果题（正确答案 21），最长可能需要数十秒…</span>';
    request({
      method: 'POST',
      path: 'candy-test',
      contentType: 'application/json',
      body: JSON.stringify({ account: account, model: model, key: key, effort: effort }),
    }).then(function (data) {
      var cls = data.result === 'correct' ? 'ok' : 'fail';
      var label = data.result === 'correct' ? '答对 ✓' : (data.result === 'wrong' ? '答错 ✗' : '调用失败');
      var html = '<span class="' + cls + '">' + esc(label) + '</span> ' + esc(data.detail || '');
      if (data.excerpt) {
        html += '<div class="probe-excerpt mono">' + esc(data.excerpt) + '</div>';
      }
      testResult.innerHTML = html;
      loadStatus();
    }).catch(function (error) {
      testResult.innerHTML = '<span class="fail">测试失败：' + esc(error.message) + '</span>';
    }).finally(function () {
      testSend.disabled = false;
    });
  }

  function setTab(name) {
    document.querySelectorAll('.settings-tabs button').forEach(function (button) {
      button.classList.toggle('active', button.getAttribute('data-tab') === name);
    });
    document.querySelectorAll('[data-panel]').forEach(function (panel) {
      panel.hidden = panel.getAttribute('data-panel') !== name;
    });
  }

  function openSettings() {
    settingsModal.hidden = false;
    loadSettings();
  }

  function closeSettings() {
    settingsModal.hidden = true;
  }

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
  openSettingsBtn.addEventListener('click', openSettings);
  settingsCloseBtn.addEventListener('click', closeSettings);
  settingsModal.addEventListener('click', function (event) {
    if (event.target && event.target.hasAttribute('data-close')) {
      closeSettings();
    }
  });
  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape') {
      if (!settingsModal.hidden) closeSettings();
      if (!testModal.hidden) closeTest();
    }
  });
  document.querySelectorAll('.settings-tabs button').forEach(function (button) {
    button.addEventListener('click', function () {
      setTab(button.getAttribute('data-tab'));
    });
  });
  testCloseBtn.addEventListener('click', closeTest);
  testModal.addEventListener('click', function (event) {
    if (event.target && event.target.hasAttribute('data-close')) {
      closeTest();
    }
  });
  testSend.addEventListener('click', runTest);
  testAllBtn.addEventListener('click', function () { openTest('', ''); });
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
        showToast('未配置通知渠道：打开「通知设置」填写 Webhook 或邮件地址', false);
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
})();
