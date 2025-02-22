// Copyright (c) 2022 MASSA LABS <info@massa.net>
//! Standalone massa wallet
//! Keypair management
#![warn(missing_docs)]
#![warn(unused_crate_dependencies)]

pub use error::WalletError;

use massa_cipher::{decrypt, encrypt, CipherData, Salt};
use massa_hash::Hash;
use massa_models::address::Address;
use massa_models::composite::PubkeySig;
use massa_models::operation::{Operation, OperationSerializer, SecureShareOperation};
use massa_models::prehash::{PreHashMap, PreHashSet};
use massa_models::secure_share::SecureShareContent;
use massa_signature::{KeyPair, PublicKey};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;

mod error;

/// Contains the keypairs created in the wallet.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Wallet {
    /// Keypairs and addresses
    pub keys: PreHashMap<Address, KeyPair>,
    /// Path to the file containing the keypairs (encrypted)
    wallet_path: PathBuf,
    /// Password
    password: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
/// Follow the standard: https://github.com/massalabs/massa-standards/blob/main/wallet/file-format.md
struct WalletFileFormat {
    version: u64,
    nickname: String,
    address: String,
    salt: Salt,
    nonce: [u8; 12],
    ciphered_data: Vec<u8>,
    public_key: Vec<u8>,
}

impl Wallet {
    /// Generates a new wallet initialized with the provided file content
    pub fn new(path: PathBuf, password: String) -> Result<Wallet, WalletError> {
        if path.is_dir() {
            let mut keys = PreHashMap::default();
            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    let content = &std::fs::read(&path)?[..];
                    let wallet = serde_yaml::from_slice::<WalletFileFormat>(content)?;
                    let secret_key = decrypt(
                        &password,
                        CipherData {
                            salt: wallet.salt,
                            nonce: wallet.nonce,
                            encrypted_bytes: wallet.ciphered_data,
                        },
                    )?;
                    keys.insert(
                        Address::from_str(&wallet.address)?,
                        KeyPair::from_bytes(&secret_key)?,
                    );
                }
            }
            Ok(Wallet {
                keys,
                wallet_path: path,
                password,
            })
        } else {
            let wallet = Wallet {
                keys: PreHashMap::default(),
                wallet_path: path,
                password,
            };
            wallet.save()?;
            Ok(wallet)
        }
    }

    /// Sign arbitrary message with the associated keypair
    /// returns none if the address isn't in the wallet or if an error occurred during the signature
    /// else returns the public key that signed the message and the signature
    pub fn sign_message(&self, address: &Address, msg: Vec<u8>) -> Option<PubkeySig> {
        if let Some(key) = self.keys.get(address) {
            if let Ok(signature) = key.sign(&Hash::compute_from(&msg)) {
                Some(PubkeySig {
                    public_key: key.get_public_key(),
                    signature,
                })
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Adds a list of keypairs to the wallet, returns their addresses.
    /// The wallet file is updated.
    pub fn add_keypairs(&mut self, keys: Vec<KeyPair>) -> Result<Vec<Address>, WalletError> {
        let mut changed = false;
        let mut addrs = Vec::with_capacity(keys.len());
        for key in keys {
            let addr = Address::from_public_key(&key.get_public_key());
            if let Entry::Vacant(e) = self.keys.entry(addr) {
                e.insert(key);
                changed = true;
            }
            addrs.push(addr);
        }
        if changed {
            self.save()?;
        }
        Ok(addrs)
    }

    /// Removes wallet entries given a list of addresses. Missing entries are ignored.
    /// call save() to persist the changes on disk.
    pub fn remove_addresses(&mut self, addresses: &Vec<Address>) -> Result<bool, WalletError> {
        let mut changed = false;
        for address in addresses {
            if self.keys.remove(address).is_some() {
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Finds the keypair associated with given address
    pub fn find_associated_keypair(&self, address: &Address) -> Option<&KeyPair> {
        self.keys.get(address)
    }

    /// Finds the public key associated with given address
    pub fn find_associated_public_key(&self, address: &Address) -> Option<PublicKey> {
        self.keys
            .get(address)
            .map(|keypair| keypair.get_public_key())
    }

    /// Get all addresses in the wallet
    pub fn get_wallet_address_list(&self) -> PreHashSet<Address> {
        self.keys.keys().copied().collect()
    }

    /// Save the wallets in a directory, each wallet in a yaml file.
    pub fn save(&self) -> Result<(), WalletError> {
        let mut existing_keys: HashSet<PathBuf> = HashSet::new();
        if !self.wallet_path.exists() {
            std::fs::create_dir_all(&self.wallet_path)?;
        } else {
            let read_dir = std::fs::read_dir(&self.wallet_path)?;
            for path in read_dir {
                existing_keys.insert(path?.path());
            }
        }
        let mut persisted_keys: HashSet<PathBuf> = HashSet::new();
        // write the keys in the directory
        for (addr, keypair) in &self.keys {
            let encrypted_secret = encrypt(&self.password, &keypair.to_bytes())?;
            let file_formatted = WalletFileFormat {
                version: keypair.get_version(),
                nickname: addr.to_string(),
                address: addr.to_string(),
                salt: encrypted_secret.salt,
                nonce: encrypted_secret.nonce,
                ciphered_data: encrypted_secret.encrypted_bytes,
                public_key: keypair.get_public_key().to_bytes().to_vec(),
            };
            let ser_keys = serde_yaml::to_string(&file_formatted)?;
            let file_path = self.wallet_path.join(format!("wallet_{}.yaml", addr));

            std::fs::write(&file_path, ser_keys)?;
            persisted_keys.insert(file_path);
        }

        let to_remove = existing_keys.difference(&persisted_keys);
        for path in to_remove {
            std::fs::remove_file(path)?;
        }

        Ok(())
    }

    /// Export keys and addresses
    pub fn get_full_wallet(&self) -> &PreHashMap<Address, KeyPair> {
        &self.keys
    }

    /// Signs an operation with the keypair corresponding to the given address
    pub fn create_operation(
        &self,
        content: Operation,
        address: Address,
    ) -> Result<SecureShareOperation, WalletError> {
        let sender_keypair = self
            .find_associated_keypair(&address)
            .ok_or_else(|| WalletError::MissingKeyError(address))?;
        Ok(Operation::new_verifiable(content, OperationSerializer::new(), sender_keypair).unwrap())
    }
}

impl std::fmt::Display for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        writeln!(f)?;
        for (addr, keypair) in &self.keys {
            writeln!(f, "Secret key: {}", keypair)?;
            writeln!(f, "Public key: {}", keypair.get_public_key())?;
            writeln!(f, "Address: {}", addr)?;
        }
        Ok(())
    }
}

/// Test utils
#[cfg(feature = "testing")]
pub mod test_exports;
