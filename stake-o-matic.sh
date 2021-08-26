#!/usr/bin/env bash
#
# Downloads and runs the latest stake-o-matic binary
#
set -ex

#"$(dirname "$0")"/fetch-release.sh "$STAKE_O_MATIC_RELEASE"
curl https://sh.rustup.rs -sSf | sh -s -- -y
source $HOME/.cargo/env
solana_stake_o_matic="cargo run --bin solana-stake-o-matic --"

if [[ -n $FOLLOWER ]]; then
  REQUIRE_CLASSIFICATION="--require-classification"
else
  MARKDOWN="--markdown first"
fi

if [[ ! -d db ]]; then
  git clone git@github.com:solana-labs/stake-o-matic.wiki.git db
fi

# shellcheck disable=SC2206
TESTNET_ARGS=(
  --url ${URL:?}
  --cluster testnet
  --quality-block-producer-percentage 30
  --max-poor-block-producer-percentage 20
  --max-infrastructure-concentration 25
  --min-epoch-credit-percentage-of-average 35
  --infrastructure-concentration-affects destake-new
  --min-release-version 1.7.3
)

# shellcheck disable=SC2206
MAINNET_BETA_ARGS=(
  --url ${URL:?}
  --cluster mainnet-beta
  --quality-block-producer-percentage 30
  --max-poor-block-producer-percentage 20
  --min-epoch-credit-percentage-of-average 35
  --max-active-stake 3000000
  --max-commission 10
  --min-release-version 1.6.20
  --max-infrastructure-concentration 10
  --infrastructure-concentration-affects destake-new
  --min-self-stake 100
  --min-testnet-participation 5 10
)

# shellcheck disable=SC2206
NOT_A_STAKE_POOL_ARGS=(
  $MARKDOWN
  $CONFIRM
  $REQUIRE_CLASSIFICATION
  stake-pool-v0
  --min-reserve-stake-balance ${MIN_RESERVE_STAKE_BALANCE:?}
  ${RESERVE_ACCOUNT_ADDRESS:?}
  ${STAKE_AUTHORITY_KEYPAIR:?}
  ${BASELINE_STAKE_AMOUNT:?}
)

# shellcheck disable=SC2206
STAKE_POOL_ARGS=(
  $CONFIRM
  $REQUIRE_CLASSIFICATION
  --db-suffix stake-pool
  stake-pool
  ${STAKE_POOL_ADDRESS:?}
  ${STAKE_AUTHORITY_KEYPAIR:?}
  ${BASELINE_STAKE_AMOUNT:?}
)

if [[ $CLUSTER = "testnet-stake-pool" ]]; then
  $solana_stake_o_matic "${TESTNET_ARGS[@]}" "${STAKE_POOL_ARGS[@]}"
elif [[ $CLUSTER = "mainnet-beta-stake-pool" ]]; then
  $solana_stake_o_matic "${MAINNET_BETA_ARGS[@]}" "${STAKE_POOL_ARGS[@]}"
elif [[ $CLUSTER == "testnet" ]]; then
  $solana_stake_o_matic "${TESTNET_ARGS[@]}" "${NOT_A_STAKE_POOL_ARGS[@]}"
elif [[ $CLUSTER == "mainnet-beta" ]]; then
  $solana_stake_o_matic "${MAINNET_BETA_ARGS[@]}" "${NOT_A_STAKE_POOL_ARGS[@]}"
else
  echo "CLUSTER must be set to testnet-stake-pool, mainnet-beta-stake-pool, testnet, or mainnet-beta"
  exit 1
fi

if [[ -z $FOLLOWER && $BUILDKITE = true ]]; then
  git config --global user.email maintainers@solana.foundation
  git config --global user.name "Solana Maintainers"
  cd db
  git add ./*
  if ! git diff-index --quiet HEAD; then
    git commit -m "Automated update by $BUILDKITE_BUILD_ID"
    git push origin
  fi
fi
