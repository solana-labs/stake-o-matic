use {
    log::*,
    solana_client::{
        rpc_client::RpcClient,
        rpc_config::RpcSimulateTransactionConfig,
        rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
        rpc_filter,
        rpc_request::MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS,
        rpc_response::{self, RpcVoteAccountInfo, RpcVoteAccountStatus},
    },
    solana_sdk::{
        clock::Epoch,
        native_token::*,
        pubkey::Pubkey,
        signature::{Keypair, Signature, Signer},
        stake,
        transaction::Transaction,
    },
    std::{
        collections::{HashMap, HashSet},
        error,
        str::FromStr,
        thread::sleep,
        time::{Duration, Instant},
    },
};

pub struct SendAndConfirmTransactionResult {
    pub succeeded: HashSet<Signature>,
    pub failed: HashSet<Signature>,
}

struct PendingTransactionsEntry {
    pub last_valid_block_height: u64,
    pub transactions: Vec<Transaction>,
}

const BLOCK_HEIGHT_CHECK_INTERVAL_SEC: Duration = Duration::from_secs(5);
const MAX_BLOCKHASH_ROTATION_TTL_BLOCKS: u64 = 225;

pub fn send_and_confirm_transactions(
    rpc_client: &RpcClient,
    dry_run: bool,
    transactions: Vec<Transaction>,
    authorized_staker: &Keypair,
) -> Result<SendAndConfirmTransactionResult, Box<dyn error::Error>> {
    let authorized_staker_balance = rpc_client.get_balance(&authorized_staker.pubkey())?;
    info!(
        "Authorized staker balance: {} SOL",
        lamports_to_sol(authorized_staker_balance)
    );

    let rpc_response::Fees {
        mut blockhash,
        fee_calculator,
        mut last_valid_block_height,
    } = rpc_client.get_fees()?;
    let total_transactions = transactions.len();
    info!("{} transactions to send", total_transactions);

    let required_fee = transactions.iter().fold(0, |fee, transaction| {
        fee + fee_calculator.calculate_fee(&transaction.message)
    });
    info!("Required fee: {} SOL", lamports_to_sol(required_fee));
    if required_fee > authorized_staker_balance {
        return Err("Authorized staker has insufficient funds".into());
    }

    let mut pending_transactions = HashMap::new();
    let mut last_block_height_check = Instant::now();
    let mut current_block_height = rpc_client.get_epoch_info()?.block_height;
    for mut transaction in transactions {
        if dry_run {
            rpc_client.simulate_transaction_with_config(
                &transaction,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    ..RpcSimulateTransactionConfig::default()
                },
            )?;
        } else {
            let now = Instant::now();
            if now.duration_since(last_block_height_check) >= BLOCK_HEIGHT_CHECK_INTERVAL_SEC {
                current_block_height = rpc_client.get_epoch_info()?.block_height;
                last_block_height_check = now;
            }

            let current_block_height_ttl = current_block_height + MAX_BLOCKHASH_ROTATION_TTL_BLOCKS;
            if current_block_height_ttl >= last_valid_block_height {
                let fees = rpc_client.get_fees()?;
                blockhash = fees.blockhash;
                last_valid_block_height = fees.last_valid_block_height;
                info!("New blockhash: {}", blockhash);
            }

            transaction.sign(&[authorized_staker], blockhash);
            let _ = rpc_client.send_transaction(&transaction).map_err(|err| {
                warn!("Failed to send transaction: {:?}", err);
            });
        }

        pending_transactions
            .entry(blockhash)
            .or_insert_with(|| PendingTransactionsEntry {
                last_valid_block_height,
                transactions: Vec::new(),
            })
            .transactions
            .push(transaction);
    }

    let mut succeeded_transactions = HashSet::new();
    let mut failed_transactions = HashSet::new();
    loop {
        if pending_transactions.is_empty() {
            break;
        }

        let mut expired_pending_transactions = Vec::new();
        let mut live_pending_transactions_entries = HashMap::new();
        for (blockhash, entry) in pending_transactions.into_iter() {
            let now = Instant::now();
            if now.duration_since(last_block_height_check) >= BLOCK_HEIGHT_CHECK_INTERVAL_SEC {
                current_block_height = rpc_client.get_epoch_info()?.block_height;
                last_block_height_check = now;
            }

            if current_block_height >= entry.last_valid_block_height {
                info!("Blockhash expired: {}", blockhash);
                let PendingTransactionsEntry { transactions, .. } = entry;
                expired_pending_transactions.extend(transactions.into_iter());
            } else {
                live_pending_transactions_entries.insert(blockhash, entry);
            }
        }
        for mut transaction in expired_pending_transactions {
            let now = Instant::now();
            if now.duration_since(last_block_height_check) >= BLOCK_HEIGHT_CHECK_INTERVAL_SEC {
                current_block_height = rpc_client.get_epoch_info()?.block_height;
                last_block_height_check = now;
            }

            let current_block_height_ttl = current_block_height + MAX_BLOCKHASH_ROTATION_TTL_BLOCKS;
            if current_block_height_ttl >= last_valid_block_height {
                let fees = rpc_client.get_fees()?;
                blockhash = fees.blockhash;
                last_valid_block_height = fees.last_valid_block_height;
                info!("New blockhash: {}", blockhash);
            }

            transaction.sign(&[authorized_staker], blockhash);
            let _ = rpc_client.send_transaction(&transaction).map_err(|err| {
                warn!("Failed to send transaction: {:?}", err);
            });

            live_pending_transactions_entries
                .entry(blockhash)
                .or_insert_with(|| PendingTransactionsEntry {
                    last_valid_block_height,
                    transactions: Vec::new(),
                })
                .transactions
                .push(transaction);
        }
        pending_transactions = live_pending_transactions_entries;

        for (_blockhash, entry) in pending_transactions.iter_mut() {
            let mut statuses = vec![];
            for pending_signatures_chunk in entry
                .transactions
                .iter()
                .map(|transaction| transaction.signatures[0])
                .collect::<Vec<_>>()
                .chunks(MAX_GET_SIGNATURE_STATUSES_QUERY_ITEMS - 1)
            {
                trace!(
                    "checking {} pending transactions",
                    pending_signatures_chunk.len()
                );
                statuses.extend(
                    rpc_client
                        .get_signature_statuses(pending_signatures_chunk)?
                        .value
                        .into_iter(),
                )
            }
            assert_eq!(statuses.len(), entry.transactions.len());

            let mut still_pending_transactions = vec![];
            for (transaction, status) in entry.transactions.drain(..).zip(statuses.into_iter()) {
                let signature = transaction.signatures[0];
                trace!("{}: status={:?}", signature, status);
                let completed = if dry_run {
                    Some(true)
                } else if let Some(status) = &status {
                    if status.satisfies_commitment(rpc_client.commitment()) {
                        Some(status.err.is_none())
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(success) = completed {
                    info!("{}: completed. success={}", signature, success);
                    if success {
                        succeeded_transactions.insert(signature);
                    } else {
                        failed_transactions.insert(signature);
                    }
                } else {
                    still_pending_transactions.push(transaction);
                }
            }
            entry
                .transactions
                .splice(.., still_pending_transactions.into_iter());
        }
        sleep(Duration::from_millis(500));
    }

    Ok(SendAndConfirmTransactionResult {
        succeeded: succeeded_transactions,
        failed: failed_transactions,
    })
}

pub struct VoteAccountInfo {
    pub identity: Pubkey,
    pub vote_address: Pubkey,
    pub commission: u8,
    pub active_stake: u64,

    /// Credits earned in the epoch
    pub epoch_credits: u64,
}

pub fn get_vote_account_info(
    rpc_client: &RpcClient,
    epoch: Epoch,
) -> Result<(Vec<VoteAccountInfo>, u64), Box<dyn error::Error>> {
    let RpcVoteAccountStatus {
        current,
        delinquent,
    } = rpc_client.get_vote_accounts()?;

    let mut latest_vote_account_info = HashMap::<String, _>::new();

    let mut total_active_stake = 0;
    for vote_account_info in current.into_iter().chain(delinquent.into_iter()) {
        total_active_stake += vote_account_info.activated_stake;

        let entry = latest_vote_account_info
            .entry(vote_account_info.node_pubkey.clone())
            .or_insert_with(|| vote_account_info.clone());

        // If the validator has multiple staked vote accounts then select the vote account that
        // voted most recently
        if entry.last_vote < vote_account_info.last_vote {
            *entry = vote_account_info.clone();
        }
    }

    Ok((
        latest_vote_account_info
            .values()
            .map(
                |RpcVoteAccountInfo {
                     commission,
                     node_pubkey,
                     vote_pubkey,
                     epoch_credits,
                     activated_stake,
                     ..
                 }| {
                    let epoch_credits = if let Some((_last_epoch, credits, prev_credits)) =
                        epoch_credits.iter().find(|ec| ec.0 == epoch)
                    {
                        credits.saturating_sub(*prev_credits)
                    } else {
                        0
                    };
                    let identity = Pubkey::from_str(node_pubkey).unwrap();
                    let vote_address = Pubkey::from_str(vote_pubkey).unwrap();

                    VoteAccountInfo {
                        identity,
                        vote_address,
                        active_stake: *activated_stake,
                        commission: *commission,
                        epoch_credits,
                    }
                },
            )
            .collect(),
        total_active_stake,
    ))
}

pub fn get_all_stake(
    rpc_client: &RpcClient,
    authorized_staker: Pubkey,
) -> Result<(HashSet<Pubkey>, u64), Box<dyn error::Error>> {
    let mut all_stake_addresses = HashSet::new();
    let mut total_stake_balance = 0;

    let all_stake_accounts = rpc_client.get_program_accounts_with_config(
        &stake::program::id(),
        RpcProgramAccountsConfig {
            filters: Some(vec![
                // Filter by `Meta::authorized::staker`, which begins at byte offset 12
                rpc_filter::RpcFilterType::Memcmp(rpc_filter::Memcmp {
                    offset: 12,
                    bytes: rpc_filter::MemcmpEncodedBytes::Binary(authorized_staker.to_string()),
                    encoding: Some(rpc_filter::MemcmpEncoding::Binary),
                }),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
                commitment: Some(rpc_client.commitment()),
                ..RpcAccountInfoConfig::default()
            },
            ..RpcProgramAccountsConfig::default()
        },
    )?;

    for (address, account) in all_stake_accounts {
        all_stake_addresses.insert(address);
        total_stake_balance += account.lamports;
    }

    Ok((all_stake_addresses, total_stake_balance))
}

#[cfg(test)]
pub mod test {
    use {
        super::*,
        borsh::BorshSerialize,
        indicatif::{ProgressBar, ProgressStyle},
        solana_client::client_error,
        solana_sdk::{
            borsh::get_packed_len,
            clock::Epoch,
            program_pack::Pack,
            pubkey::Pubkey,
            stake::{
                instruction as stake_instruction,
                state::{Authorized, Lockup},
            },
            system_instruction,
        },
        solana_vote_program::{vote_instruction, vote_state::VoteInit},
        spl_stake_pool::{
            find_stake_program_address, find_withdraw_authority_program_address,
            state::{Fee, StakePool, ValidatorList},
        },
        spl_token::state::{Account, Mint},
    };

    fn new_spinner_progress_bar() -> ProgressBar {
        let progress_bar = ProgressBar::new(42);
        progress_bar
            .set_style(ProgressStyle::default_spinner().template("{spinner:.green} {wide_msg}"));
        progress_bar.enable_steady_tick(100);
        progress_bar
    }

    pub fn wait_for_next_epoch(rpc_client: &RpcClient) -> client_error::Result<Epoch> {
        let current_epoch = rpc_client.get_epoch_info()?.epoch;

        let progress_bar = new_spinner_progress_bar();
        loop {
            let epoch_info = rpc_client.get_epoch_info()?;
            if epoch_info.epoch > current_epoch {
                return Ok(epoch_info.epoch);
            }
            progress_bar.set_message(&format!(
                "Waiting for epoch {} ({} slots remaining)",
                current_epoch + 1,
                epoch_info
                    .slots_in_epoch
                    .saturating_sub(epoch_info.slot_index),
            ));

            sleep(Duration::from_millis(200));
        }
    }

    pub fn create_vote_account(
        rpc_client: &RpcClient,
        payer: &Keypair,
        identity_keypair: &Keypair,
        vote_keypair: &Keypair,
    ) -> client_error::Result<()> {
        let mut transaction = Transaction::new_with_payer(
            &vote_instruction::create_account(
                &payer.pubkey(),
                &vote_keypair.pubkey(),
                &VoteInit {
                    node_pubkey: identity_keypair.pubkey(),
                    authorized_voter: identity_keypair.pubkey(),
                    authorized_withdrawer: identity_keypair.pubkey(),
                    commission: 10,
                },
                sol_to_lamports(1.),
            ),
            Some(&payer.pubkey()),
        );

        transaction.sign(
            &[payer, identity_keypair, vote_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn create_stake_account(
        rpc_client: &RpcClient,
        payer: &Keypair,
        authority: &Pubkey,
        amount: u64,
    ) -> client_error::Result<Keypair> {
        let stake_keypair = Keypair::new();
        let mut transaction = Transaction::new_with_payer(
            &stake_instruction::create_account(
                &payer.pubkey(),
                &stake_keypair.pubkey(),
                &Authorized::auto(authority),
                &Lockup::default(),
                amount,
            ),
            Some(&payer.pubkey()),
        );

        transaction.sign(
            &[payer, &stake_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| stake_keypair)
    }

    pub fn delegate_stake(
        rpc_client: &RpcClient,
        authority: &Keypair,
        stake_address: &Pubkey,
        vote_address: &Pubkey,
    ) -> client_error::Result<()> {
        let transaction = Transaction::new_signed_with_payer(
            &[stake_instruction::delegate_stake(
                stake_address,
                &authority.pubkey(),
                vote_address,
            )],
            Some(&authority.pubkey()),
            &[authority],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub struct ValidatorAddressPair {
        pub identity: Pubkey,
        pub vote_address: Pubkey,
    }

    pub fn create_validators(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        num_validators: u32,
    ) -> client_error::Result<Vec<ValidatorAddressPair>> {
        let mut validators = vec![];

        for _ in 0..num_validators {
            let identity_keypair = Keypair::new();
            let vote_keypair = Keypair::new();

            create_vote_account(
                rpc_client,
                authorized_staker,
                &identity_keypair,
                &vote_keypair,
            )?;

            validators.push(ValidatorAddressPair {
                identity: identity_keypair.pubkey(),
                vote_address: vote_keypair.pubkey(),
            });
        }

        Ok(validators)
    }

    pub fn create_mint(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        manager: &Pubkey,
    ) -> client_error::Result<Pubkey> {
        let mint_rent = rpc_client.get_minimum_balance_for_rent_exemption(Mint::LEN)?;
        let mint_keypair = Keypair::new();

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &authorized_staker.pubkey(),
                    &mint_keypair.pubkey(),
                    mint_rent,
                    Mint::LEN as u64,
                    &spl_token::id(),
                ),
                spl_token::instruction::initialize_mint(
                    &spl_token::id(),
                    &mint_keypair.pubkey(),
                    manager,
                    None,
                    0,
                )
                .unwrap(),
            ],
            Some(&authorized_staker.pubkey()),
        );

        transaction.sign(
            &[authorized_staker, &mint_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| mint_keypair.pubkey())
    }

    pub fn create_token_account(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        mint: &Pubkey,
        owner: &Pubkey,
    ) -> client_error::Result<Pubkey> {
        let account_rent = rpc_client.get_minimum_balance_for_rent_exemption(Account::LEN)?;
        let account_keypair = Keypair::new();

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &authorized_staker.pubkey(),
                    &account_keypair.pubkey(),
                    account_rent,
                    Account::LEN as u64,
                    &spl_token::id(),
                ),
                spl_token::instruction::initialize_account(
                    &spl_token::id(),
                    &account_keypair.pubkey(),
                    mint,
                    owner,
                )
                .unwrap(),
            ],
            Some(&authorized_staker.pubkey()),
        );

        transaction.sign(
            &[authorized_staker, &account_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| account_keypair.pubkey())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_stake_pool(
        rpc_client: &RpcClient,
        payer: &Keypair,
        stake_pool: &Keypair,
        reserve_stake: &Pubkey,
        pool_mint: &Pubkey,
        pool_token_account: &Pubkey,
        manager: &Keypair,
        staker: &Pubkey,
        max_validators: u32,
    ) -> client_error::Result<()> {
        let stake_pool_size = get_packed_len::<StakePool>();
        let stake_pool_rent = rpc_client
            .get_minimum_balance_for_rent_exemption(stake_pool_size)
            .unwrap();
        let validator_list = ValidatorList::new(max_validators);
        let validator_list_size = validator_list.try_to_vec().unwrap().len();
        let validator_list_rent = rpc_client
            .get_minimum_balance_for_rent_exemption(validator_list_size)
            .unwrap();
        let validator_list = Keypair::new();
        let fee = Fee {
            numerator: 10,
            denominator: 100,
        };

        let mut transaction = Transaction::new_with_payer(
            &[
                system_instruction::create_account(
                    &payer.pubkey(),
                    &stake_pool.pubkey(),
                    stake_pool_rent,
                    stake_pool_size as u64,
                    &spl_stake_pool::id(),
                ),
                system_instruction::create_account(
                    &payer.pubkey(),
                    &validator_list.pubkey(),
                    validator_list_rent,
                    validator_list_size as u64,
                    &spl_stake_pool::id(),
                ),
                spl_stake_pool::instruction::initialize(
                    &spl_stake_pool::id(),
                    &stake_pool.pubkey(),
                    &manager.pubkey(),
                    staker,
                    &validator_list.pubkey(),
                    reserve_stake,
                    pool_mint,
                    pool_token_account,
                    &spl_token::id(),
                    None,
                    fee,
                    max_validators,
                ),
            ],
            Some(&payer.pubkey()),
        );
        transaction.sign(
            &[payer, stake_pool, &validator_list, manager],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn deposit_into_stake_pool(
        rpc_client: &RpcClient,
        authorized_staker: &Keypair,
        stake_pool_address: &Pubkey,
        stake_pool: &StakePool,
        vote_address: &Pubkey,
        stake_address: &Pubkey,
        pool_token_address: &Pubkey,
    ) -> client_error::Result<()> {
        let validator_stake_address =
            find_stake_program_address(&spl_stake_pool::id(), vote_address, stake_pool_address).0;
        let pool_withdraw_authority =
            find_withdraw_authority_program_address(&spl_stake_pool::id(), stake_pool_address).0;
        let transaction = Transaction::new_signed_with_payer(
            &spl_stake_pool::instruction::deposit(
                &spl_stake_pool::id(),
                stake_pool_address,
                &stake_pool.validator_list,
                &pool_withdraw_authority,
                stake_address,
                &authorized_staker.pubkey(),
                &validator_stake_address,
                &stake_pool.reserve_stake,
                pool_token_address,
                &stake_pool.pool_mint,
                &spl_token::id(),
            ),
            Some(&authorized_staker.pubkey()),
            &[authorized_staker],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }

    pub fn transfer(
        rpc_client: &RpcClient,
        from_keypair: &Keypair,
        to_address: &Pubkey,
        lamports: u64,
    ) -> client_error::Result<()> {
        let transaction = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &from_keypair.pubkey(),
                to_address,
                lamports,
            )],
            Some(&from_keypair.pubkey()),
            &[from_keypair],
            rpc_client.get_recent_blockhash()?.0,
        );
        rpc_client
            .send_and_confirm_transaction_with_spinner(&transaction)
            .map(|_| ())
    }
}
