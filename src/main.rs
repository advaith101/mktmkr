use ethers::{
    prelude::*,
    providers::{Provider, Ws, StreamExt},
    types::{Transaction, H256, U256, Bytes},
    utils::keccak256,
};
use std::time::{Duration, Instant};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;

const UNISWAP_V2_ROUTER: &str = "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D";
const WETH_ADDRESS: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

struct TradingBot {
    provider: Provider<Ws>,
    wallet: LocalWallet,
    token_address: Address,
    router: Address,
    weth: Address,
    sell_percentage: f64,
    target_eth: U256,
    expiry_time: Instant,
    total_sold: Arc<Mutex<U256>>,
}

impl TradingBot {
    async fn new(
        ws_url: &str,
        private_key: &str,
        token_address: &str,
        sell_percentage: f64,
        target_eth: U256,
        expiry_seconds: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let provider = Provider::<Ws>::connect(ws_url).await?;
        let wallet: LocalWallet = private_key.parse()?;
        let token_address = Address::from_str(token_address)?;
        let router = Address::from_str(UNISWAP_V2_ROUTER)?;
        let weth = Address::from_str(WETH_ADDRESS)?;
        let expiry_time = Instant::now() + Duration::from_secs(expiry_seconds);

        Ok(Self {
            provider,
            wallet,
            token_address,
            router,
            weth,
            sell_percentage,
            target_eth,
            expiry_time,
            total_sold: Arc::new(Mutex::new(U256::zero())),
        })
    }

    async fn monitor_mempool(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut pending_txs = self.provider.subscribe_pending_txs().await?;
        
        while let Some(tx_hash) = pending_txs.next().await {
            if Instant::now() >= self.expiry_time {
                break;
            }

            if let Some(tx) = self.provider.get_transaction(tx_hash).await? {
                if let Some(buy_amount) = self.is_token_buy(&tx) {
                    self.execute_sell(buy_amount).await?;
                }
            }
        }

        Ok(())
    }

    fn is_token_buy(&self, tx: &Transaction) -> Option<U256> {
        // Check if the transaction is to the Uniswap V2 Router
        if tx.to != Some(self.router) {
            return None;
        }

        // Function selector for swapExactETHForTokensSupportingFeeOnTransferTokens
        const SWAP_ETH_FOR_TOKENS: [u8; 4] = [0x7f, 0xf3, 0x6a, 0xb5];
        // Function selector for swapExactTokensForTokensSupportingFeeOnTransferTokens
        const SWAP_TOKENS_FOR_TOKENS: [u8; 4] = [0x38, 0xed, 0x17, 0x39];

        if tx.input.starts_with(&SWAP_ETH_FOR_TOKENS) || tx.input.starts_with(&SWAP_TOKENS_FOR_TOKENS) {
            // Check if our token is in the path (should be the last address)
            let path_offset = if tx.input.starts_with(&SWAP_ETH_FOR_TOKENS) { 164 } else { 196 };
            let path_length = U256::from_big_endian(&tx.input[path_offset..path_offset + 32]).as_usize();
            let last_token = Address::from_slice(&tx.input[tx.input.len() - 20..]);

            if last_token == self.token_address {
                return Some(tx.value);
            }
        }

        None
    }

    async fn execute_sell(&self, buy_amount: U256) -> Result<(), Box<dyn std::error::Error>> {
        let sell_amount = (buy_amount.as_u128() as f64 * self.sell_percentage / 100.0) as u128;
        let sell_amount = U256::from(sell_amount);

        let deadline = U256::from(Instant::now().elapsed().as_secs() + 300); // 5 minutes from now

        let swap_call = encode_function_data(
            "swapExactTokensForETHSupportingFeeOnTransferTokens",
            &[
                Token::Uint(sell_amount),
                Token::Uint(U256::zero()),  // We accept any amount of ETH
                Token::Array(vec![
                    Token::Address(self.token_address),
                    Token::Address(self.weth),
                ]),
                Token::Address(self.wallet.address()),
                Token::Uint(deadline),
            ],
        )?;

        let tx = TransactionRequest::new()
            .to(self.router)
            .data(swap_call)
            .from(self.wallet.address());

        let pending_tx = self.wallet.sign_transaction(&tx).await?;
        let receipt = self.provider.send_raw_transaction(pending_tx).await?;

        if let Some(receipt) = self.provider.get_transaction_receipt(receipt).await? {
            let mut total_sold = self.total_sold.lock().await;
            *total_sold += sell_amount;
            println!("Sold {} tokens. Total sold: {} ETH", sell_amount, *total_sold);

            if *total_sold >= self.target_eth {
                println!("Target reached: {} ETH sold", *total_sold);
            }
        }

        Ok(())
    }

    async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.monitor_mempool().await?;

        let total_sold = self.total_sold.lock().await;
        if *total_sold < self.target_eth {
            println!("Time expired. Total sold: {} ETH", *total_sold);
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ws_url = "wss://mainnet.infura.io/ws/v3/YOUR-PROJECT-ID";
    let private_key = "your_private_key_here";
    let token_address = "0x..."; // The address of the ERC-20 token you're trading
    let sell_percentage = 10.0;
    let target_eth = U256::from(100_000_000_000_000_000_000u128); // 100 ETH
    let expiry_seconds = 3600; // 1 hour

    let bot = TradingBot::new(
        ws_url,
        private_key,
        token_address,
        sell_percentage,
        target_eth,
        expiry_seconds,
    ).await?;

    bot.run().await?;

    Ok(())
}

fn encode_function_data(function_name: &str, tokens: &[Token]) -> Result<Bytes, Box<dyn std::error::Error>> {
    let function = ethers::abi::Function {
        name: function_name.to_string(),
        inputs: vec![
            ethers::abi::Param { name: "amountIn".to_string(), kind: ethers::abi::ParamType::Uint(256), internal_type: None },
            ethers::abi::Param { name: "amountOutMin".to_string(), kind: ethers::abi::ParamType::Uint(256), internal_type: None },
            ethers::abi::Param { name: "path".to_string(), kind: ethers::abi::ParamType::Array(Box::new(ethers::abi::ParamType::Address)), internal_type: None },
            ethers::abi::Param { name: "to".to_string(), kind: ethers::abi::ParamType::Address, internal_type: None },
            ethers::abi::Param { name: "deadline".to_string(), kind: ethers::abi::ParamType::Uint(256), internal_type: None },
        ],
        outputs: vec![],
        constant: None,
        state_mutability: ethers::abi::StateMutability::NonPayable,
    };

    let encoded = function.encode_input(tokens)?;
    Ok(encoded.into())
}