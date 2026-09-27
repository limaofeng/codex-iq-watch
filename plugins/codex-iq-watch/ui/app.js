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
  var testKey = document.getElementById('test-key');
  var testModel = document.getElementById('test-model');
  var testEffort = document.getElementById('test-effort');
  var testModelsHint = document.getElementById('test-models-hint');
  var testSend = document.getElementById('test-send');
  var testResult = document.getElementById('test-result');
  var testAllBtn = document.getElementById('test-all-btn');
  var lastAccounts = [];
  var lastAlerts = [];
  var lastModels = { keys: [], models: [] };
  var PREFERRED_MODEL = 'gpt-6-astra';
  var PREFERRED_EFFORT = 'low';

  var STATUS_LABELS = {
    unknown: '未知',
    healthy: '正常',
    suspect: '疑似',
    degraded: '降智',
    unobserved: '未观察',
  };

  /* 测试可选 reasoning effort；default 表示不指定，交回宿主默认。 */
  var EFFORTS = ['default', 'low', 'medium', 'high', 'xhigh', 'max'];

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

  function deliveryOutcome(alert) {
    var deliveries = alert.deliveries || [];
    if (!deliveries.length) return 'unconfirmed';
    if (deliveries.some(function (d) { return d.ok; })) return 'ok';
    if (deliveries.every(function (d) { return !d.ok && (d.detail || '').indexOf('未确认') >= 0; })) {
      return 'unconfirmed';
    }
    return deliveries.every(function (d) { return d.ok; }) ? 'ok' : 'failed';
  }

  function renderAccounts(accounts) {
    lastAccounts = accounts;
    var keyword = (field('account-search').value || '').trim().toLowerCase();
    var status = field('account-status').value;
    var filtered = accounts.filter(function (account) {
      if (status === 'disabled') {
        if (account.enabled !== false) return false;
      } else if (status && account.status !== status) return false;
      if (!keyword) return true;
      var text = [account.account_id, account.name, account.provider, account.model,
        STATUS_LABELS[account.status] || account.status].join(' ').toLowerCase();
      return text.indexOf(keyword) >= 0;
    });
    if (!filtered.length) {
      accountsBody.innerHTML = '<tr><td colspan="9" class="empty">'
        + (accounts.length ? '没有匹配的账号' : '暂无账号观察数据') + '</td></tr>';
      return;
    }
    accountsBody.innerHTML = filtered.map(function (account) {
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
    lastAlerts = alerts || [];
    var list = lastAlerts.slice().reverse();
    var keyword = (field('alert-search').value || '').trim().toLowerCase();
    var delivery = field('alert-delivery').value;
    list = list.filter(function (alert) {
      if (delivery && deliveryOutcome(alert) !== delivery) return false;
      if (!keyword) return true;
      var verdict = alert.verdict || {};
      var text = [alert.account_id, alert.account_name,
        (verdict.signal_kinds || []).map(function (k) { return SIGNAL_LABELS[k] || k; }).join(' ')]
        .join(' ').toLowerCase();
      return text.indexOf(keyword) >= 0;
    });
    if (!list.length) {
      historyBody.innerHTML = '<tr><td colspan="4" class="empty">'
        + (lastAlerts.length ? '没有匹配的告警' : '暂无告警；账号持续命中降智判定且过了冷却期才会记录')
        + '</td></tr>';
      return;
    }
    historyBody.innerHTML = list.map(function (alert) {
      var verdict = alert.verdict || {};
      var kinds = (verdict.signal_kinds || []).map(function (kind) {
        return SIGNAL_LABELS[kind] || kind;
      }).join('、');
      var deliveries = (alert.deliveries || []).map(function (d) {
        var unknown = !d.ok && (d.detail || '').indexOf('未确认') >= 0;
        return '<span class="' + (d.ok ? 'ok' : unknown ? 'muted' : 'fail') + '">'
          + esc(d.channel) + (d.ok ? ' ✓' : unknown ? ' ？未确认' : ' ✗ ' + esc(d.detail))
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

  function renderEvents(events) {
    var list = (events || []).slice().reverse();
    if (!list.length) {
      eventsBody.innerHTML = '<tr><td colspan="7" class="empty">暂无信号记录</td></tr>';
      return;
    }
    eventsBody.innerHTML = list.map(function (event) {
      var signals = (event.signals || []).map(function (signal) {
        return '<span class="tag signal">' + esc(SIGNAL_LABELS[signal] || signal) + '</span>';
      }).join('') || '<span class="muted">—</span>';
      var cache = (event.cached_tokens == null ? '—' : formatTokens(event.cached_tokens))
        + ' / ' + (event.input_tokens == null ? '—' : formatTokens(event.input_tokens));
      var latency = event.latency_ms != null ? (event.latency_ms / 1000).toFixed(1) + 's' : '—';
      var firstToken = event.first_token_ms != null ? (event.first_token_ms / 1000).toFixed(1) + 's' : '—';
      return '<tr>'
        + '<td>' + esc(formatMs(event.observed_at_ms)) + '</td>'
        + '<td>' + esc(OUTCOME_LABELS[event.outcome] || event.outcome) + '</td>'
        + '<td>' + signals + '</td>'
        + '<td>' + esc(event.upstream_status || event.client_status || '—') + '</td>'
        + '<td>' + esc(cache) + '</td>'
        + '<td>' + esc(latency) + '</td>'
        + '<td>' + esc(firstToken) + '</td>'
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
      var total = (data.accounts || []).length;
      var degraded = (data.accounts || []).filter(function (a) { return a.status === 'degraded'; }).length;
      var observed = (data.accounts || []).filter(function (a) { return a.status !== 'unobserved'; }).length;
      statusSub.textContent = total
        ? ('共 ' + total + ' 个账号（' + observed + ' 个已观察），' + degraded + ' 个降智')
        : '暂无账号；插件在后台持续统计请求终态';
      if (data.accounts_error) statusSub.textContent += ' / ' + data.accounts_error;
      if (data.storage_warning) statusSub.textContent += ' / ' + data.storage_warning;
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
    eventsBody.innerHTML = '<tr><td colspan="7" class="empty">正在加载…</td></tr>';
    alertsBody.innerHTML = '<tr><td colspan="3" class="empty">正在加载…</td></tr>';
    var query = 'account=' + encodeURIComponent(accountId);
    Promise.all([
      request({ method: 'GET', path: 'events', query: query }),
      request({ method: 'GET', path: 'alerts', query: query }),
    ]).then(function (results) {
      renderEvents(results[0].events || []);
      renderAlerts(results[1].alerts || []);
    }).catch(function (error) {
      eventsBody.innerHTML = '<tr><td colspan="7" class="empty">加载失败：' + esc(error.message) + '</td></tr>';
      alertsBody.innerHTML = '<tr><td colspan="3" class="empty">加载失败</td></tr>';
    });
  }

  var confirmDialog = field('confirm-dialog');
  var confirmAction = null;
  var confirmBusy = false;
  var confirmFocus = null;
  function askConfirm(title, message, action) {
    if (confirmDialog.open || confirmBusy) return;
    confirmFocus = document.activeElement;
    confirmAction = action;
    field('confirm-title').textContent = title;
    field('confirm-message').textContent = message;
    field('confirm-error').textContent = '';
    confirmDialog.showModal();
    field('confirm-cancel').focus();
  }
  confirmDialog.addEventListener('cancel', function (event) {
    if (confirmBusy) event.preventDefault();
  });
  confirmDialog.addEventListener('close', function () {
    confirmAction = null;
    if (confirmFocus && confirmFocus.isConnected) confirmFocus.focus();
  });
  field('confirm-cancel').addEventListener('click', function () {
    if (!confirmBusy) confirmDialog.close();
  });
  field('confirm-accept').addEventListener('click', async function () {
    if (confirmBusy || !confirmAction) return;
    confirmBusy = true;
    field('confirm-cancel').disabled = true;
    field('confirm-accept').disabled = true;
    field('confirm-error').textContent = '';
    try {
      await confirmAction();
      confirmDialog.close();
    } catch (error) {
      field('confirm-error').textContent = '操作失败：' + error.message;
    } finally {
      confirmBusy = false;
      field('confirm-cancel').disabled = false;
      field('confirm-accept').disabled = false;
    }
  });

  function clearAccount(accountId) {
    if (!accountId) return;
    askConfirm('清除账号记录', '清除该账号的窗口事件、判定计数与告警历史，并解除降智隔离，监控会在下一次请求终态后重新开始', async function () {
      await request({
        method: 'POST', path: 'account-clear', contentType: 'application/json',
        body: JSON.stringify({ account: accountId }),
      });
      if (!detailCard.hidden && detailSub.textContent.indexOf(accountId) !== -1) detailCard.hidden = true;
      showToast('已清除账号观察记录', true);
      loadStatus();
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
      testEffort.value = PREFERRED_EFFORT;
    }
    testModelsHint.textContent = '正在加载 Key 与模型…';
    testModel.disabled = true;
    request({ method: 'GET', path: 'models' }).then(function (data) {
      lastModels = data;
      fillKeys();
      fillModels();
    }).catch(function (error) {
      testModelsHint.textContent = '模型加载失败：' + error.message;
    });
  }

  function fillKeys() {
    var keys = (lastModels && lastModels.keys) || [];
    if (!keys.length) {
      testKey.innerHTML = '<option value="">（自动：第一个可见 Key）</option>';
      return;
    }
    testKey.innerHTML = keys.map(function (key) {
      return '<option value="' + esc(key.id) + '">' + esc(key.name || key.id)
        + '（' + (key.models || []).length + ' 个模型）</option>';
    }).join('');
    // 默认选中含偏好模型的 Key，便于直接用 gpt-6-astra。
    var prefer = keys.filter(function (key) {
      return (key.models || []).indexOf(PREFERRED_MODEL) !== -1;
    })[0] || keys[0];
    testKey.value = prefer.id;
  }

  testKey.addEventListener('change', function () { fillModels(); });

  function fillModels(preferModel) {
    var keys = (lastModels && lastModels.keys) || [];
    var fallback = (lastModels && lastModels.models) || [];
    var key = keys.filter(function (item) { return item.id === testKey.value; })[0];
    var names = key ? (key.models || [])
      : Array.from(new Set(fallback.map(function (item) { return item.model; })));
    if (!names.length) {
      testModel.innerHTML = '<option value="">没有可用模型</option>';
      testModel.disabled = true;
      testModelsHint.textContent = (lastModels && lastModels.error)
        || '没有可用模型；请先在宿主配置客户端 Key 与账号';
      return;
    }
    testModel.innerHTML = names.map(function (name) {
      return '<option value="' + esc(name) + '">' + esc(name) + '</option>';
    }).join('');
    var want = preferModel || PREFERRED_MODEL;
    testModel.value = names.indexOf(want) !== -1 ? want : names[0];
    testModel.disabled = false;
    testModelsHint.textContent = '经所选 Key 发送；模型范围为该 Key 可见列表';
  }

  function runTest() {
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
    var key = testKey.value;
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
      fillDetect(data.effective || {});
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

  /* 检测参数按生效值回填；单位换算：ms → 分钟/秒。 */
  function fillDetect(effective) {
    field('f-enabled').checked = effective.enabled !== false;
    field('f-watch-providers').value = (effective.watch_providers || []).join(', ');
    field('f-window-min').value = effective.window_ms ? Math.round(effective.window_ms / 60000) : '';
    field('f-cooldown-min').value = effective.cooldown_ms ? Math.round(effective.cooldown_ms / 60000) : '';
    field('f-first-token-s').value = effective.first_token_ms ? (effective.first_token_ms / 1000) : '';
    field('f-cache-min-input').value = effective.cache_min_input_tokens != null
      ? effective.cache_min_input_tokens : '';
    field('f-min-signaled').value = effective.min_signaled_requests != null
      ? effective.min_signaled_requests : '';
    field('f-min-kinds').value = effective.min_signal_kinds != null
      ? effective.min_signal_kinds : '';
    field('f-consecutive').value = effective.consecutive_triggers != null
      ? effective.consecutive_triggers : '';
    field('f-pause-degraded').checked = effective.schedule_exclude_degraded === true;
    field('f-all-degraded').value = effective.schedule_all_degraded || '';
  }

  /* 数值字段：留空 → null（清除覆盖回宿主配置），有值 → 换算回毫秒/原始单位。 */
  function detectPayload() {
    var providers = read('f-watch-providers');
    function num(id, factor) {
      var value = read(id);
      if (!value) return null;
      var parsed = Number(value);
      return Number.isFinite(parsed) ? Math.round(parsed * factor) : null;
    }
    return {
      enabled: field('f-enabled').checked,
      watch_providers: providers
        ? providers.split(/[,\n]/).map(function (item) { return item.trim(); }).filter(Boolean)
        : [],
      window_ms: num('f-window-min', 60000),
      cooldown_ms: num('f-cooldown-min', 60000),
      first_token_ms: num('f-first-token-s', 1000),
      cache_min_input_tokens: num('f-cache-min-input', 1),
      min_signaled_requests: num('f-min-signaled', 1),
      min_signal_kinds: num('f-min-kinds', 1),
      consecutive_triggers: num('f-consecutive', 1),
      schedule_exclude_degraded: field('f-pause-degraded').checked,
      schedule_all_degraded: field('f-all-degraded').value || null,
    };
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
    var detect = detectPayload();
    Object.keys(detect).forEach(function (key) { payload[key] = detect[key]; });
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
    // 顶层确认由 dialog 处理 Escape，不连带关闭下层设置页。
    if (confirmDialog.open) return;
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
    askConfirm('恢复默认设置', '删除本页保存的全部通知设置（含认证头），改回宿主配置', function () {
    return request({
      method: 'POST',
      path: 'settings-reset',
      contentType: 'application/json',
      body: '{}',
    }).then(function () {
      fillSettings({});
      settingsHint.className = 'form-hint ok';
      settingsHint.textContent = '已恢复默认，改回宿主配置。';
      loadStatus();
    });
    });
  });
  detailClose.addEventListener('click', function () { detailCard.hidden = true; });
  var filterApply = function () {
    renderAccounts(lastAccounts);
    renderHistory(lastAlerts);
  };
  field('account-search').addEventListener('input', filterApply);
  field('account-status').addEventListener('change', filterApply);
  field('alert-search').addEventListener('input', filterApply);
  field('alert-delivery').addEventListener('change', filterApply);
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
