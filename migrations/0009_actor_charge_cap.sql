-- Provider-side hard cap for one run (Apify `maxTotalChargeUsd`). Pay-per-event Actors can bill more
-- than a configured estimate; this cap is enforced by the provider, in its own currency.
ALTER TABLE actor_registry ADD COLUMN max_total_charge_usd REAL;
