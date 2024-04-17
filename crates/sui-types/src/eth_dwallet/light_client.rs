use std::str::FromStr;

use anyhow::anyhow;
use ethers::prelude::*;
use ethers::prelude::H160;
use ethers::utils::keccak256;
use ethers::utils::rlp::RlpStream;
use eyre::{eyre, Report};
use helios::client::{Client, ClientBuilder, FileDB};
use helios::config::checkpoints;
use helios::config::networks::Network;
use helios::consensus::types::Bytes32;
// use helios::prelude::*;
use tracing::info;

use crate::eth_dwallet::config::EthClientConfig;
use crate::eth_dwallet::proof::{Proof, ProofResponse};
use crate::eth_dwallet::utils;
use crate::eth_dwallet::utils::is_empty_value;

pub struct EthLightClient {
    pub client: Client<FileDB>,
    config: EthClientConfig,
}

impl EthLightClient {
    pub async fn new(conf: EthClientConfig) -> Result<Self, anyhow::Error> {
        // todo(yuval): make sure it's set based on the net in sui (test = goerli, main = main)
        let network = conf.network;

        let client: Client<FileDB> = ClientBuilder::new()
            .network(network)
            .execution_rpc(&conf.execution_rpc)
            .consensus_rpc(&conf.consensus_rpc)
            .checkpoint(&conf.checkpoint)
            .data_dir("/tmp/helios".parse()?)
            .build()
            .map_err(|e| anyhow!("failed to create client: {}", e))?;

        info!("EthLightClient created");

        Ok(Self {
            client,
            config: conf,
        })
    }

    pub async fn start(&mut self) -> Result<(), anyhow::Error> {
        self.client.start().await.map_err(|e| anyhow!("failed to start client: {}", e))?;
        self.client.wait_synced().await;
        info!("EthLightClient connected");

        Ok(())
    }

    /// Get the Merkle Tree Proof (EIP1186Proof) for the client parameters.
    pub async fn get_proofs(&self, execution_state_root: &Bytes32) -> eyre::Result<ProofResponse, Report> {
        let message_map_index = self.get_mapping_index()?;

        // Fetch the latest block number to get the proof.
        let block_number = self.client.get_block_number().await?;
        let contract_addr = self.config.contract_addr.clone();

        let proof = self
            .client
            .get_proof(
                &Address::from_str(&contract_addr)?,
                &[message_map_index],
                block_number.as_u64(),
            )
            .await?;

        let account_path = keccak256(contract_addr.as_bytes()).to_vec();
        let account_encoded = encode_account(&proof);

        let account_proof = Proof {
            proof: proof.account_proof,
            root: execution_state_root.as_slice().to_vec(),
            path: account_path,
            value: account_encoded,
        };

        // The storage proof for the specific message and dWalletID in the mapping.
        let msg_storage_proof = proof
            .storage_proof
            .iter()
            // TODO(yuval): make sure conversion from H256 to U256 does not break proof logic.
            //  Conversion is made because of breaking changes introduced in ethers library.
            .find(|p| p.key == U256::from_big_endian(message_map_index.as_bytes()))
            .ok_or_else(|| eyre!("Storage proof not found"))?;

        // 1 for True (if the message is approved,
        // the value in the contract's storage map would be True)
        let storage_value = [1].to_vec();
        // todo(zeev): no urgent, but need to check the relation to to proof.
        let mut msg_storage_proof_key_bytes = [0u8; 32];
        msg_storage_proof
            .key
            .to_big_endian(&mut msg_storage_proof_key_bytes);
        let storage_key_hash = keccak256(msg_storage_proof_key_bytes);

        // Validate value is not empty because we are looking for inclusion.
        if is_empty_value(&storage_value) {
            return Err(eyre!("Storage value is empty"));
        };

        let storage_proof = Proof {
            proof: msg_storage_proof.clone().proof,
            root: proof.storage_hash.as_bytes().to_vec(),
            path: storage_key_hash.to_vec(),
            value: storage_value,
        };

        Ok(ProofResponse {
            account_proof,
            storage_proof,
            execution_state_root: execution_state_root.clone(),
        })
    }

    fn get_mapping_index(&self) -> Result<H256, Report> {
        let decoded_msg = hex::decode(&self.config.message[2..])?;

        // Calculate memory slot.
        // Each mapping slot is calculated by concatenating of the msg and dWalletID.
        let key = utils::calculate_key(decoded_msg, H512::from_slice(self.config.dwallet_id.as_slice()));
        Ok(utils::calculate_mapping_slot(key, self.config.data_slot))
    }

    pub async fn get_block_number(&self) -> Result<u64, Report> {
        Ok(self.client.get_block_number().await?.as_u64())
    }
}

/// Fetch the latest checkpoint
/// More info here:
/// https://www.ledger.com/academy/ethereum-proof-of-stake-pos-explained#:~:text=Under%20Proof%20of%20Stake%20(PoS,6.4%20minutes)%20is%20a%20checkpoint.
async fn fetch_latest_checkpoint(network: Network) -> Result<String, anyhow::Error> {
    let checkpoint_fb = checkpoints::CheckpointFallback::new().build().await
        .map_err(|e| anyhow!("failed to create checkpoint fallback: {}", e))?;
    let checkpoint = checkpoint_fb.fetch_latest_checkpoint(&network).await
        .map_err(|e| anyhow!("failed to fetch latest checkpoint from fallback services: {}", e))?;
    info!("fetched latest Ethereum `{network}` checkpoint: `{checkpoint}`");
    Ok(checkpoint.to_string())
}

pub fn encode_account(proof: &EIP1186ProofResponse) -> Vec<u8> {
    let mut stream = RlpStream::new_list(4);
    stream.append(&proof.nonce);
    stream.append(&proof.balance);
    stream.append(&proof.storage_hash);
    stream.append(&proof.code_hash);
    let encoded = stream.out();
    encoded.to_vec()
}