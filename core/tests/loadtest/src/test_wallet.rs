// Built-in import
use std::sync::atomic::{AtomicU32, Ordering};
// External uses
use num::BigUint;
// Workspace uses
use zksync::{
    error::ClientError,
    ethereum::ierc20_contract,
    provider::Provider,
    types::BlockStatus,
    utils::{biguint_to_u256, closest_packable_fee_amount, u256_to_biguint},
    web3::{
        contract::{Contract, Options},
        types::H256,
    },
    EthereumProvider, Network, RpcProvider, Wallet, WalletCredentials,
};
use zksync_eth_signer::PrivateKeySigner;
use zksync_types::{
    tx::PackedEthSignature, AccountId, Address, Nonce, PriorityOp, TokenLike, TxFeeTypes, ZkSyncTx,
};
// Local uses
use crate::{config::AccountInfo, monitor::Monitor, session::save_wallet};

/// A wrapper over `zksync::Wallet` to make testing more convenient.
#[derive(Debug)]
pub struct TestWallet {
    monitor: Monitor,
    eth_provider: EthereumProvider<PrivateKeySigner>,
    inner: Wallet<PrivateKeySigner, RpcProvider>,
    token_name: TokenLike,

    nonce: AtomicU32,
}

impl TestWallet {
    const FEE_FACTOR: u64 = 3;

    /// Creates a new wallet from the given account information and Ethereum configuration options.
    pub async fn from_info(monitor: Monitor, info: &AccountInfo, web3_url: &str) -> Self {
        let credentials = WalletCredentials::from_eth_signer(
            info.address,
            PrivateKeySigner::new(info.private_key),
            Network::Localhost,
        )
        .await
        .unwrap();

        let inner = Wallet::new(monitor.provider.clone(), credentials)
            .await
            .unwrap();

        let wallet = Self::from_wallet(info.token_name.clone(), monitor, inner, web3_url).await;
        save_wallet(info.clone());
        wallet
    }

    /// Creates a random wallet.
    pub async fn new_random(token_name: TokenLike, monitor: Monitor, web3_url: &str) -> Self {
        let eth_private_key = gen_random_eth_private_key();
        let address_from_pk =
            PackedEthSignature::address_from_private_key(&eth_private_key).unwrap();

        let info = AccountInfo {
            address: address_from_pk,
            private_key: eth_private_key,
            token_name,
        };

        Self::from_info(monitor, &info, web3_url).await
    }

    async fn from_wallet(
        token_name: TokenLike,
        monitor: Monitor,
        inner: Wallet<PrivateKeySigner, RpcProvider>,
        web3_url: impl AsRef<str>,
    ) -> Self {
        let eth_provider = inner.ethereum(web3_url).await.unwrap();
        let zk_nonce = inner
            .provider
            .account_info(inner.address())
            .await
            .unwrap()
            .committed
            .nonce;

        monitor
            .api_data_pool
            .write()
            .await
            .store_address(inner.address());

        Self {
            monitor,
            inner,
            eth_provider,
            nonce: AtomicU32::new(*zk_nonce),
            token_name,
        }
    }

    /// Sets the correct nonce from the zkSync network.
    ///
    /// This method fixes further "nonce mismatch" errors.
    pub async fn refresh_nonce(&self) -> Result<(), ClientError> {
        let zk_nonce = self
            .inner
            .provider
            .account_info(self.address())
            .await?
            .committed
            .nonce;

        self.nonce.store(*zk_nonce, Ordering::SeqCst);
        Ok(())
    }

    /// Returns the wallet address.
    pub fn address(&self) -> Address {
        self.inner.address()
    }

    /// Returns sufficient fee required to process each kind of transactions in zkSync network.
    pub async fn sufficient_fee(&self) -> Result<BigUint, ClientError> {
        let fee = self
            .monitor
            .provider
            .get_tx_fee(
                TxFeeTypes::FastWithdraw,
                Address::zero(),
                self.token_name.clone(),
            )
            .await?
            .total_fee
            * BigUint::from(Self::FEE_FACTOR);

        Ok(closest_packable_fee_amount(&fee))
    }

    /// Returns the wallet balance in zkSync network.
    pub async fn balance(&self, block_status: BlockStatus) -> Result<BigUint, ClientError> {
        self.inner
            .get_balance(block_status, self.token_name.clone())
            .await
    }

    /// Returns the wallet balance in Ehtereum network.
    pub async fn eth_balance(&self) -> Result<BigUint, ClientError> {
        self.eth_provider.balance().await
    }

    /// Returns erc20 token balance in Ethereum network.
    pub async fn erc20_balance(&self) -> Result<BigUint, ClientError> {
        let token = self
            .inner
            .tokens
            .resolve(self.token_name.clone())
            .ok_or(ClientError::UnknownToken)?;

        let contract = Contract::new(
            self.eth_provider.client().web3.eth(),
            token.address,
            ierc20_contract(),
        );

        let balance = contract
            .query("balanceOf", self.address(), None, Options::default(), None)
            .await
            .map(u256_to_biguint)
            .map_err(|err| ClientError::NetworkError(err.to_string()))?;

        Ok(balance)
    }

    /// Returns eth balance if token name is ETH; otherwise returns erc20 balance.
    pub async fn l1_balance(&self) -> Result<BigUint, ClientError> {
        if self.token_name.is_eth() {
            self.eth_balance().await
        } else {
            self.erc20_balance().await
        }
    }

    /// Returns the token name of this wallet.
    pub fn token_name(&self) -> &TokenLike {
        &self.token_name
    }

    /// Returns the current account ID.
    pub fn account_id(&self) -> Option<AccountId> {
        self.inner.account_id()
    }

    // Updates ZKSync account id.
    pub async fn update_account_id(&mut self) -> Result<(), ClientError> {
        self.inner.update_account_id().await?;
        if let Some(account_id) = self.account_id() {
            self.monitor
                .api_data_pool
                .write()
                .await
                .set_account_id(self.address(), account_id);
        }
        Ok(())
    }

    // Creates a signed change public key transaction.
    pub async fn sign_change_pubkey(
        &self,
        fee: impl Into<BigUint>,
    ) -> Result<(ZkSyncTx, Option<PackedEthSignature>), ClientError> {
        let tx = self
            .inner
            .start_change_pubkey()
            .nonce(self.pending_nonce())
            .fee_token(self.token_name.clone())?
            .fee(fee)
            .tx()
            .await?;

        Ok((tx, None))
    }

    // Creates a signed withdraw transaction with a fee provided.
    pub async fn sign_withdraw(
        &self,
        amount: impl Into<BigUint>,
        fee: impl Into<BigUint>,
    ) -> Result<(ZkSyncTx, Option<PackedEthSignature>), ClientError> {
        self.inner
            .start_withdraw()
            .nonce(self.pending_nonce())
            .token(self.token_name.clone())?
            .amount(amount)
            .fee(fee)
            .to(self.inner.address())
            .tx()
            .await
    }

    // Creates a signed transfer tx to a given receiver.
    pub async fn sign_transfer(
        &self,
        to: impl Into<Address>,
        amount: impl Into<BigUint>,
        fee: BigUint,
    ) -> Result<(ZkSyncTx, Option<PackedEthSignature>), ClientError> {
        self.inner
            .start_transfer()
            .nonce(self.pending_nonce())
            .token(self.token_name.clone())?
            .amount(amount)
            .fee(fee)
            .to(to.into())
            .tx()
            .await
    }

    // Deposits tokens from Ethereum to the contract.
    pub async fn deposit(&self, amount: impl Into<BigUint>) -> anyhow::Result<PriorityOp> {
        let eth_tx_hash = self
            .eth_provider
            .deposit(
                self.token_name.clone(),
                biguint_to_u256(amount.into()),
                self.address(),
            )
            .await?;
        println!("{:?}", eth_tx_hash);

        self.monitor
            .get_priority_op(&self.eth_provider, eth_tx_hash)
            .await
    }

    // Performs a full exit operation.
    pub async fn full_exit(&self) -> anyhow::Result<PriorityOp> {
        let eth_tx_hash = self
            .eth_provider
            .full_exit(
                self.token_name.clone(),
                self.account_id()
                    .expect("An attempt to perform full exit on a wallet without account_id."),
            )
            .await?;

        self.monitor
            .get_priority_op(&self.eth_provider, eth_tx_hash)
            .await
    }

    /// Returns an underlying wallet.
    pub fn into_inner(self) -> Wallet<PrivateKeySigner, RpcProvider> {
        self.inner
    }

    /// Sends a transaction to ERC20 token contract to approve the ERC20 deposit.
    pub async fn approve_erc20_deposits(&self) -> anyhow::Result<()> {
        let tx_hash = self
            .eth_provider
            .approve_erc20_token_deposits(self.token_name.clone())
            .await?;
        self.eth_provider.wait_for_tx(tx_hash).await?;

        Ok(())
    }

    /// Sends a some amount tokens to the given address in the Ethereum network.
    pub async fn transfer_to(
        &self,
        token: impl Into<TokenLike>,
        amount: impl Into<BigUint>,
        to: Address,
    ) -> anyhow::Result<()> {
        let tx_hash = self
            .eth_provider
            .transfer(token, biguint_to_u256(amount.into()), to)
            .await?;
        self.eth_provider.wait_for_tx(tx_hash).await?;

        Ok(())
    }

    /// Returns appropriate nonce for the new transaction and increments the nonce.
    fn pending_nonce(&self) -> Nonce {
        Nonce(self.nonce.fetch_add(1, Ordering::SeqCst))
    }
}

fn gen_random_eth_private_key() -> H256 {
    let mut eth_private_key = H256::default();
    eth_private_key.randomize();
    eth_private_key
}
