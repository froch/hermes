/*!
   Implementation of [`ChainDriver`].
*/

use alloc::sync::Arc;
use core::time::Duration;
use eyre::eyre;
use std::cmp::max;
use tokio::runtime::Runtime;

use ibc_relayer::chain::cosmos::types::config::TxConfig;
use ibc_relayer::config::compat_mode::CompatMode;
use ibc_relayer_types::applications::transfer::amount::Amount;
use ibc_relayer_types::core::ics24_host::identifier::ChainId;

use crate::chain::chain_type::ChainType;
use crate::chain::cli::query::query_balance;
use crate::error::Error;
use crate::ibc::denom::Denom;
use crate::ibc::token::Token;
use crate::relayer::tx::new_tx_config_for_test;
use crate::types::env::{EnvWriter, ExportEnv};
use crate::types::wallet::WalletAddress;
use crate::util::retry::assert_eventually_succeed;

/**
   Number of times (seconds) to try and query a wallet to reach the
   target amount, as used by [`assert_eventual_wallet_amount`].

   We set this to around 60 seconds to make sure that the tests still
   pass in slower environments like the CI.

   If you encounter retry error, try increasing this constant. If the
   test is taking much longer to reach eventual consistency, it might
   be indication of some underlying performance issues.
*/
const WAIT_WALLET_AMOUNT_ATTEMPTS: u16 = 90;

/**
    A driver for interacting with a chain full nodes through command line.

    The name `ChainDriver` is inspired by
    [WebDriver](https://developer.mozilla.org/en-US/docs/Web/WebDriver),
    which is the term used to describe programs that control spawning of the
    web browsers. In our case, the ChainDriver is used to spawn and manage
    chain full nodes.

    Currently the `ChainDriver` is hardcoded to support only a single version
    of Gaia chain. In the future, we will want to turn this into one or more
    `ChainDriver` traits so that they can be used to spawn multiple chain
    implementations other than a single version of Gaia.
*/

#[derive(Debug, Clone)]
pub struct ChainDriver {
    pub chain_type: ChainType,
    /**
       The filesystem path to the Gaia CLI. Defaults to `gaiad`.
    */
    pub command_path: String,

    /**
       The ID of the chain.
    */
    pub chain_id: ChainId,

    /**
       The home directory for the full node to store data files.
    */
    pub home_path: String,

    pub account_prefix: String,

    /**
       The port used for RPC.
    */
    pub rpc_port: u16,

    /**
       The port used for GRPC.
    */
    pub grpc_port: u16,

    pub grpc_web_port: u16,

    /**
       The port used for P2P. (Currently unused other than for setup)
    */
    pub p2p_port: u16,

    /**
       The port used for pprof. (Currently unused other than for setup)
    */
    pub pprof_port: u16,

    pub tx_config: TxConfig,

    pub runtime: Arc<Runtime>,

    pub compat_mode: Option<CompatMode>,

    pub ipv6_grpc: bool,
}

impl ExportEnv for ChainDriver {
    fn export_env(&self, writer: &mut impl EnvWriter) {
        writer.write_env("CMD", &self.command_path);
        writer.write_env("HOME", &self.home_path);
        writer.write_env("RPC_ADDR", &self.rpc_address());
        writer.write_env("GRPC_ADDR", &self.grpc_address());
    }
}

impl ChainDriver {
    /// Create a new [`ChainDriver`]
    pub fn create(
        chain_type: ChainType,
        command_path: String,
        chain_id: ChainId,
        home_path: String,
        account_prefix: String,
        rpc_port: u16,
        grpc_port: u16,
        grpc_web_port: u16,
        p2p_port: u16,
        pprof_port: u16,
        runtime: Arc<Runtime>,
        native_token: String,
        compat_mode: Option<CompatMode>,
        ipv6_grpc: bool,
    ) -> Result<Self, Error> {
        let grpc_address = if ipv6_grpc {
            format!("http://[::1]:{grpc_port}")
        } else {
            format!("http://localhost:{grpc_port}")
        };
        let tx_config = new_tx_config_for_test(
            chain_id.clone(),
            chain_type.clone(),
            format!("http://localhost:{rpc_port}"),
            grpc_address,
            chain_type.address_type(),
            native_token,
        )?;

        Ok(Self {
            chain_type,
            command_path,
            chain_id,
            home_path,
            account_prefix,
            rpc_port,
            grpc_port,
            grpc_web_port,
            p2p_port,
            pprof_port,
            tx_config,
            runtime,
            compat_mode,
            ipv6_grpc,
        })
    }

    /// Returns the full URL for the RPC address.
    pub fn rpc_address(&self) -> String {
        format!("http://localhost:{}", self.rpc_port)
    }

    /// Returns the full URL for the WebSocket address.
    pub fn websocket_address(&self) -> String {
        format!("ws://localhost:{}/websocket", self.rpc_port)
    }

    /// Returns the full URL for the GRPC address.
    pub fn grpc_address(&self) -> String {
        if self.ipv6_grpc {
            format!("http://[::1]:{}", self.grpc_port)
        } else {
            format!("http://127.0.0.1:{}", self.grpc_port)
        }
    }

    /**
        Returns the full URL for the RPC address to listen to when starting
        the full node.

        This is somehow different from [`rpc_address`](ChainDriver::rpc_address)
        as it requires the `"tcp://"` scheme.
    */
    pub fn rpc_listen_address(&self) -> String {
        format!("tcp://localhost:{}", self.rpc_port)
    }

    /**
        Returns the full URL for the GRPC address to listen to when starting
        the full node.

        This is somehow different from [`grpc_address`](ChainDriver::grpc_address)
        as it requires no scheme to be specified.
    */
    pub fn grpc_listen_address(&self) -> String {
        if self.ipv6_grpc {
            format!("[::1]:{}", self.grpc_port)
        } else {
            format!("127.0.0.1:{}", self.grpc_port)
        }
    }

    /**
       Query for the balances for a given wallet address and denomination
    */
    pub fn query_balance(&self, wallet_id: &WalletAddress, denom: &Denom) -> Result<Amount, Error> {
        query_balance(
            self.chain_id.as_str(),
            &self.command_path,
            &self.rpc_listen_address(),
            &wallet_id.0,
            &denom.to_string(),
        )
    }

    /**
       Assert that a wallet should eventually have the expected amount in the
       given denomination.
    */
    pub fn assert_eventual_wallet_amount(
        &self,
        wallet: &WalletAddress,
        token: &Token,
    ) -> Result<(), Error> {
        assert_eventually_succeed(
            &format!("wallet reach {wallet} amount {token}"),
            WAIT_WALLET_AMOUNT_ATTEMPTS,
            Duration::from_secs(1),
            || {
                let amount: Amount = self.query_balance(wallet, &token.denom)?;

                if amount == token.amount {
                    Ok(())
                } else {
                    Err(Error::generic(eyre!(
                        "current balance of account {} with amount {} does not match the target amount {}",
                        wallet,
                        amount,
                        token
                    )))
                }
            },
        )?;

        Ok(())
    }

    /**
       Assert that a wallet should eventually have escrowed the amount for ICS29
       fees of a given denomination.
       Legacy ICS29 will escrow recv_fee + ack_fee + timeout_fee while more recent
       versions will escrow max(recv_fee + ack_fee, timeout_fee).
    */
    pub fn assert_eventual_escrowed_amount_ics29(
        &self,
        wallet: &WalletAddress,
        token: &Token,
        recv_fee: u128,
        ack_fee: u128,
        timeout_fee: u128,
    ) -> Result<(), Error> {
        assert_eventually_succeed(
            &format!("wallet reach {wallet} amount {token}"),
            WAIT_WALLET_AMOUNT_ATTEMPTS,
            Duration::from_secs(1),
            || {
                let amount: Amount = self.query_balance(wallet, &token.denom)?;

                let legacy_escrow = token
                    .amount
                    .checked_sub(recv_fee + ack_fee + timeout_fee)
                    .ok_or_else(|| {
                        Error::generic(eyre!(
                            "error computing the following subtraction: {}-{}",
                            token.amount,
                            recv_fee + ack_fee + timeout_fee
                        ))
                    })?;
                let escrow = token
                    .amount
                    .checked_sub(max(recv_fee + ack_fee, timeout_fee))
                    .ok_or_else(|| {
                        Error::generic(eyre!(
                            "error computing the following subtraction: {}-{}",
                            token.amount,
                            max(recv_fee + ack_fee, timeout_fee)
                        ))
                    })?;

                // Assert either the legacy or current ICS29 amount has been escrowed
                if amount == legacy_escrow || amount == escrow {
                    Ok(())
                } else {
                    Err(Error::generic(eyre!(
                        "current balance of account {} with amount {} does not match the target amount {}",
                        wallet,
                        amount,
                        token
                    )))
                }
            },
        )?;

        Ok(())
    }
}
