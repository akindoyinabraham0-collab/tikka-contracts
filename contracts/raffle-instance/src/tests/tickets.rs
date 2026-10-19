//! Ticket purchases, per-tx caps, pricing, pausing, and overflow guards.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
#[soroban_sdk::contracttype]
pub struct ReceiptMint {
    pub recipient: Address,
    pub ticket_id: u32,
    pub raffle_id: Address,
}

#[soroban_sdk::contracttype]
enum ReceiptKey {
    Calls,
    FailOn,
}

#[contract]
pub struct TicketReceiptMock;

#[contractimpl]
impl TicketReceiptMock {
    pub fn fail_on(env: Env, ticket_id: u32) {
        env.storage().instance().set(&ReceiptKey::FailOn, &ticket_id);
    }

    pub fn calls(env: Env) -> Vec<ReceiptMint> {
        env.storage().instance().get(&ReceiptKey::Calls).unwrap_or(Vec::new(&env))
    }

    pub fn mint(env: Env, recipient: Address, ticket_id: u32, raffle_id: Address) {
        raffle_id.require_auth();
        let fail_on: u32 = env.storage().instance().get(&ReceiptKey::FailOn).unwrap_or(0);
        assert_ne!(ticket_id, fail_on, "receipt mint rejected");
        let mut calls = Self::calls(env.clone());
        calls.push_back(ReceiptMint { recipient, ticket_id, raffle_id });
        env.storage().instance().set(&ReceiptKey::Calls, &calls);
    }
}

fn receipt_raffle(env: &Env, enabled: bool) -> (Address, Address, Address, Address, Address) {
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);
    let raffle_id = env.register(Contract, ());
    let factory = env.register(MockFactory, ());
    let nft_id = env.register(TicketReceiptMock, ());
    let creator = Address::generate(env);
    let buyer = Address::generate(env);
    let recipient = Address::generate(env);
    let (payment_token, token) = create_token(env, &Address::generate(env));
    token.mint(&creator, &1_000_000);
    token.mint(&buyer, &1_000_000);
    let mut config = base_config(env, &payment_token);
    config.max_tickets = 20;
    config.max_tickets_per_tx = raffle_shared::constants::MAX_NFT_TICKETS_PER_TX;
    config.prize_amount = MIN_TICKET_PRICE * 20;
    config.protocol_fee_bp = 100;
    config.treasury_address = Some(Address::generate(env));
    config.nft_contract = if enabled { Some(nft_id.clone()) } else { None };
    let client = ContractClient::new(env, &raffle_id);
    client.init(&factory, &Address::generate(env), &creator, &config);
    client.deposit_prize();
    (raffle_id, nft_id, buyer, recipient, payment_token)
}

#[test]
fn purchases_mint_receipts_for_each_ticket_and_emit_matching_events() {
    use soroban_sdk::{IntoVal, Map, Symbol, Val};

    let env = Env::default();
    let (raffle_id, nft_id, buyer, recipient, _) = receipt_raffle(&env, true);
    let client = ContractClient::new(&env, &raffle_id);
    client.buy_tickets(&buyer, &2);
    client.buy_tickets_for(&buyer, &recipient, &2);

    let calls = TicketReceiptMockClient::new(&env, &nft_id).calls();
    assert_eq!(calls.len(), 4);
    let mut receipts = 0;
    for (emitter, topics, data) in env.events().all().iter() {
        if emitter != raffle_id
            || topics != soroban_sdk::vec![&env, Symbol::new(&env, "ticket_nft_minted").into_val(&env)]
        {
            continue;
        }
        let ticket_id = receipts + 1;
        let owner = if ticket_id <= 2 { &buyer } else { &recipient };
        assert_eq!(calls.get(receipts).unwrap(), ReceiptMint {
            recipient: owner.clone(), ticket_id, raffle_id: raffle_id.clone(),
        });
        let expected: Map<Symbol, Val> = Map::from_array(&env, [
            (Symbol::new(&env, "recipient"), owner.into_val(&env)),
            (Symbol::new(&env, "ticket_id"), ticket_id.into_val(&env)),
            (Symbol::new(&env, "raffle_id"), raffle_id.into_val(&env)),
            (Symbol::new(&env, "nft_contract"), nft_id.into_val(&env)),
            (Symbol::new(&env, "timestamp"), 1_000u64.into_val(&env)),
        ]);
        assert_eq!(data, expected.into_val(&env));
        receipts += 1;
    }
    assert_eq!(receipts, 4);
}

#[test]
fn failing_second_receipt_rolls_back_payment_tickets_fees_mints_and_events() {
    for gift in [false, true] {
        let env = Env::default();
        let (raffle_id, nft_id, buyer, recipient, payment_token) = receipt_raffle(&env, true);
        let client = ContractClient::new(&env, &raffle_id);
        let nft = TicketReceiptMockClient::new(&env, &nft_id);
        nft.fail_on(&2);
        let token = soroban_sdk::token::Client::new(&env, &payment_token);
        let buyer_balance = token.balance(&buyer);
        let raffle_balance = token.balance(&raffle_id);
        let events_before = env.events().all();
        if gift {
            assert!(client.try_buy_tickets_for(&buyer, &recipient, &2).is_err());
        } else {
            assert!(client.try_buy_tickets(&buyer, &2).is_err());
        }
        assert_eq!(client.get_raffle().tickets_sold, 0);
        assert_eq!(token.balance(&buyer), buyer_balance);
        assert_eq!(token.balance(&raffle_id), raffle_balance);
        assert_eq!(nft.calls().len(), 0);
        assert_eq!(env.events().all(), events_before);
        env.as_contract(&raffle_id, || {
            assert!(!env.storage().persistent().has(&DataKey::Ticket(1)));
            assert!(!env.storage().persistent().has(&DataKey::Ticket(2)));
            assert!(!env.storage().persistent().has(&DataKey::TicketCount(buyer.clone())));
            assert!(!env.storage().persistent().has(&DataKey::TicketCount(recipient.clone())));
            assert!(!env.storage().persistent().has(&DataKey::TicketBuyers));
            assert_eq!(env.storage().instance().get::<_, i128>(&DataKey::AccumulatedFees).unwrap_or(0), 0);
        });
        // The guard and ticket sequence also recover after an aborted purchase.
        nft.fail_on(&0);
        assert_eq!(client.buy_tickets(&buyer, &1), 1);
    }
}

#[test]
fn no_nft_configured_does_not_call_receipt_contract() {
    let env = Env::default();
    let (raffle_id, nft_id, buyer, recipient, _) = receipt_raffle(&env, false);
    let nft = TicketReceiptMockClient::new(&env, &nft_id);
    nft.fail_on(&1);
    let client = ContractClient::new(&env, &raffle_id);
    assert_eq!(client.buy_tickets(&buyer, &1), 1);
    assert_eq!(client.buy_tickets_for(&buyer, &recipient, &1), 2);
    assert_eq!(nft.calls().len(), 0);
}

#[test]
fn nft_batch_at_cap_fits_default_budget_and_larger_batches_are_rejected() {
    for gift in [false, true] {
        let env = Env::default();
        let (raffle_id, nft_id, buyer, recipient, _) = receipt_raffle(&env, true);
        let client = ContractClient::new(&env, &raffle_id);
        let cap = raffle_shared::constants::MAX_NFT_TICKETS_PER_TX;
        // Also exercise the runtime cap for previously initialized instances
        // whose stored per-transaction limit predates NFT minting support.
        env.as_contract(&raffle_id, || {
            let mut raffle = crate::read_raffle(&env).unwrap();
            raffle.max_tickets_per_tx = cap + 1;
            crate::write_raffle(&env, &raffle);
        });
        if gift {
            assert_eq!(client.try_buy_tickets_for(&buyer, &recipient, &(cap + 1)), Err(Ok(Error::ExceedsMaxTicketsPerTx)));
        } else {
            assert_eq!(client.try_buy_tickets(&buyer, &(cap + 1)), Err(Ok(Error::ExceedsMaxTicketsPerTx)));
        }
        env.cost_estimate().budget().reset_default();
        if gift {
            assert_eq!(client.buy_tickets_for(&buyer, &recipient, &cap), cap);
        } else {
            assert_eq!(client.buy_tickets(&buyer, &cap), cap);
        }
        let budget = env.cost_estimate().budget();
        assert!(budget.cpu_instruction_cost() < 100_000_000);
        assert!(budget.memory_bytes_cost() < 40 * 1024 * 1024);
        assert_eq!(TicketReceiptMockClient::new(&env, &nft_id).calls().len(), cap);
    }
}

#[test]
fn ticket_number_matches_monotonic_id() {
    let env = Env::default();
    let owner = Address::generate(&env);

    let ticket = Ticket::new(7, owner, 123, MIN_TICKET_PRICE);

    assert_eq!(ticket.id, 7);
    assert_eq!(ticket.ticket_number, 7);
    assert_eq!(ticket.ticket_number, ticket.id);
}

#[test]
fn buy_tickets_rejects_quantity_above_per_tx_cap() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let contract_id = env.register(RaffleInstance, ());
    let client = RaffleInstanceClient::new(&env, &contract_id);

    let factory = env.register(MockFactory, ());
    let admin = Address::generate(&env);
    let creator = Address::generate(&env);
    let buyer = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let (token_addr, token_mint) = create_token(&env, &token_admin);
    token_mint.mint(&creator, &1_000_000);
    token_mint.mint(&buyer, &1_000_000);

    let config = RaffleConfig {
        description: String::from_str(&env, "Per-tx cap"),
        end_time: 0,
        no_deadline: true,
        max_tickets: 100,
        max_tickets_per_tx: 5,
        min_tickets: 1,
        allow_multiple: true,
        ticket_price: MIN_TICKET_PRICE,
        payment_token: token_addr.clone(),
        prize_amount: MIN_TICKET_PRICE * 100,
        prizes: vec![&env, 10000u32],
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 0,
        treasury_address: None,
        swap_router: None,
        tikka_token: None,
        unique_winners: false,
            metadata_hash: BytesN::from_array(&env, &[5u8; 32]),
        claim_lockup_seconds: None,
        swap_deadline_seconds: None,
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        max_tickets_per_address: 0,
        claim_expiry_seconds: None,
        category: None,
        prize_token: None,
        nft_contract: None,

            bundles: soroban_sdk::Vec::new(&env),
    };

    client.init(&factory, &admin, &creator, &config);
    client.deposit_prize();

    assert_eq!(
        client.try_buy_tickets(&buyer, &6),
        Err(Ok(Error::ExceedsMaxTicketsPerTx))
    );
    assert_eq!(env.events().all().len(), 0);
    assert_eq!(client.buy_tickets(&buyer, &5), 5);
}

#[test]
fn buy_tickets_rejects_overflowing_total_price_without_wrapping() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let contract_id = env.register(RaffleInstance, ());
    let client = RaffleInstanceClient::new(&env, &contract_id);

    let factory = env.register(MockFactory, ());
    let admin = Address::generate(&env);
    let creator = Address::generate(&env);
    let buyer = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let (token_addr, token_mint) = create_token(&env, &token_admin);
    token_mint.mint(&creator, &1_000_000_000_000);
    token_mint.mint(&buyer, &1_000_000_000_000);

    let quantity = 100_000u32;
    let config = RaffleConfig {
        description: String::from_str(&env, "overflow guard"),
        end_time: 0,
        no_deadline: true,
        max_tickets: quantity,
        max_tickets_per_tx: quantity,
        min_tickets: 1,
        allow_multiple: true,
        ticket_price: MIN_TICKET_PRICE,
        payment_token: token_addr.clone(),
        prize_amount: MIN_TICKET_PRICE * 10,
        prizes: vec![&env, 10000u32],
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 0,
        treasury_address: None,
        swap_router: None,
        tikka_token: None,
        metadata_hash: BytesN::from_array(&env, &[77u8; 32]),
        claim_lockup_seconds: None,
        swap_deadline_seconds: None,
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        max_tickets_per_address: 0,
        claim_expiry_seconds: None,
        category: None,
        unique_winners: false,
        prize_token: None,
        nft_contract: None,

            bundles: soroban_sdk::Vec::new(&env),
    };

    client.init(&factory, &admin, &creator, &config);
    client.deposit_prize();

    let overflow_price = (i128::MAX / quantity as i128) + 1;
    env.as_contract(&contract_id, || {
        let mut raffle = crate::read_raffle(&env).unwrap();
        raffle.ticket_price = overflow_price;
        raffle.prize_amount = overflow_price;
        crate::write_raffle(&env, &raffle);
    });

    assert_eq!(
        client.try_buy_tickets(&buyer, &quantity),
        Err(Ok(Error::ArithmeticOverflow))
    );

    let raffle = client.get_raffle();
    assert_eq!(raffle.tickets_sold, 0);
}

fn setup_scale_raffle(
    env: &Env,
    max_tickets: u32,
    max_tickets_per_tx: u32,
    prize_amount: i128,
) -> (
    RaffleInstanceClient<'_>,
    Address,
    Address,
    Address,
    Address,
    token::StellarAssetClient<'_>,
) {
    let contract_id = env.register(RaffleInstance, ());
    let client = RaffleInstanceClient::new(env, &contract_id);
    let factory = env.register(MockFactory, ());
    let admin = Address::generate(env);
    let creator = Address::generate(env);
    let buyer = Address::generate(env);

    let token_admin = Address::generate(env);
    let (payment_token, token_mint) = create_token(env, &token_admin);
    token_mint.mint(&creator, &(prize_amount * 2));
    token_mint.mint(&buyer, &(prize_amount * 2));

    let config = RaffleConfig {
        description: String::from_str(env, "scale benchmark"),
        end_time: 0,
        no_deadline: true,
        max_tickets,
        max_tickets_per_tx,
        min_tickets: 1,
        allow_multiple: true,
        ticket_price: MIN_TICKET_PRICE,
        payment_token: payment_token.clone(),
        prize_amount,
        prizes: vec![env, 10000u32],
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 0,
        treasury_address: None,
        swap_router: None,
        tikka_token: None,
        metadata_hash: BytesN::from_array(env, &[88u8; 32]),
        claim_lockup_seconds: None,
        swap_deadline_seconds: None,
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        max_tickets_per_address: 0,
        claim_expiry_seconds: None,
        category: None,
        unique_winners: false,
        prize_token: None,
        nft_contract: None,

            bundles: soroban_sdk::Vec::new(env),
    };

    client.init(&factory, &admin, &creator, &config);
    client.deposit_prize();

    (
        client,
        contract_id,
        creator,
        buyer,
        payment_token,
        token_mint,
    )
}

fn record_costs<F: FnOnce()>(env: &Env, f: F) -> (u64, u64) {
    env.cost_estimate().budget().reset_default();
    f();
    let budget = env.cost_estimate().budget();
    (budget.cpu_instruction_cost(), budget.memory_bytes_cost())
}

const BUY_TICKETS_1K_CPU_CEILING: u64 = 30_000_000;
const BUY_TICKETS_1K_MEM_CEILING: u64 = 10 * 1024 * 1024;

#[test]
fn buy_tickets_cost_stays_below_ceiling_for_1k_batch() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let (client, _contract_id, buyer, _, _, _) =
        setup_scale_raffle(&env, 1_000, 1_000, MIN_TICKET_PRICE * 10);

    let (cpu, mem) = record_costs(&env, || {
        client.buy_tickets(&buyer, &1_000);
    });

    assert!(
        cpu < BUY_TICKETS_1K_CPU_CEILING,
        "buy_tickets 1k CPU {cpu} exceeded ceiling {BUY_TICKETS_1K_CPU_CEILING}"
    );
    assert!(
        mem < BUY_TICKETS_1K_MEM_CEILING,
        "buy_tickets 1k memory {mem} exceeded ceiling {BUY_TICKETS_1K_MEM_CEILING}"
    );
}

#[test]
fn pause_resume_ticket_sales_controls_buy_tickets() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let (client, _admin, _creator, buyer, _factory, _token_mint) = setup_active_raffle(&env);

    assert_eq!(client.get_raffle().status, RaffleStatus::Active);
    assert!(!client.is_ticket_sales_paused());

    client.pause_ticket_sales(&_admin);
    assert!(client.is_ticket_sales_paused());
    assert_eq!(
        client.try_buy_tickets(&buyer, &1),
        Err(Ok(Error::ContractPaused))
    );

    client.resume_ticket_sales(&_admin);
    assert!(!client.is_ticket_sales_paused());
    assert_eq!(client.get_raffle().status, RaffleStatus::Active);
    assert_eq!(client.buy_tickets(&buyer, &1), 1);
}

#[test]
fn admin_can_pause_and_resume_ticket_sales() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let (client, admin, _creator, buyer, _factory, _token_mint) = setup_active_raffle(&env);

    client.pause_ticket_sales(&admin);
    assert!(client.is_ticket_sales_paused());
    assert_eq!(
        client.try_buy_tickets(&buyer, &1),
        Err(Ok(Error::ContractPaused))
    );
    assert_eq!(env.events().all().len(), 0);

    client.resume_ticket_sales(&admin);
    assert!(!client.is_ticket_sales_paused());
    assert_eq!(client.buy_tickets(&buyer, &1), 1);
}

#[test]
fn test_bundle_pricing_applies() {
    let env = Env::default();
    env.mock_all_auths();

    let factory = env.register(MockFactory, ());
    let admin = Address::generate(&env);
    let creator = Address::generate(&env);
    let buyer = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let payment_token = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let token_client = StellarAssetClient::new(&env, &payment_token);
    token_client.mint(&creator, &100_000_000);
    token_client.mint(&buyer, &100_000_000);

    let contract_id = env.register(RaffleInstance, ());
    let client = RaffleInstanceClient::new(&env, &contract_id);

    let config = RaffleConfig {
        description: String::from_str(&env, "Bundle test"),
        end_time: 0,
        no_deadline: true,
        max_tickets: 50,
        max_tickets_per_tx: 50,
        min_tickets: 1,
        allow_multiple: true,
        ticket_price: 100_000,
        payment_token: payment_token.clone(),
        prize_amount: 100_000 * 50,
        prizes: soroban_sdk::vec![&env, 10000],
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 0,
        treasury_address: None,
        swap_router: None,
        tikka_token: None,
        unique_winners: false,
            metadata_hash: BytesN::from_array(&env, &[9; 32]),
        claim_lockup_seconds: None,
        swap_deadline_seconds: None,
        bundles: soroban_sdk::vec![
            &env,
            raffle_shared::TicketBundle {
                quantity: 5,
                price_per_ticket: 90_000
            },
            raffle_shared::TicketBundle {
                quantity: 10,
                price_per_ticket: 80_000
            },
            raffle_shared::TicketBundle {
                quantity: 20,
                price_per_ticket: 70_000
            },
        ],
        max_tickets_per_address: 0,
        claim_expiry_seconds: None,
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        category: None,
        prize_token: None,
        nft_contract: None,

    };

    client.init(&factory, &admin, &creator, &config);

    client.deposit_prize();

    let balance_before = token_client.balance(&buyer);
    client.buy_tickets(&buyer, &11);
    let balance_after = token_client.balance(&buyer);

    assert_eq!(balance_before - balance_after, 11 * 80_000);
}

// ===========================================================================
// Full lifecycle integration test (#440)
//
// Walks a complete, successful raffle from init through prize claim, asserting
// token-balance and status transitions at each step, plus the core value
// invariant `total_distributed == prize_amount - prize_claim_fees` (which is
// `prize_amount` in this build, where the prize-claim fee is 0).
// ===========================================================================

#[test]
fn test_adversarial_ceiling_rounding() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let factory = env.register(MockFactory, ());
    let admin = Address::generate(&env);
    let creator = Address::generate(&env);
    let buyer = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let payment_token = env.register_stellar_asset_contract_v2(token_admin.clone()).address();
    let token_client = StellarAssetClient::new(&env, &payment_token);
    
    // Price = 10,001. Fee = 1 bp (0.01%)
    // floor(10,001 * 1 / 10,000) = 1 (truncating — favours the payer)
    let ticket_price = 10_001i128;
    token_client.mint(&creator, &1_000_000);
    token_client.mint(&buyer, &1_000_000);

    let contract_id = env.register(RaffleInstance, ());
    let client = RaffleInstanceClient::new(&env, &contract_id);

    let config = RaffleConfig {
        description: String::from_str(&env, "Adversarial Rounding"),
        end_time: 0,
        no_deadline: true,
        max_tickets: 1,
        max_tickets_per_tx: 1,
        min_tickets: 1,
        allow_multiple: true,
        ticket_price: ticket_price,
        payment_token: payment_token.clone(),
        prize_amount: ticket_price, // 10,001
        prizes: soroban_sdk::vec![&env, 10000], // 100%
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 1, // 1 basis point
        treasury_address: Some(Address::generate(&env)),
        swap_router: None,
        tikka_token: None,
        metadata_hash: BytesN::from_array(&env, &[9; 32]),
        claim_lockup_seconds: None,
        swap_deadline_seconds: None,
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        max_tickets_per_address: 0,
        claim_expiry_seconds: None,
        category: None,
        unique_winners: false,
        prize_token: None,
        nft_contract: None,

            bundles: soroban_sdk::Vec::new(&env),
    };

    client.init(&factory, &admin, &creator, &config);
    client.deposit_prize();
    client.buy_tickets(&buyer, &1);
    
    let expected_ticket_fee = 1i128; // floor(10_001 * 1 / 10_000) = 1 (truncating)
    assert_eq!(client.get_accumulated_fees(), expected_ticket_fee);
    
    client.finalize_raffle();
    let winner = client.get_raffle().winners.get(0).unwrap().address;
    let balance_before = token_client.balance(&winner);
    
    let claimed = client.claim_prize(&winner, &0);
    assert_eq!(claimed, ticket_price); // Gross amount
    
    let expected_prize_fee = 1i128; // floor(10_001 * 1 / 10_000) = 1 (truncating)
    assert_eq!(client.get_accumulated_fees(), expected_ticket_fee + expected_prize_fee);
    
    let balance_after = token_client.balance(&winner);
    assert_eq!(balance_after, balance_before + ticket_price - expected_prize_fee);
}

struct TicketSetup<'a> {
    client: ContractClient<'a>,
    buyer: Address,
    recipient: Address,
    token: token::StellarAssetClient<'a>,
}

fn setup_per_address_cap(
    env: &Env,
    cap: u32,
    allow_multiple: bool,
    max_tickets: u32,
) -> TicketSetup<'_> {
    let contract_id = env.register(RaffleInstance, ());
    let client = RaffleInstanceClient::new(env, &contract_id);
    let factory = env.register(MockFactory, ());
    let admin = Address::generate(env);
    let creator = Address::generate(env);
    let buyer = Address::generate(env);
    let recipient = Address::generate(env);
    let token_admin = Address::generate(env);
    let (payment_token, token) = create_token(env, &token_admin);

    token.mint(&creator, &1_000_000_000);
    token.mint(&buyer, &1_000_000_000);
    let config = RaffleConfig {
        description: String::from_str(env, "ticket cap"),
        end_time: 0,
        no_deadline: true,
        max_tickets,
        max_tickets_per_tx: max_tickets,
        max_tickets_per_address: cap,
        min_tickets: 1,
        allow_multiple,
        ticket_price: MIN_TICKET_PRICE,
        payment_token,
        prize_amount: MIN_TICKET_PRICE * max_tickets as i128,
        prizes: vec![env, 10_000u32],
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 0,
        treasury_address: None,
        swap_router: None,
        tikka_token: None,
        metadata_hash: BytesN::from_array(env, &[1; 32]),
        claim_lockup_seconds: Some(0),
        swap_deadline_seconds: Some(0),
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        category: None,
        unique_winners: false,
        bundles: Vec::new(env),
        prize_token: None,
        nft_contract: None,
        claim_expiry_seconds: None,

    };

    client.init(&factory, &admin, &creator, &config);
    client.deposit_prize();

    TicketSetup {
        client,
        buyer,
        recipient,
        token,
    }
}

#[test]
fn buying_exactly_the_cap_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_per_address_cap(&env, 5, true, 10);

    assert_eq!(setup.client.buy_tickets(&setup.buyer, &5), 5);
    assert_eq!(setup.client.get_remaining_ticket_allowance(&setup.buyer), 0);
}

#[test]
fn buying_beyond_the_cap_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_per_address_cap(&env, 5, true, 10);

    setup.client.buy_tickets(&setup.buyer, &5);
    assert_eq!(
        setup.client.try_buy_tickets(&setup.buyer, &1),
        Err(Ok(Error::ExceedsMaxTicketsPerAddress))
    );
}

#[test]
fn cap_is_enforced_across_transactions() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_per_address_cap(&env, 5, true, 10);

    setup.client.buy_tickets(&setup.buyer, &3);
    assert_eq!(
        setup.client.try_buy_tickets(&setup.buyer, &3),
        Err(Ok(Error::ExceedsMaxTicketsPerAddress))
    );
    assert_eq!(setup.client.get_remaining_ticket_allowance(&setup.buyer), 2);
}

#[test]
fn zero_cap_is_unlimited_up_to_raffle_capacity() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_per_address_cap(&env, 0, true, 10);

    setup.client.buy_tickets(&setup.buyer, &5);
    assert_eq!(setup.client.buy_tickets(&setup.buyer, &5), 10);
    assert_eq!(setup.client.get_remaining_ticket_allowance(&setup.buyer), 0);
}

#[test]
fn configured_cap_supersedes_allow_multiple() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_per_address_cap(&env, 3, false, 10);

    assert_eq!(setup.client.buy_tickets(&setup.buyer, &2), 2);
    assert_eq!(setup.client.get_remaining_ticket_allowance(&setup.buyer), 1);
}

#[test]
fn gifted_tickets_count_against_recipient_cap() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_per_address_cap(&env, 2, true, 10);

    setup
        .client
        .buy_tickets_for(&setup.buyer, &setup.recipient, &2);
    assert_eq!(
        setup
            .client
            .get_remaining_ticket_allowance(&setup.recipient),
        0
    );
    assert_eq!(setup.client.get_remaining_ticket_allowance(&setup.buyer), 2);
    assert_eq!(
        setup
            .client
            .try_buy_tickets_for(&setup.buyer, &setup.recipient, &1),
        Err(Ok(Error::ExceedsMaxTicketsPerAddress))
    );
}

#[test]
fn gifted_tickets_charge_buyer_and_assign_owner_to_recipient() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_per_address_cap(&env, 10, true, 20);

    let buyer_balance_before = setup.token.balance(&setup.buyer);
    let recipient_balance_before = setup.token.balance(&setup.recipient);

    let sold = setup
        .client
        .buy_tickets_for(&setup.buyer, &setup.recipient, &3);
    assert_eq!(sold, 3);

    let buyer_balance_after = setup.token.balance(&setup.buyer);
    let recipient_balance_after = setup.token.balance(&setup.recipient);

    assert_eq!(
        buyer_balance_before - buyer_balance_after,
        3 * MIN_TICKET_PRICE
    );
    assert_eq!(recipient_balance_after, recipient_balance_before);

    let contract_id = setup.client.address.clone();
    for ticket_id in 1..=3 {
        let ticket: Ticket = env.as_contract(&contract_id, || {
            env.storage()
                .persistent()
                .get(&DataKey::Ticket(ticket_id))
                .unwrap()
        });
        assert_eq!(ticket.owner, setup.recipient);
        assert_eq!(ticket.payer, setup.buyer);
    }
}

#[test]
fn cap_cannot_exceed_max_tickets() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(RaffleInstance, ());
    let client = RaffleInstanceClient::new(&env, &contract_id);
    let factory = env.register(MockFactory, ());
    let admin = Address::generate(&env);
    let creator = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let (payment_token, _token) = create_token(&env, &token_admin);
    let config = RaffleConfig {
        description: String::from_str(&env, "invalid cap"),
        end_time: 0,
        no_deadline: true,
        max_tickets: 2,
        max_tickets_per_tx: 2,
        max_tickets_per_address: 3,
        min_tickets: 1,
        allow_multiple: true,
        ticket_price: MIN_TICKET_PRICE,
        payment_token,
        prize_amount: MIN_TICKET_PRICE * 2,
        prizes: vec![&env, 10_000u32],
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 0,
        treasury_address: None,
        swap_router: None,
        tikka_token: None,
        metadata_hash: BytesN::from_array(&env, &[1; 32]),
        claim_lockup_seconds: Some(0),
        swap_deadline_seconds: Some(0),
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        category: None,
        unique_winners: false,
        bundles: Vec::new(&env),
        prize_token: None,
        nft_contract: None,
        claim_expiry_seconds: None,

    };

    assert_eq!(
        client.try_init(&factory, &admin, &creator, &config),
        Err(Ok(Error::InvalidParameters))
    );
}

#[test]
fn init_rejects_non_ascending_bundle_quantities() {
    // build RaffleConfig with bundles qty 5 then 3 → init Err(InvalidParameters)
}

#[test]
fn calculate_buy_quote_uses_best_bundle() {
    // raffle with list 100, bundle qty 5 price 80
    // quantity 5 → unit 80; quantity 4 → unit 100
}

#[test]
fn early_bird_applies_after_bundle_unit_price() {
    // document numeric precedence in asserts
}

struct DeadlineSetup<'a> {
    client: ContractClient<'a>,
    buyer: Address,
    recipient: Address,
}

/// Like `setup()`, but with a real deadline (`no_deadline: false`) instead
/// of the disabled deadline the ticket-cap tests use. `min_tickets: 2` so
/// `finalize_raffle` cannot succeed via the tickets-full path with a single
/// purchase — these tests exercise the deadline path only, with zero
/// tickets sold.
fn setup_with_deadline(env: &Env, end_time: u64) -> DeadlineSetup<'_> {
    env.ledger().set_timestamp(1); // baseline "now" so init's end_time > now check passes
    let contract_id = env.register(Contract, ());
    let client = ContractClient::new(env, &contract_id);
    let factory = env.register(MockFactory, ());
    let admin = Address::generate(env);
    let creator = Address::generate(env);
    let buyer = Address::generate(env);
    let recipient = Address::generate(env);
    let token_admin = Address::generate(env);
    let (payment_token, token) = create_token(env, &token_admin);

    token.mint(&creator, &1_000_000_000);
    token.mint(&buyer, &1_000_000_000);

    let config = RaffleConfig {
        description: String::from_str(env, "deadline boundary"),
        end_time,
        no_deadline: false,
        max_tickets: 10,
        max_tickets_per_tx: 10,
        max_tickets_per_address: 0,
        min_tickets: 2,
        allow_multiple: true,
        ticket_price: MIN_TICKET_PRICE,
        payment_token,
        prize_amount: MIN_TICKET_PRICE * 10,
        prizes: vec![env, 10_000u32],
        randomness_source: RandomnessSource::Internal,
        oracle_address: None,
        oracle_public_key: None,
        protocol_fee_bp: 0,
        treasury_address: None,
        swap_router: None,
        tikka_token: None,
        metadata_hash: BytesN::from_array(env, &[1; 32]),
        claim_lockup_seconds: Some(0),
        swap_deadline_seconds: Some(0),
        early_bird_ticket_percentage: 0,
        early_bird_discount_bp: 0,
        category: None,
        unique_winners: false,
        bundles: Vec::new(env),
        prize_token: None,
        nft_contract: None,
        claim_expiry_seconds: None,

    };

    client.init(&factory, &admin, &creator, &config);
    client.deposit_prize();

    DeadlineSetup { client, buyer, recipient }
}

// --- buy_tickets: end_time is an exclusive boundary ---

#[test]
fn buy_tickets_succeeds_just_before_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(999);
    assert_eq!(setup.client.buy_tickets(&setup.buyer, &1), 1);
}

#[test]
fn buy_tickets_rejected_exactly_at_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_000);
    assert_eq!(
        setup.client.try_buy_tickets(&setup.buyer, &1),
        Err(Ok(Error::RaffleExpired))
    );
}

#[test]
fn buy_tickets_rejected_after_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_001);
    assert_eq!(
        setup.client.try_buy_tickets(&setup.buyer, &1),
        Err(Ok(Error::RaffleExpired))
    );
}

// --- buy_tickets_for: must agree with buy_tickets at the same boundary.
// buy_tickets_for_rejected_exactly_at_end_time is the regression test for
// the end_time inconsistency bug — before the fix (`>` instead of `>=`),
// this purchase succeeded instead of returning RaffleExpired. ---

#[test]
fn buy_tickets_for_succeeds_just_before_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(999);
    assert_eq!(
        setup.client.buy_tickets_for(&setup.buyer, &setup.recipient, &1),
        1
    );
}

#[test]
fn buy_tickets_for_rejected_exactly_at_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_000);
    assert_eq!(
        setup.client.try_buy_tickets_for(&setup.buyer, &setup.recipient, &1),
        Err(Ok(Error::RaffleExpired))
    );
}

#[test]
fn buy_tickets_for_rejected_after_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_001);
    assert_eq!(
        setup.client.try_buy_tickets_for(&setup.buyer, &setup.recipient, &1),
        Err(Ok(Error::RaffleExpired))
    );
}

// --- finalize_raffle: deadline gate must open at the same instant ---

#[test]
fn finalize_raffle_rejected_before_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(999);
    assert_eq!(
        setup.client.try_finalize_raffle(),
        Err(Ok(Error::InvalidStateTransition))
    );
}

#[test]
fn finalize_raffle_allowed_exactly_at_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_000);
    setup.client.finalize_raffle();
    // 0 tickets sold < min_tickets (2) -> ZeroTicketsSold failure path.
    assert_eq!(setup.client.get_raffle().status, RaffleStatus::Failed);
}

#[test]
fn finalize_raffle_allowed_after_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_001);
    setup.client.finalize_raffle();
    assert_eq!(setup.client.get_raffle().status, RaffleStatus::Failed);
}

// --- get_stats().time_remaining: 0 at the first rejecting instant ---

#[test]
fn time_remaining_is_one_just_before_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(999);
    assert_eq!(setup.client.get_stats().time_remaining, 1);
}

#[test]
fn time_remaining_is_zero_exactly_at_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_000);
    assert_eq!(setup.client.get_stats().time_remaining, 0);
}

#[test]
fn time_remaining_is_zero_after_end_time() {
    let env = Env::default();
    env.mock_all_auths();
    let setup = setup_with_deadline(&env, 1_000);
    env.ledger().set_timestamp(1_001);
    assert_eq!(setup.client.get_stats().time_remaining, 0);
}
