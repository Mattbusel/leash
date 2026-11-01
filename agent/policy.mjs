// The demo policy, with caps set relative to what the wallet already spent in the last 24h
// so the e2e test and the attack demo can be re-run on the same deployment.
import { view, near } from './near.mjs';

export async function relativeBaseline(d) {
  const sp = await view(d.wallet, 'get_spend');
  const spent = BigInt(sp.spent_24h);
  const vendorSpent = BigInt((sp.per_receiver.find(([a]) => a === d.vendor) || [0, '0'])[1]);
  const daily = spent + BigInt(near('0.5'));
  let perReceiver = vendorSpent + BigInt(near('0.3'));
  if (perReceiver > daily) perReceiver = daily;
  return {
    per_payment_cap: near('0.1'),
    daily_cap: daily.toString(),
    per_receiver_daily_cap: perReceiver.toString(),
    max_payments_per_hour: 200,
    allowlist_mode: false,
    allowlist: [d.vendor],
    denylist: [],
    contracts_require_allowlist: true,
    known_contracts: [],
    min_purpose_len: 8,
    registry: d.registry,
    auto_freeze_after_blocks: 0,
    guardians: [d.scouts[0]],
  };
}
