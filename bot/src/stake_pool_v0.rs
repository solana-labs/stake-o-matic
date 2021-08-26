use {
    crate::{
        generic_stake_pool::*,
        rpc_client_utils::{get_all_stake, send_and_confirm_transactions},
    },
    log::*,
    solana_client::{rpc_client::RpcClient, rpc_response::StakeActivationState},
    solana_sdk::{
        account::Account,
        native_token::{Sol, LAMPORTS_PER_SOL},
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        stake::{self, instruction as stake_instruction, state::StakeState},
        transaction::Transaction,
    },
    std::{
        collections::{HashMap, HashSet},
        error,
    },
};

// Minimum amount of lamports in a stake pool account
pub const MIN_STAKE_ACCOUNT_BALANCE: u64 = LAMPORTS_PER_SOL;

// Don't bother adjusting stake if less than this amount of lamports will be affected
// (must be >= MIN_STAKE_ACCOUNT_BALANCE)
const MIN_STAKE_CHANGE_AMOUNT: u64 = MIN_STAKE_ACCOUNT_BALANCE;

#[derive(Debug)]
pub struct StakePool {
    authorized_staker: Keypair,
    baseline_stake_amount: u64,
    reserve_stake_address: Pubkey,
    min_reserve_stake_balance: u64,
}

pub fn new(
    _rpc_client: &RpcClient,
    authorized_staker: Keypair,
    baseline_stake_amount: u64,
    reserve_stake_address: Pubkey,
    min_reserve_stake_balance: u64,
) -> Result<StakePool, Box<dyn error::Error>> {
    if baseline_stake_amount < MIN_STAKE_CHANGE_AMOUNT {
        return Err(format!(
            "baseline stake amount too small: {}",
            Sol(baseline_stake_amount)
        )
        .into());
    }

    if min_reserve_stake_balance < MIN_STAKE_ACCOUNT_BALANCE {
        return Err(format!(
            "minimum reserve stake balance is too small: {}",
            Sol(min_reserve_stake_balance)
        )
        .into());
    }

    Ok(StakePool {
        authorized_staker,
        baseline_stake_amount,
        reserve_stake_address,
        min_reserve_stake_balance,
    })
}

fn validator_stake_address_seed(vote_address: Pubkey) -> String {
    format!("S{}", vote_address)[..32].to_string()
}

fn validator_transient_stake_address_seed(vote_address: Pubkey) -> String {
    format!("T{}", vote_address)[..32].to_string()
}

fn validator_stake_address(authorized_staker: Pubkey, vote_address: Pubkey) -> Pubkey {
    Pubkey::create_with_seed(
        &authorized_staker,
        &validator_stake_address_seed(vote_address),
        &stake::program::id(),
    )
    .unwrap()
}

fn validator_transient_stake_address(authorized_staker: Pubkey, vote_address: Pubkey) -> Pubkey {
    Pubkey::create_with_seed(
        &authorized_staker,
        &validator_transient_stake_address_seed(vote_address),
        &stake::program::id(),
    )
    .unwrap()
}

impl GenericStakePool for StakePool {
    fn apply(
        &mut self,
        rpc_client: &RpcClient,
        dry_run: bool,
        desired_validator_stake: &[ValidatorStake],
    ) -> Result<
        (
            EpochStakeNotes,
            ValidatorStakeActions,
            UnfundedValidators,
            u64,
        ),
        Box<dyn error::Error>,
    > {
        let mut validator_stake_actions = HashMap::default();

        let mut inuse_stake_addresses = HashSet::default();
        inuse_stake_addresses.insert(self.reserve_stake_address);

        let mut min_stake_node_count = 0;
        let mut bonus_stake_node_count = 0;
        let mut baseline_stake_node_count = 0;

        for ValidatorStake {
            vote_address,
            stake_state,
            ..
        } in desired_validator_stake
        {
            let stake_address =
                validator_stake_address(self.authorized_staker.pubkey(), *vote_address);
            let transient_stake_address =
                validator_transient_stake_address(self.authorized_staker.pubkey(), *vote_address);

            inuse_stake_addresses.insert(stake_address);
            inuse_stake_addresses.insert(transient_stake_address);

            match stake_state {
                ValidatorStakeState::None => min_stake_node_count += 1,
                ValidatorStakeState::Bonus => bonus_stake_node_count += 1,
                ValidatorStakeState::Baseline => baseline_stake_node_count += 1,
            }
        }

        let (all_stake_addresses, all_stake_total_amount) =
            get_all_stake(rpc_client, self.authorized_staker.pubkey())?;

        info!("Merge orphaned stake into the reserve");
        merge_orphaned_stake_accounts(
            rpc_client,
            &self.authorized_staker,
            &all_stake_addresses - &inuse_stake_addresses,
            self.reserve_stake_address,
        )?;

        info!("Merge transient stake back into either the reserve or validator stake");
        merge_transient_stake_accounts(
            rpc_client,
            &self.authorized_staker,
            desired_validator_stake,
            self.reserve_stake_address,
            &mut validator_stake_actions,
        )?;

        info!("Create validator stake accounts if needed");
        create_validator_stake_accounts(
            rpc_client,
            &self.authorized_staker,
            desired_validator_stake,
            self.reserve_stake_address,
            self.min_reserve_stake_balance,
            &mut validator_stake_actions,
        )?;

        // `total_stake_amount` excludes the amount that always remains in the reserve account
        let total_stake_amount = all_stake_total_amount - self.min_reserve_stake_balance;

        info!("Total stake pool balance: {}", Sol(total_stake_amount));

        let total_min_stake_amount = min_stake_node_count * MIN_STAKE_ACCOUNT_BALANCE;
        info!("Min node count: {}", min_stake_node_count);
        info!("Min stake amount: {}", Sol(total_min_stake_amount));

        let total_baseline_stake_amount = baseline_stake_node_count * self.baseline_stake_amount;
        info!("Baseline node count: {}", baseline_stake_node_count);
        info!("Baseline stake amount: {}", Sol(self.baseline_stake_amount));
        info!(
            "Total baseline stake amount: {}",
            Sol(total_baseline_stake_amount)
        );

        if total_stake_amount < total_baseline_stake_amount {
            return Err("Not enough stake to cover the baseline".into());
        }

        info!("Bonus node count: {}", bonus_stake_node_count);
        let total_bonus_stake_amount =
            total_stake_amount.saturating_sub(total_min_stake_amount + total_baseline_stake_amount);
        info!(
            "Total bonus stake amount: {}",
            Sol(total_bonus_stake_amount)
        );

        let bonus_stake_amount = if bonus_stake_node_count == 0 {
            0
        } else {
            total_bonus_stake_amount / (bonus_stake_node_count as u64)
        };

        info!("Bonus stake amount: {}", Sol(bonus_stake_amount));

        let reserve_stake_balance = get_available_reserve_stake_balance(
            rpc_client,
            self.reserve_stake_address,
            self.min_reserve_stake_balance,
        )
        .map_err(|err| {
            format!(
                "Unable to get reserve stake account balance: {}: {}",
                self.reserve_stake_address, err
            )
        })?;

        info!(
            "Reserve stake available balance before updates: {}",
            Sol(reserve_stake_balance)
        );

        let notes = vec![
            format!(
                "Stake pool size: {} (available for delegation: {})",
                Sol(total_stake_amount),
                Sol(reserve_stake_balance)
            ),
            format!("Baseline stake amount: {}", Sol(self.baseline_stake_amount)),
            format!("Bonus stake amount: {}", Sol(bonus_stake_amount)),
            format!(
                "Validators by stake level: None={}, Baseline={}, Bonus={}",
                min_stake_node_count, baseline_stake_node_count, bonus_stake_node_count
            ),
        ];

        let busy_validators = validator_stake_actions
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let mut unfunded_validators = HashSet::default();
        distribute_validator_stake(
            rpc_client,
            dry_run,
            &self.authorized_staker,
            desired_validator_stake
                .iter()
                .filter(|vs| !busy_validators.contains(&vs.identity))
                .cloned(),
            self.reserve_stake_address,
            reserve_stake_balance,
            self.baseline_stake_amount,
            bonus_stake_amount,
            &mut validator_stake_actions,
            &mut unfunded_validators,
        )?;
        Ok((
            notes,
            validator_stake_actions,
            unfunded_validators,
            bonus_stake_amount,
        ))
    }
}

fn get_available_reserve_stake_balance(
    rpc_client: &RpcClient,
    reserve_stake_address: Pubkey,
    reserve_stake_balance: u64,
) -> Result<u64, Box<dyn error::Error>> {
    let balance = rpc_client
        .get_balance(&reserve_stake_address)
        .map_err(|err| {
            format!(
                "Unable to get reserve stake account balance: {}: {}",
                reserve_stake_address, err
            )
        })?;
    if balance < reserve_stake_balance {
        warn!(
            "reserve stake account {} balance too low, {}. Minimum is {}",
            reserve_stake_address,
            Sol(balance),
            Sol(reserve_stake_balance)
        );
        Ok(0)
    } else {
        Ok(balance.saturating_sub(reserve_stake_balance))
    }
}

fn merge_orphaned_stake_accounts(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
    source_stake_addresses: HashSet<Pubkey>,
    reserve_stake_address: Pubkey,
) -> Result<(), Box<dyn error::Error>> {
    let mut transactions = vec![];
    for stake_address in source_stake_addresses {
        let stake_activation = rpc_client
            .get_stake_activation(stake_address, None)
            .map_err(|err| {
                format!(
                    "Unable to get stake activation for {}: {}",
                    stake_address, err
                )
            })?;

        match stake_activation.state {
            StakeActivationState::Activating | StakeActivationState::Deactivating => {}
            StakeActivationState::Active => {
                transactions.push(Transaction::new_with_payer(
                    &[stake_instruction::deactivate_stake(
                        &stake_address,
                        &authorized_staker.pubkey(),
                    )],
                    Some(&authorized_staker.pubkey()),
                ));
                debug!("Deactivating stake {}", stake_address);
            }
            StakeActivationState::Inactive => {
                transactions.push(Transaction::new_with_payer(
                    &stake_instruction::merge(
                        &reserve_stake_address,
                        &stake_address,
                        &authorized_staker.pubkey(),
                    ),
                    Some(&authorized_staker.pubkey()),
                ));

                debug!(
                    "Merging orphaned stake, {}, into reserve {}",
                    stake_address, reserve_stake_address
                );
            }
        }
    }

    if !send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty()
    {
        Err("Failed to merge orphaned stake accounts".into())
    } else {
        Ok(())
    }
}

fn merge_transient_stake_accounts(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
    desired_validator_stake: &[ValidatorStake],
    reserve_stake_address: Pubkey,
    validator_stake_actions: &mut ValidatorStakeActions,
) -> Result<(), Box<dyn error::Error>> {
    let mut transactions = vec![];
    for ValidatorStake {
        identity,
        vote_address,
        ..
    } in desired_validator_stake
    {
        let stake_address = validator_stake_address(authorized_staker.pubkey(), *vote_address);
        let transient_stake_address =
            validator_transient_stake_address(authorized_staker.pubkey(), *vote_address);

        let transient_stake_account = rpc_client
            .get_account_with_commitment(&transient_stake_address, rpc_client.commitment())?
            .value;

        if let Some(transient_stake_account) = transient_stake_account {
            let transient_stake_activation = rpc_client
                .get_stake_activation(transient_stake_address, None)
                .map_err(|err| {
                    format!(
                        "Unable to get activation information for transient stake account: {}: {}",
                        transient_stake_address, err
                    )
                })?;

            match transient_stake_activation.state {
                StakeActivationState::Activating => {
                    let action = format!(
                        "stake account busy due to transient stake activation: {:?}",
                        transient_stake_address,
                    );
                    warn!("Busy validator {}: {}", *identity, action);
                    validator_stake_actions.insert(*identity, action);
                }
                StakeActivationState::Deactivating => {
                    let action = format!(
                        "stake account busy due to transient stake deactivation: {}",
                        transient_stake_address,
                    );
                    warn!("Busy validator {}: {}", *identity, action);
                    validator_stake_actions.insert(*identity, action);
                }
                StakeActivationState::Active => {
                    let stake_account = rpc_client
                        .get_account_with_commitment(&stake_address, rpc_client.commitment())?
                        .value
                        .unwrap_or_default();

                    if stake_accounts_have_same_credits_observed(
                        &stake_account,
                        &transient_stake_account,
                    )? {
                        transactions.push(Transaction::new_with_payer(
                            &stake_instruction::merge(
                                &stake_address,
                                &transient_stake_address,
                                &authorized_staker.pubkey(),
                            ),
                            Some(&authorized_staker.pubkey()),
                        ));
                        debug!("Merging active transient stake for {}", identity);
                    } else {
                        let action = format!(
                            "stake account busy due to credits observed mismatch with transient stake account {}",
                            transient_stake_address
                        );
                        warn!("Busy validator {}: {}", *identity, action);
                        validator_stake_actions.insert(*identity, action);
                    }
                }
                StakeActivationState::Inactive => {
                    transactions.push(Transaction::new_with_payer(
                        &stake_instruction::merge(
                            &reserve_stake_address,
                            &transient_stake_address,
                            &authorized_staker.pubkey(),
                        ),
                        Some(&authorized_staker.pubkey()),
                    ));
                    debug!("Merging inactive transient stake for {}", identity);
                }
            }
        }
    }

    if !send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty()
    {
        Err("Failed to merge transient stake".into())
    } else {
        Ok(())
    }
}

fn stake_accounts_have_same_credits_observed(
    stake_account1: &Account,
    stake_account2: &Account,
) -> Result<bool, Box<dyn error::Error>> {
    use solana_sdk::stake::state::Stake;

    let stake_state1 = bincode::deserialize(stake_account1.data.as_slice())
        .map_err(|err| format!("Invalid stake account 1: {}", err))?;
    let stake_state2 = bincode::deserialize(stake_account2.data.as_slice())
        .map_err(|err| format!("Invalid stake account 2: {}", err))?;

    if let (
        StakeState::Stake(
            _,
            Stake {
                delegation: _,
                credits_observed: credits_observed1,
            },
        ),
        StakeState::Stake(
            _,
            Stake {
                delegation: _,
                credits_observed: credits_observed2,
            },
        ),
    ) = (stake_state1, stake_state2)
    {
        return Ok(credits_observed1 == credits_observed2);
    }
    Ok(false)
}

fn create_validator_stake_accounts(
    rpc_client: &RpcClient,
    authorized_staker: &Keypair,
    desired_validator_stake: &[ValidatorStake],
    reserve_stake_address: Pubkey,
    min_reserve_stake_balance: u64,
    validator_stake_actions: &mut ValidatorStakeActions,
) -> Result<(), Box<dyn error::Error>> {
    let mut reserve_stake_balance = get_available_reserve_stake_balance(
        rpc_client,
        reserve_stake_address,
        min_reserve_stake_balance,
    )
    .map_err(|err| {
        format!(
            "Unable to get reserve stake account balance: {}: {}",
            reserve_stake_address, err
        )
    })?;
    info!(
        "Reserve stake available balance: {}",
        Sol(reserve_stake_balance)
    );

    let mut transactions = vec![];
    for ValidatorStake {
        identity,
        vote_address,
        ..
    } in desired_validator_stake
    {
        let stake_address = validator_stake_address(authorized_staker.pubkey(), *vote_address);
        let stake_account = rpc_client
            .get_account_with_commitment(&stake_address, rpc_client.commitment())?
            .value;

        if stake_account.is_some() {
            // Check if the stake account is busy
            let stake_activation = rpc_client
                .get_stake_activation(stake_address, None)
                .map_err(|err| {
                    format!(
                        "Unable to get activation information for stake account: {}: {}",
                        stake_address, err
                    )
                })?;

            match stake_activation.state {
                StakeActivationState::Activating => {
                    let action = format!(
                        "stake account busy due to stake activation of {}",
                        stake_address
                    );
                    warn!("Busy validator {}: {}", *identity, action);
                    validator_stake_actions.insert(*identity, action);
                }
                StakeActivationState::Deactivating => {
                    let action = format!(
                        "stake account busy due to stake deactivation of {}",
                        stake_address
                    );
                    warn!("Busy validator {}: {}", *identity, action);
                    validator_stake_actions.insert(*identity, action);
                }
                StakeActivationState::Active => {}
                StakeActivationState::Inactive => {
                    let action =
                        format!("stake account busy due to inactive stake {}", stake_address);
                    warn!("Busy validator {}: {}", *identity, action);

                    transactions.push(Transaction::new_with_payer(
                        &[stake_instruction::delegate_stake(
                            &stake_address,
                            &authorized_staker.pubkey(),
                            vote_address,
                        )],
                        Some(&authorized_staker.pubkey()),
                    ));
                    debug!(
                        "Activating stake account for validator {} ({})",
                        identity, stake_address
                    );
                    validator_stake_actions.insert(*identity, action);
                }
            }
        } else {
            let action = if reserve_stake_balance < MIN_STAKE_ACCOUNT_BALANCE {
                // Try again next epoch
                warn!(
                    "Insufficient funds in reserve stake account to create stake account: {} required, {} balance",
                    Sol(MIN_STAKE_ACCOUNT_BALANCE), Sol(reserve_stake_balance)
                );

                format!(
                    "insufficient funds in reserve account to create stake account {}",
                    stake_address
                )
            } else {
                // Create a stake account for the validator
                reserve_stake_balance -= MIN_STAKE_ACCOUNT_BALANCE;

                let mut instructions = stake_instruction::split_with_seed(
                    &reserve_stake_address,
                    &authorized_staker.pubkey(),
                    MIN_STAKE_ACCOUNT_BALANCE,
                    &stake_address,
                    &authorized_staker.pubkey(),
                    &validator_stake_address_seed(*vote_address),
                );
                instructions.push(stake_instruction::delegate_stake(
                    &stake_address,
                    &authorized_staker.pubkey(),
                    vote_address,
                ));

                transactions.push(Transaction::new_with_payer(
                    &instructions,
                    Some(&authorized_staker.pubkey()),
                ));
                format!("creating new stake account {}", stake_address)
            };
            warn!("Busy validator {}: {}", *identity, action);
            validator_stake_actions.insert(*identity, action);
        }
    }

    if !send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
        .failed
        .is_empty()
    {
        Err("Failed to create validator stake accounts".into())
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn distribute_validator_stake<V>(
    rpc_client: &RpcClient,
    dry_run: bool,
    authorized_staker: &Keypair,
    desired_validator_stake: V,
    reserve_stake_address: Pubkey,
    mut reserve_stake_balance: u64,
    baseline_stake_amount: u64,
    bonus_stake_amount: u64,
    validator_stake_actions: &mut ValidatorStakeActions,
    unfunded_validators: &mut HashSet<Pubkey>,
) -> Result<(), Box<dyn error::Error>>
where
    V: IntoIterator<Item = ValidatorStake>,
{
    // Prioritize funding smaller stake accounts to maximize the number of accounts that will be
    // funded with the available reserve stake.  But validators with the priority flag jump the
    // line, since they were missed previous epoch
    let mut priority_stake = vec![];
    let mut min_stake = vec![];
    let mut baseline_stake = vec![];
    let mut bonus_stake = vec![];

    for validator_stake in desired_validator_stake {
        let stake_address =
            validator_stake_address(authorized_staker.pubkey(), validator_stake.vote_address);
        let transient_stake_address = validator_transient_stake_address(
            authorized_staker.pubkey(),
            validator_stake.vote_address,
        );

        let balance = rpc_client.get_balance(&stake_address).map_err(|err| {
            format!(
                "Unable to get stake account balance: {}: {}",
                stake_address, err
            )
        })? + rpc_client
            .get_balance(&transient_stake_address)
            .map_err(|err| {
                format!(
                    "Unable to get transient stake account balance: {}: {}",
                    transient_stake_address, err
                )
            })?;

        let list = if validator_stake.priority {
            &mut priority_stake
        } else {
            match validator_stake.stake_state {
                ValidatorStakeState::None => &mut min_stake,
                ValidatorStakeState::Baseline => &mut baseline_stake,
                ValidatorStakeState::Bonus => &mut bonus_stake,
            }
        };
        list.push((
            balance,
            stake_address,
            transient_stake_address,
            validator_stake,
        ));
    }

    // Sort from lowest to highest balance
    priority_stake.sort_by_key(|k| k.0);
    min_stake.sort_by_key(|k| k.0);
    baseline_stake.sort_by_key(|k| k.0);
    bonus_stake.sort_by_key(|k| k.0);

    let mut transactions = vec![];
    for (
        balance,
        stake_address,
        transient_stake_address,
        ValidatorStake {
            identity,
            stake_state,
            vote_address,
            priority,
        },
    ) in priority_stake
        .into_iter()
        .chain(min_stake)
        .chain(baseline_stake)
        .chain(bonus_stake)
    {
        let desired_balance = match stake_state {
            ValidatorStakeState::None => MIN_STAKE_ACCOUNT_BALANCE,
            ValidatorStakeState::Baseline => baseline_stake_amount,
            ValidatorStakeState::Bonus => bonus_stake_amount,
        };
        let transient_stake_address_seed = validator_transient_stake_address_seed(vote_address);

        #[allow(clippy::comparison_chain)]
        let op_msg = if balance > desired_balance {
            let amount_to_remove = balance - desired_balance;
            if amount_to_remove < MIN_STAKE_CHANGE_AMOUNT {
                format!("not removing {} (amount too small)", Sol(amount_to_remove))
            } else {
                let mut instructions = stake_instruction::split_with_seed(
                    &stake_address,
                    &authorized_staker.pubkey(),
                    amount_to_remove,
                    &transient_stake_address,
                    &authorized_staker.pubkey(),
                    &transient_stake_address_seed,
                );
                instructions.push(stake_instruction::deactivate_stake(
                    &transient_stake_address,
                    &authorized_staker.pubkey(),
                ));

                transactions.push(Transaction::new_with_payer(
                    &instructions,
                    Some(&authorized_staker.pubkey()),
                ));
                format!("removing {}", Sol(amount_to_remove))
            }
        } else if balance < desired_balance {
            let mut amount_to_add = desired_balance - balance;

            if amount_to_add < MIN_STAKE_CHANGE_AMOUNT {
                format!("not adding {} (amount too small)", Sol(amount_to_add))
            } else {
                if amount_to_add > reserve_stake_balance {
                    trace!(
                        "note: amount_to_add > reserve_stake_balance: {} > {}",
                        amount_to_add,
                        reserve_stake_balance
                    );
                    amount_to_add = reserve_stake_balance;
                }

                if amount_to_add < MIN_STAKE_CHANGE_AMOUNT {
                    if priority {
                        warn!("Failed to fund a priority node");
                    }
                    unfunded_validators.insert(identity);
                    "reserve depleted".to_string()
                } else {
                    reserve_stake_balance -= amount_to_add;

                    let mut instructions = stake_instruction::split_with_seed(
                        &reserve_stake_address,
                        &authorized_staker.pubkey(),
                        amount_to_add,
                        &transient_stake_address,
                        &authorized_staker.pubkey(),
                        &transient_stake_address_seed,
                    );
                    instructions.push(stake_instruction::delegate_stake(
                        &transient_stake_address,
                        &authorized_staker.pubkey(),
                        &vote_address,
                    ));

                    transactions.push(Transaction::new_with_payer(
                        &instructions,
                        Some(&authorized_staker.pubkey()),
                    ));
                    format!("adding {}", Sol(amount_to_add))
                }
            }
        } else {
            "no change".to_string()
        };

        let action = format!(
            "target stake amount: {}, current stake amount: {} - {}",
            Sol(desired_balance),
            Sol(balance),
            op_msg,
        );
        info!(
            "{} ({:?},priority={}) | {}",
            identity, stake_state, priority, action
        );
        validator_stake_actions.insert(identity, action);
    }
    info!(
        "Reserve stake available balance after updates: {}",
        Sol(reserve_stake_balance)
    );

    let ok = if dry_run {
        true
    } else {
        send_and_confirm_transactions(rpc_client, false, transactions, authorized_staker)?
            .failed
            .is_empty()
    };

    if !ok {
        Err("One or more transactions failed to execute".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::rpc_client_utils::test::*,
        solana_sdk::{
            clock::Epoch,
            epoch_schedule::{EpochSchedule, MINIMUM_SLOTS_PER_EPOCH},
            native_token::sol_to_lamports,
            signature::{Keypair, Signer},
        },
        solana_streamer::socket::SocketAddrSpace,
        solana_validator::test_validator::*,
    };

    fn num_stake_accounts(rpc_client: &RpcClient, authorized_staker: &Keypair) -> usize {
        get_all_stake(rpc_client, authorized_staker.pubkey())
            .unwrap()
            .0
            .len()
    }

    fn validator_stake_balance(
        rpc_client: &RpcClient,
        authorized_staker: Pubkey,
        validator: &ValidatorAddressPair,
    ) -> u64 {
        let stake_address = validator_stake_address(authorized_staker, validator.vote_address);
        rpc_client.get_balance(&stake_address).unwrap()
    }

    fn uniform_stake_pool_apply(
        stake_pool: &mut StakePool,
        rpc_client: &RpcClient,
        validators: &[ValidatorAddressPair],
        stake_state: ValidatorStakeState,
        expected_validator_stake_balance: u64,
        expected_reserve_stake_balance: u64,
    ) {
        let desired_validator_stake = validators
            .iter()
            .map(|vap| ValidatorStake {
                identity: vap.identity,
                vote_address: vap.vote_address,
                stake_state,
                priority: false,
            })
            .collect::<Vec<_>>();

        stake_pool
            .apply(rpc_client, false, &desired_validator_stake)
            .unwrap();

        assert_eq!(
            num_stake_accounts(rpc_client, &stake_pool.authorized_staker),
            1 + 2 * validators.len()
        );
        let _epoch = wait_for_next_epoch(rpc_client).unwrap();
        stake_pool
            .apply(rpc_client, false, &desired_validator_stake)
            .unwrap();

        assert_eq!(
            num_stake_accounts(rpc_client, &stake_pool.authorized_staker),
            1 + validators.len()
        );
        assert_eq!(
            rpc_client
                .get_balance(&stake_pool.reserve_stake_address)
                .unwrap(),
            expected_reserve_stake_balance
        );
        for validator in validators {
            assert_eq!(
                validator_stake_balance(
                    rpc_client,
                    stake_pool.authorized_staker.pubkey(),
                    validator
                ),
                expected_validator_stake_balance
            );
        }
    }

    #[test]
    fn this_test_is_too_big_and_slow() {
        solana_logger::setup_with_default("solana_stake_o_matic=info");

        let mut test_validator_genesis = TestValidatorGenesis::default();
        test_validator_genesis.epoch_schedule(EpochSchedule::custom(
            MINIMUM_SLOTS_PER_EPOCH,
            MINIMUM_SLOTS_PER_EPOCH,
            /* enable_warmup_epochs = */ false,
        ));
        let (test_validator, authorized_staker) = test_validator_genesis.start(SocketAddrSpace::Unspecified);

        let (rpc_client, _recent_blockhash, _fee_calculator) = test_validator.rpc_client();

        let authorized_staker_address = authorized_staker.pubkey();

        let assert_validator_stake_activation =
            |vap: &ValidatorAddressPair, epoch: Epoch, state: StakeActivationState| {
                let stake_address =
                    validator_stake_address(authorized_staker_address, vap.vote_address);
                assert_eq!(
                    rpc_client
                        .get_stake_activation(stake_address, Some(epoch))
                        .unwrap()
                        .state,
                    state,
                );
            };

        // ===========================================================
        info!("Create three validators, the reserve stake account, and a stake pool");
        let validators = create_validators(&rpc_client, &authorized_staker, 3).unwrap();

        let baseline_stake_amount = sol_to_lamports(10.);
        let min_reserve_stake_balance = MIN_STAKE_ACCOUNT_BALANCE;
        let total_stake_amount =
            (baseline_stake_amount + sol_to_lamports(100.)) * validators.len() as u64;
        let total_stake_amount_plus_min = total_stake_amount + min_reserve_stake_balance;

        let reserve_stake_address = create_stake_account(
            &rpc_client,
            &authorized_staker,
            &authorized_staker.pubkey(),
            total_stake_amount_plus_min,
        )
        .unwrap()
        .pubkey();

        let assert_reserve_account_only = || {
            assert_eq!(
                rpc_client.get_balance(&reserve_stake_address).unwrap(),
                total_stake_amount_plus_min
            );
            {
                assert_eq!(
                    get_available_reserve_stake_balance(
                        &rpc_client,
                        reserve_stake_address,
                        min_reserve_stake_balance
                    )
                    .unwrap(),
                    total_stake_amount
                );

                let (all_stake, all_stake_total_amount) =
                    get_all_stake(&rpc_client, authorized_staker_address).unwrap();
                assert_eq!(all_stake_total_amount, total_stake_amount_plus_min);
                assert_eq!(all_stake.len(), 1);
                assert!(all_stake.contains(&reserve_stake_address));
            }
        };
        assert_reserve_account_only();

        let mut stake_pool = new(
            &rpc_client,
            authorized_staker,
            baseline_stake_amount,
            reserve_stake_address,
            min_reserve_stake_balance,
        )
        .unwrap();

        // ===========================================================
        info!("Start with no stake in the validators");
        let epoch = rpc_client.get_epoch_info().unwrap().epoch;
        stake_pool
            .apply(
                &rpc_client,
                false,
                &validators
                    .iter()
                    .map(|vap| ValidatorStake {
                        identity: vap.identity,
                        vote_address: vap.vote_address,
                        stake_state: ValidatorStakeState::None,
                        priority: false,
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        info!("min: wait for stake activation");
        assert_eq!(
            rpc_client
                .get_balance(&stake_pool.reserve_stake_address)
                .unwrap(),
            total_stake_amount_plus_min - MIN_STAKE_ACCOUNT_BALANCE * validators.len() as u64,
        );

        for validator in &validators {
            assert_validator_stake_activation(validator, epoch, StakeActivationState::Activating);
            assert_eq!(
                validator_stake_balance(
                    &rpc_client,
                    stake_pool.authorized_staker.pubkey(),
                    validator
                ),
                MIN_STAKE_ACCOUNT_BALANCE
            );
        }
        assert_eq!(
            num_stake_accounts(&rpc_client, &stake_pool.authorized_staker),
            1 + validators.len()
        );
        let epoch = wait_for_next_epoch(&rpc_client).unwrap();
        for validator in &validators {
            assert_validator_stake_activation(validator, epoch, StakeActivationState::Active);
        }

        // ===========================================================
        info!("All validators to baseline stake level");
        uniform_stake_pool_apply(
            &mut stake_pool,
            &rpc_client,
            &validators,
            ValidatorStakeState::Baseline,
            baseline_stake_amount,
            total_stake_amount_plus_min - baseline_stake_amount * validators.len() as u64,
        );

        // ===========================================================
        info!("All the validators to bonus stake level");
        uniform_stake_pool_apply(
            &mut stake_pool,
            &rpc_client,
            &validators,
            ValidatorStakeState::Bonus,
            total_stake_amount / validators.len() as u64,
            MIN_STAKE_ACCOUNT_BALANCE,
        );

        // ===========================================================
        info!("Different stake for each validator");
        let desired_validator_stake = vec![
            ValidatorStake {
                identity: validators[0].identity,
                vote_address: validators[0].vote_address,
                stake_state: ValidatorStakeState::None,
                priority: false,
            },
            ValidatorStake {
                identity: validators[1].identity,
                vote_address: validators[1].vote_address,
                stake_state: ValidatorStakeState::Baseline,
                priority: false,
            },
            ValidatorStake {
                identity: validators[2].identity,
                vote_address: validators[2].vote_address,
                stake_state: ValidatorStakeState::Bonus,
                priority: false,
            },
        ];

        stake_pool
            .apply(&rpc_client, false, &desired_validator_stake)
            .unwrap();
        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();
        stake_pool
            .apply(&rpc_client, false, &desired_validator_stake)
            .unwrap();

        // after the first epoch, validators 0 and 1 are at their target levels but validator 2
        // needs one more epoch for the additional bonus stake to arrive
        for (validator, expected_sol_balance) in validators.iter().zip(&[1., 10., 110.]) {
            assert_eq!(
                sol_to_lamports(*expected_sol_balance),
                validator_stake_balance(
                    &rpc_client,
                    stake_pool.authorized_staker.pubkey(),
                    validator
                ),
                "stake balance mismatch for validator {}, expected {}",
                validator.identity,
                expected_sol_balance
            );
        }

        assert_eq!(
            rpc_client
                .get_balance(&stake_pool.reserve_stake_address)
                .unwrap(),
            MIN_STAKE_ACCOUNT_BALANCE,
        );

        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();
        stake_pool
            .apply(&rpc_client, false, &desired_validator_stake)
            .unwrap();

        assert_eq!(
            rpc_client
                .get_balance(&stake_pool.reserve_stake_address)
                .unwrap(),
            MIN_STAKE_ACCOUNT_BALANCE,
        );

        // after the second epoch, validator 2 is now has all the bonus stake
        for (validator, expected_sol_balance) in validators.iter().zip(&[1., 10., 319.]) {
            assert_eq!(
                sol_to_lamports(*expected_sol_balance),
                validator_stake_balance(
                    &rpc_client,
                    stake_pool.authorized_staker.pubkey(),
                    validator
                ),
                "stake balance mismatch for validator {}",
                validator.identity
            );
        }

        // ===========================================================
        info!("remove all validators");

        // deactivate all validator stake
        stake_pool.apply(&rpc_client, false, &[]).unwrap();
        let _epoch = wait_for_next_epoch(&rpc_client).unwrap();
        // merge deactivated validator stake back into the reserve
        stake_pool.apply(&rpc_client, false, &[]).unwrap();
        // all stake has returned to the reserve account
        assert_reserve_account_only();
    }
}
