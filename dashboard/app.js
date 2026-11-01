// Leash dashboard: read-only, plain ES module, no build step. Every number comes from
// NEAR testnet JSON-RPC view calls against the wallet and registry contracts.
import { CONFIG } from './config.js';

const qs = new URLSearchParams(location.search);
const WALLET = qs.get('wallet') || CONFIG.wallet;
const REGISTRY = qs.get('registry') || CONFIG.registry;
const RPCS = qs.get('rpc') ? [qs.get('rpc')] : CONFIG.rpcs;
const EXPLORER = 'https://testnet.nearblocks.io';
const REFRESH_MS = 10_000;
const YOCTO = 10n ** 24n;

let filter = 'all';
let lastHistory = [];

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

function fmt(yocto, digits = 4) {
  const v = BigInt(yocto ?? 0);
  const frac = (v % YOCTO).toString().padStart(24, '0').slice(0, digits);
  return `${v / YOCTO}.${frac}`;
}
function pct(a, b) {
  const B = BigInt(b || 0);
  if (B === 0n) return 0;
  return Math.min(100, Number((BigInt(a || 0) * 10000n) / B) / 100);
}
function when(ns) {
  const d = new Date(Number(BigInt(ns) / 1_000_000n));
  return d.toISOString().replace('T', ' ').slice(0, 19) + 'Z';
}
function acct(id) {
  return `<a class="mono" href="${EXPLORER}/address/${encodeURIComponent(id)}" target="_blank" rel="noopener">${esc(id)}</a>`;
}

async function rpc(method, params) {
  let lastErr;
  for (const url of RPCS) {
    try {
      const r = await fetch(url, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ jsonrpc: '2.0', id: 'dash', method, params }),
      });
      const j = await r.json();
      if (j.error) throw new Error(j.error.data || j.error.message);
      return j.result;
    } catch (e) { lastErr = e; }
  }
  throw lastErr;
}

async function view(contract, method, args = {}) {
  const b64 = btoa(String.fromCharCode(...new TextEncoder().encode(JSON.stringify(args))));
  const r = await rpc('query', { request_type: 'call_function', finality: 'optimistic', account_id: contract, method_name: method, args_base64: b64 });
  const txt = new TextDecoder().decode(new Uint8Array(r.result));
  return txt ? JSON.parse(txt) : null;
}

function setMeter(id, value, cap) {
  const p = pct(value, cap);
  const el = $(id);
  el.style.width = `${p}%`;
  el.className = 'fill' + (p >= 100 ? ' full' : p >= 75 ? ' hot' : '');
}

function renderState(s, sp) {
  $('wallet-link').href = `${EXPLORER}/address/${WALLET}`;
  $('wallet-link').textContent = WALLET;
  $('owner').innerHTML = acct(s.owner_id);
  $('balance').textContent = `${fmt(s.balance)} NEAR`;
  $('available').textContent = `${fmt(s.available)} NEAR`;
  $('paid').textContent = `${s.paid_count} payments, ${fmt(s.paid_total)} NEAR`;
  $('blocked').textContent = `${s.blocked_count} attempts`;
  const fz = $('frozen');
  fz.textContent = s.frozen ? `FROZEN: ${s.frozen_reason || ''}` : 'Live: payments allowed within policy';
  fz.className = 'badge ' + (s.frozen ? 'frozen' : 'live');

  setMeter('m24', sp.spent_24h, sp.daily_cap);
  $('m24-text').textContent = `${fmt(sp.spent_24h)} / ${fmt(sp.daily_cap)} NEAR`;
  setMeter('mh', String(sp.attempts_last_hour), String(sp.max_payments_per_hour));
  $('mh-text').textContent = `${sp.attempts_last_hour} / ${sp.max_payments_per_hour} (${sp.blocked_last_hour} blocked)`;
  const rows = sp.per_receiver.map(([a, v]) => {
    const p = pct(v, sp.per_receiver_daily_cap);
    return `<div class="rec"><div class="meter-label"><span class="mono">${esc(a)}</span><span>${fmt(v)} / ${fmt(sp.per_receiver_daily_cap)}</span></div>
      <div class="meter"><div class="fill${p >= 100 ? ' full' : p >= 75 ? ' hot' : ''}" style="width:${p}%"></div></div></div>`;
  });
  $('per-receiver').innerHTML = rows.join('') || '<p class="small muted">No spend in the last 24 hours.</p>';
}

function renderKeys(keys) {
  $('keys').innerHTML = keys.map((k) => {
    const p = k.access_key.permission;
    if (p === 'FullAccess') {
      return `<div class="keyrow"><span class="chip rejected">full access</span> <span class="small muted">held by the owner, for upgrades</span><div class="mono">${esc(k.public_key)}</div></div>`;
    }
    const f = p.FunctionCall;
    return `<div class="keyrow"><span class="chip info">function call</span> <span class="small">allowance left ${f.allowance ? fmt(f.allowance) + ' NEAR' : 'unlimited'}</span>
      <div class="mono">${esc(k.public_key)}</div>
      <div class="small">may call <code>${f.method_names.map(esc).join('</code>, <code>') || 'any method'}</code> on ${acct(f.receiver_id)}</div></div>`;
  }).join('');
}

function renderPending(list) {
  $('pending-count').textContent = list.length;
  const tb = $('pending').querySelector('tbody');
  tb.innerHTML = list.map((p) => `<tr><td>${esc(p.id)}</td><td class="num">${fmt(p.amount)}</td><td>${acct(p.receiver_id)}</td>
    <td><span class="chip pending">${esc(p.reason)}</span></td><td>${esc(p.purpose)}</td><td class="num">${when(p.created_at)}</td></tr>`).join('')
    || '<tr><td colspan="6" class="empty">Nothing waiting for the owner.</td></tr>';
}

function renderFilters() {
  const kinds = ['all', 'paid', 'blocked', 'pending', 'approved', 'rejected', 'failed'];
  $('filters').innerHTML = kinds.map((k) => `<button type="button" data-k="${k}" aria-pressed="${k === filter}">${k}</button>`).join('');
  $('filters').querySelectorAll('button').forEach((b) => b.addEventListener('click', () => {
    filter = b.dataset.k;
    renderFilters();
    renderHistory(lastHistory);
  }));
}

function renderHistory(list) {
  lastHistory = list;
  const rows = list.filter((h) => filter === 'all' || h.status === filter).slice(0, 40);
  $('history').querySelector('tbody').innerHTML = rows.map((h) => `<tr><td>${esc(h.id)}</td><td class="num">${when(h.ts)}</td>
    <td><span class="chip ${esc(h.status)}">${esc(h.status)}</span></td><td class="num">${fmt(h.amount)}</td><td>${acct(h.receiver_id)}</td>
    <td class="mono small">${esc(h.reason || '')}</td><td>${esc(h.purpose)}</td></tr>`).join('')
    || '<tr><td colspan="7" class="empty">No decisions yet.</td></tr>';
}

function renderPolicy(p) {
  const list = (a) => (a.length ? a.map(acct).join('<br>') : '<span class="muted">none</span>');
  const items = [
    ['Per payment cap', `${fmt(p.per_payment_cap)} NEAR`],
    ['Rolling 24h cap', `${fmt(p.daily_cap)} NEAR`],
    ['Per receiver 24h cap', `${fmt(p.per_receiver_daily_cap)} NEAR`],
    ['Max attempts per hour', p.max_payments_per_hour],
    ['Allowlist mode', p.allowlist_mode ? 'on: only allowlisted receivers' : 'off'],
    ['Allowlist', list(p.allowlist)],
    ['Denylist', list(p.denylist)],
    ['Contracts need allowlist', p.contracts_require_allowlist ? 'yes' : 'no'],
    ['Known contracts', list(p.known_contracts)],
    ['Min purpose length', `${p.min_purpose_len} chars`],
    ['Blocklist registry', p.registry ? acct(p.registry) : '<span class="muted">none</span>'],
    ['Auto-freeze after', p.auto_freeze_after_blocks ? `${p.auto_freeze_after_blocks} blocks/hour` : 'off'],
    ['Guardians (can freeze)', list(p.guardians)],
  ];
  $('policy').innerHTML = items.map(([k, v]) => `<dt>${k}</dt><dd>${v}</dd>`).join('');
}

async function renderBlocklist() {
  const [cfg, targets] = await Promise.all([view(REGISTRY, 'get_config'), view(REGISTRY, 'list_targets', {})]);
  $('threshold').textContent = cfg.threshold;
  const detail = await Promise.all(targets.map((t) => view(REGISTRY, 'get_reports', { account_id: t.account_id })));
  $('blocklist').innerHTML = targets.map((t, i) => `<div class="target">
      <span class="chip ${t.blocked ? 'blocked' : 'pending'}">${t.blocked ? 'blocked' : 'reported'}</span> ${acct(t.account_id)}
      <span class="small muted">${t.reports}/${t.threshold} reports${t.blocked_by ? ', by ' + esc(t.blocked_by) : ''}${t.contract_code_hash ? ', contract code ' + esc(t.contract_code_hash.slice(0, 10)) + '...' : ''}</span>
      <ul class="small">${detail[i].map((r) => `<li>${acct(r.reporter)}: ${esc(r.reason)} <a href="${esc(r.evidence_url)}" target="_blank" rel="noopener nofollow">evidence</a></li>`).join('')}</ul>
    </div>`).join('') || '<p class="small muted">No reports yet.</p>';
}

async function refresh() {
  try {
    const [s, sp, p, pend, hist, keys] = await Promise.all([
      view(WALLET, 'get_state'), view(WALLET, 'get_spend'), view(WALLET, 'get_policy'),
      view(WALLET, 'get_pending', {}), view(WALLET, 'get_history', { limit: 100 }),
      rpc('query', { request_type: 'view_access_key_list', finality: 'final', account_id: WALLET }).then((r) => r.keys),
    ]);
    renderState(s, sp);
    renderPolicy(p);
    renderPending(pend);
    renderHistory(hist);
    renderKeys(keys);
    await renderBlocklist();
    $('updated').textContent = `updated ${new Date().toLocaleTimeString()}`;
  } catch (e) {
    $('updated').textContent = `RPC error: ${e.message}`;
  }
}

$('rpc').textContent = RPCS[0];
$('registry-link').href = `${EXPLORER}/address/${REGISTRY}`;
$('registry-link').textContent = REGISTRY;
renderFilters();
refresh();
setInterval(refresh, REFRESH_MS);
