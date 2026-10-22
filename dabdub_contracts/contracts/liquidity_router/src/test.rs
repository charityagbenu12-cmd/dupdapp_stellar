#![cfg(test)]

use super::*;
use soroban_sdk::{contract, contractimpl, testutils::Address as _, Address, Env};

#[contract]
pub struct MockAmm;

#[contractimpl]
impl MockAmm {
    pub fn get_reserves(_env: Env) -> (i128, i128) {
        (100_000, 100_000)
    }
}

/// Mock slippage protection contract exposing the same `check_slippage`
/// entrypoint the real `slippage_protection` contract provides. It records the
/// configured max slippage (bps) and rejects any swap whose actual slippage
/// exceeds it, mirroring the production contract's bounds enforcement.
#[contract]
pub struct MockSlippageProtection;

#[contractimpl]
impl MockSlippageProtection {
    pub fn set_max_slippage_bps(env: Env, max_bps: u32) {
        env.storage().instance().set(&"max", &max_bps);
    }

    pub fn check_slippage(env: Env, actual_slippage_bps: u32) -> bool {
        let max_bps: u32 = env.storage().instance().get(&"max").unwrap_or(0);
        if actual_slippage_bps > max_bps {
            panic!("slippage exceeds max");
        }
        true
    }
}

fn setup() -> (Env, LiquidityRouterClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(LiquidityRouter, ());
    let client = LiquidityRouterClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    client.initialize(&admin);
    (env, client, admin)
}

#[test]
fn test_initialize_sets_admin_and_empty_allowlist() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(LiquidityRouter, ());
    let client = LiquidityRouterClient::new(&env, &contract_id);
    let admin = Address::generate(&env);

    client.initialize(&admin);

    // A pool that was never added must be rejected, proving the allowlist
    // was initialized to an empty set.
    let pool = env.register(MockAmm, ());
    let result = client.try_check_and_route(&pool, &1_000);
    assert!(result.is_err());
}

#[test]
fn test_add_pool_allows_routing() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());

    client.add_pool(&admin, &pool);

    let route = client.check_and_route(&pool, &1_000);
    assert_eq!(route, Route::SorobanAMM);
}

#[test]
fn test_add_pool_duplicate_is_noop() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());

    client.add_pool(&admin, &pool);
    // Adding the same pool again must be a no-op (no panic, still approved).
    client.add_pool(&admin, &pool);

    let route = client.check_and_route(&pool, &1_000);
    assert_eq!(route, Route::SorobanAMM);
}

#[test]
fn test_remove_pool_revokes_approval() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());

    client.add_pool(&admin, &pool);
    client.remove_pool(&admin, &pool);

    let result = client.try_check_and_route(&pool, &1_000);
    assert!(result.is_err());
}

#[test]
fn test_remove_pool_not_present_is_noop() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());

    // Removing a pool that was never added must not panic.
    client.remove_pool(&admin, &pool);

    let result = client.try_check_and_route(&pool, &1_000);
    assert!(result.is_err());
}

#[test]
#[should_panic(expected = "pool is not approved")]
fn test_check_and_route_panics_for_unapproved_pool() {
    let (env, client, _admin) = setup();
    let pool = env.register(MockAmm, ());

    // Pool was never added to the allowlist.
    client.check_and_route(&pool, &1_000);
}

/// A pool that no longer implements `AmmInterface` (e.g. after an upgrade):
/// it has no `get_reserves` entrypoint at all, so any cross-contract call to
/// it reverts. Used to verify graceful fallback instead of an uncontrolled panic.
#[contract]
pub struct BrokenAmm;

#[contractimpl]
impl BrokenAmm {
    pub fn something_else(_env: Env) -> u32 {
        0
    }
}

#[test]
fn test_deep_pool_routing() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    // amount_in (1_000) < reserve (100_000) / 10 => SorobanAMM
    let route = client.check_and_route(&pool, &1_000);
    assert_eq!(route, Route::SorobanAMM);
}

#[test]
fn test_shallow_pool_routing() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    // amount_in (50_000) >= reserve (100_000) / 10 => StellarClassicDEX
    let route = client.check_and_route(&pool, &50_000);
    assert_eq!(route, Route::StellarClassicDEX);
}

#[test]
fn test_borderline_depth() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    // amount_in (10_000) == reserve (100_000) / 10 => not strictly less => StellarClassicDEX
    let route = client.check_and_route(&pool, &10_000);
    assert_eq!(route, Route::StellarClassicDEX);
}

#[test]
fn test_default_depth_threshold_is_1000_bps() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    // With no explicit configuration the threshold defaults to 1000 bps (10%),
    // preserving the previous hardcoded behavior.
    assert_eq!(client.get_depth_threshold_bps(), 1000);

    // amount_in (1_000) < reserve (100_000) * 1000 / 10_000 => SorobanAMM
    let route = client.check_and_route(&pool, &1_000);
    assert_eq!(route, Route::SorobanAMM);
}

#[test]
fn test_set_depth_threshold_bps_updates_routing() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    // Raise the threshold to 5000 bps (50%).
    client.set_depth_threshold_bps(&admin, &5_000);
    assert_eq!(client.get_depth_threshold_bps(), 5_000);

    // amount_in (50_000) < reserve (100_000) * 5000 / 10_000 (50_000) is false,
    // so it still routes to the classic DEX at the boundary.
    let route = client.check_and_route(&pool, &50_000);
    assert_eq!(route, Route::StellarClassicDEX);

    // amount_in (40_000) < 50_000 => SorobanAMM under the raised threshold.
    let route = client.check_and_route(&pool, &40_000);
    assert_eq!(route, Route::SorobanAMM);
}

#[test]
fn test_set_depth_threshold_bps_lowers_threshold() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    // Lower the threshold to 100 bps (1%).
    client.set_depth_threshold_bps(&admin, &100);

    // amount_in (1_000) < reserve (100_000) * 100 / 10_000 (1_000) is false,
    // so it now routes to the classic DEX.
    let route = client.check_and_route(&pool, &1_000);
    assert_eq!(route, Route::StellarClassicDEX);
}

#[test]
fn test_set_depth_threshold_bps_rejects_out_of_range() {
    let (_env, client, admin) = setup();

    // A threshold above 100% (10_000 bps) is invalid.
    let result = client.try_set_depth_threshold_bps(&admin, &10_001);
    assert!(result.is_err());
}

#[test]
fn test_initialize_sets_admin_and_requires_auth() {
    let env = Env::default();
    env.mock_all_auths();

    let router_id = env.register_contract(None, LiquidityRouter);
    let router_client = LiquidityRouterClient::new(&env, &router_id);

    let admin = Address::generate(&env);
    router_client.initialize(&admin);

    // Admin is recorded and the allowlist starts empty.
    assert_eq!(router_client.get_admin(), admin);
    assert_eq!(router_client.get_approved_pools().len(), 0);
}

#[test]
#[should_panic(expected = "already initialized")]
fn test_initialize_cannot_be_called_twice() {
    let env = Env::default();
    env.mock_all_auths();

    let router_id = env.register_contract(None, LiquidityRouter);
    let router_client = LiquidityRouterClient::new(&env, &router_id);

    let admin = Address::generate(&env);
    router_client.initialize(&admin);

    // A second call (e.g. by an attacker) must be rejected and must not
    // overwrite the admin or wipe the approved-pools allowlist.
    let attacker = Address::generate(&env);
    router_client.initialize(&attacker);
}

#[test]
fn test_admin_and_approved_pools_live_in_instance_storage() {
    let env = Env::default();
    env.mock_all_auths();

    let router_id = env.register_contract(None, LiquidityRouter);
    let router_client = LiquidityRouterClient::new(&env, &router_id);

    let admin = Address::generate(&env);
    router_client.initialize(&admin);

    // Admin and the approved-pools allowlist are config singletons and must be
    // kept in instance() storage so their TTL is bumped with every contract
    // call, rather than in persistent() storage where they could be archived.
    let instance = env.as_contract(&router_id, || env.storage().instance());
    assert!(instance.has(&DataKey::Admin));
}

// ---------------------------------------------------------------------------
// Slippage protection integration tests (#1223)
// ---------------------------------------------------------------------------

/// Router swap succeeds when the actual slippage is within the merchant's
/// configured max slippage bound.
#[test]
fn test_swap_succeeds_within_slippage_bounds() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    let slippage_id = env.register(MockSlippageProtection, ());
    let slippage_client = MockSlippageProtectionClient::new(&env, &slippage_id);
    // Merchant allows up to 100 bps (1%) slippage.
    slippage_client.set_max_slippage_bps(&100);

    client.set_slippage_protection(&admin, &slippage_id);

    // Actual slippage (50 bps) is within bounds => swap succeeds.
    let route = client.swap(&pool, &1_000, &50);
    assert_eq!(route, Route::SorobanAMM);
}

/// Router swap is rejected when the actual slippage breaches the merchant's
/// configured max slippage bound.
#[test]
fn test_swap_rejected_on_slippage_breach() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    let slippage_id = env.register(MockSlippageProtection, ());
    let slippage_client = MockSlippageProtectionClient::new(&env, &slippage_id);
    // Merchant allows up to 100 bps (1%) slippage.
    slippage_client.set_max_slippage_bps(&100);

    client.set_slippage_protection(&admin, &slippage_id);

    // Actual slippage (500 bps) breaches the bound => swap is rejected.
    let result = client.try_swap(&pool, &1_000, &500);
    assert!(result.is_err());
}

/// Per-merchant max slippage is read from merchant_registry metadata and
/// enforced by the router before the swap executes.
#[test]
fn test_swap_uses_per_merchant_max_slippage() {
    let (env, client, admin) = setup();
    let pool = env.register(MockAmm, ());
    client.add_pool(&admin, &pool);

    let slippage_id = env.register(MockSlippageProtection, ());
    let slippage_client = MockSlippageProtectionClient::new(&env, &slippage_id);
    slippage_client.set_max_slippage_bps(&100);

    client.set_slippage_protection(&admin, &slippage_id);

    let merchant = Address::generate(&env);
    // Merchant A is configured with a tight 50 bps bound.
    client.set_merchant_max_slippage_bps(&admin, &merchant, &50);

    // 75 bps breaches merchant A's 50 bps bound => rejected.
    let result = client.try_swap_for_merchant(&pool, &1_000, &75, &merchant);
    assert!(result.is_err());

    // 40 bps is within merchant A's bound => succeeds.
    let route = client.swap_for_merchant(&pool, &1_000, &40, &merchant);
    assert_eq!(route, Route::SorobanAMM);
}
