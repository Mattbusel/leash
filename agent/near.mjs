// Shared NEAR helpers for the Leash scripts: config, keys, RPC views, transactions, events.
// Testnet only. Nothing in here ever touches mainnet.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Account, JsonRpcProvider, KeyPair, KeyPairSigner, actions } from 'near-api-js';

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export const ENV_FILE = path.join(ROOT, '.env.testnet');
export const DEPLOY_FILE = path.join(ROOT, 'deployments', 'testnet.json');
export const NETWORK = 'testnet';
export const RPC_URL = process.env.NEAR_RPC || 'https://test.rpc.fastnear.com';
export const EXPLORER_TX = 'https://testnet.nearblocks.io/txns/';
export const EXPLORER_ACCOUNT = 'https://testnet.nearblocks.io/address/';

export const TGAS = 1_000_000_000_000n;
export const YOCTO_PER_NEAR = 10n ** 24n;

export function near(amount) {
  // "0.05" -> yoctoNEAR string, exact decimal math.
  const [whole, frac = ''] = String(amount).split('.');
  const fracPadded = (frac + '0'.repeat(24)).slice(0, 24);
  return (BigInt(whole || '0') * YOCTO_PER_NEAR + BigInt(fracPadded || '0')).toString();
}

export function fmtNear(yocto, digits = 4) {
  const v = BigInt(yocto ?? 0);
  const whole = v / YOCTO_PER_NEAR;
  const frac = (v % YOCTO_PER_NEAR).toString().padStart(24, '0').slice(0, digits);
  return `${whole}.${frac}`;
}

if (!RPC_URL.includes('test')) {
  throw new Error(`Refusing to run: NEAR_RPC (${RPC_URL}) does not look like a testnet endpoint.`);
}

export const provider = new JsonRpcProvider({ url: RPC_URL });

// ---------------------------------------------------------------- env / secrets

export function readEnv(file = ENV_FILE) {
  const out = {};
  if (!fs.existsSync(file)) return out;
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    const m = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*)\s*$/);
    if (m) out[m[1]] = m[2];
  }
  return out;
}

export function appendEnv(entries, file = ENV_FILE) {
  const lines = Object.entries(entries).map(([k, v]) => `${k}=${v}`).join('\n') + '\n';
  fs.appendFileSync(file, lines, { mode: 0o600 });
}

export function readDeployments() {
  if (!fs.existsSync(DEPLOY_FILE)) {
    throw new Error(`No deployments file at ${DEPLOY_FILE}. Run: npm run setup`);
  }
  return JSON.parse(fs.readFileSync(DEPLOY_FILE, 'utf8'));
}

export function writeDeployments(d) {
  fs.mkdirSync(path.dirname(DEPLOY_FILE), { recursive: true });
  fs.writeFileSync(DEPLOY_FILE, JSON.stringify(d, null, 2) + '\n');
}

/** Account object that signs with the given private key (ed25519:...). */
export function accountWithKey(accountId, privateKey) {
  if (!privateKey) throw new Error(`Missing key for ${accountId} in ${ENV_FILE}`);
  const signer = new KeyPairSigner(KeyPair.fromString(privateKey));
  return new Account(accountId, provider, signer);
}

export function newKeyPair() {
  return KeyPair.fromRandom('ed25519');
}

// ---------------------------------------------------------------- RPC

export async function rpc(method, params) {
  const res = await fetch(RPC_URL, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 'leash', method, params }),
  });
  const j = await res.json();
  if (j.error) {
    const err = new Error(j.error.data || j.error.message || JSON.stringify(j.error));
    err.rpc = j.error;
    throw err;
  }
  return j.result;
}

export async function viewAccount(accountId) {
  try {
    return await rpc('query', { request_type: 'view_account', finality: 'final', account_id: accountId });
  } catch (e) {
    if (String(e.message).includes('does not exist') || e.rpc?.cause?.name === 'UNKNOWN_ACCOUNT') return null;
    throw e;
  }
}

export async function viewAccessKeys(accountId) {
  const r = await rpc('query', { request_type: 'view_access_key_list', finality: 'final', account_id: accountId });
  return r.keys;
}

export async function view(contractId, methodName, args = {}) {
  const r = await rpc('query', {
    request_type: 'call_function',
    finality: 'optimistic',
    account_id: contractId,
    method_name: methodName,
    args_base64: Buffer.from(JSON.stringify(args)).toString('base64'),
  });
  const text = Buffer.from(r.result).toString('utf8');
  return text ? JSON.parse(text) : null;
}

// ---------------------------------------------------------------- transactions

/**
 * Sign and send one transaction, wait for final execution, and summarise it:
 * hash, explorer link, success flag, return value, logs, NEP-297 events, failure.
 */
export async function send(account, receiverId, actionList) {
  const res = await account.signAndSendTransaction({
    receiverId,
    actions: actionList,
    waitUntil: 'FINAL',
    throwOnFailure: false,
  });
  return summarise(res);
}

export function call(account, contractId, methodName, args = {}, { gasTgas = 100, deposit = '0' } = {}) {
  return send(account, contractId, [
    actions.functionCall(methodName, args, BigInt(gasTgas) * TGAS, BigInt(deposit)),
  ]);
}

export function summarise(res) {
  const hash = res.transaction_outcome?.id || res.transaction?.hash;
  const logs = [];
  const failures = [];
  for (const o of [res.transaction_outcome, ...(res.receipts_outcome || [])]) {
    if (!o) continue;
    logs.push(...(o.outcome.logs || []));
    const st = o.outcome.status;
    if (st && st.Failure) failures.push(st.Failure);
  }
  const events = logs
    .filter((l) => l.startsWith('EVENT_JSON:'))
    .map((l) => {
      try { return JSON.parse(l.slice('EVENT_JSON:'.length)); } catch { return null; }
    })
    .filter(Boolean);
  let value = null;
  const sv = res.status?.SuccessValue;
  if (typeof sv === 'string' && sv.length) {
    const txt = Buffer.from(sv, 'base64').toString('utf8');
    try { value = JSON.parse(txt); } catch { value = txt; }
  }
  return {
    hash,
    url: hash ? EXPLORER_TX + hash : null,
    ok: res.status && 'SuccessValue' in res.status,
    value,
    logs,
    events,
    failure: res.status?.Failure || failures[0] || null,
    failures,
  };
}

export { actions, KeyPair };

/** Pull a readable error name out of an RPC InvalidTxError or a near-api-js error. */
export function describeError(e) {
  const parts = [];
  if (e?.type) parts.push(e.type);
  if (e?.name && e.name !== 'Error') parts.push(e.name);
  const msg = String(e?.message || e);
  parts.push(msg.length > 400 ? msg.slice(0, 400) + '...' : msg);
  return parts.join(': ');
}
