use super::*;

#[test]
fn invalid_categories_reject_before_deployment_or_indexing() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, _treasury) = setup_factory(&env);
    let creator = Address::generate(&env);
    let token = make_token(&env);
    let overlong = String::from_bytes(
        &env,
        &[b'a'; raffle_shared::constants::MAX_CATEGORY_LENGTH as usize + 1],
    );
    for category in [
        overlong,
        String::from_str(&env, ""),
        String::from_str(&env, "game night"),
        String::from_str(&env, "game_night"),
        String::from_str(&env, "caf\u{00e9}"),
    ] {
        let mut config = rate_limit_config(&env, &token, "invalid category");
        config.category = Some(category.clone());
        assert_eq!(
            client.try_create_raffle(&creator, &config),
            Err(Ok(ContractError::InvalidParameters))
        );
        env.as_contract(&client.address, || {
            assert!(!env.storage().persistent().has(&DataKey::RaffleInstancesCount));
            assert!(!env.storage().persistent().has(&DataKey::NextRaffleId));
            assert!(!env.storage().persistent().has(&DataKey::CreatorRaffles(creator.clone())));
            assert!(!env.storage().persistent().has(&DataKey::CategoryRaffles(category.clone())));
        });
        assert_eq!(client.get_profile(&creator).raffles_created, 0);
    }
}

#[test]
fn mixed_case_categories_share_the_lowercase_index_and_instance_value() {
    let env = Env::default();
    env.mock_all_auths();
    let (client, _admin, _treasury) = setup_factory(&env);
    let creator = Address::generate(&env);
    let token = make_token(&env);
    let mixed_case = String::from_str(&env, "GaMiNg-2026");
    let canonical = String::from_str(&env, "gaming-2026");
    let mut first_config = rate_limit_config(&env, &token, "first category");
    first_config.category = Some(mixed_case.clone());
    let first = client.create_raffle(&creator, &first_config);
    let mut second_config = rate_limit_config(&env, &token, "second category");
    second_config.category = Some(canonical.clone());
    let second = client.create_raffle(&creator, &second_config);

    let page = client.get_raffles_by_category(
        &canonical,
        &PaginationParams { limit: 10, offset: 0 },
    );
    assert_eq!(page.total, 2);
    assert_eq!(page.items.get(0), Some(first.clone()));
    assert_eq!(page.items.get(1), Some(second.clone()));
    for address in [first, second] {
        let raffle = raffle_instance::RaffleInstanceClient::new(&env, &address).get_raffle();
        assert_eq!(raffle.category, Some(canonical.clone()));
    }
    env.as_contract(&client.address, || {
        assert!(!env.storage().persistent().has(&DataKey::CategoryRaffles(mixed_case)));
    });
}
