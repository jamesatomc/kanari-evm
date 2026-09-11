// Copyright (c) KanariNetwork, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Validator committee files, kanari-sdk-style.
//!
//! `generate_committee` (the `keygen` CLI command, mirroring
//! `kanari-node consensus-keygen`) writes one shared committee file plus one
//! secret-key file per validator:
//!
//! ```text
//! <out-dir>/
//! ├── dag-committee.json        — validator ids, DAG socket addresses, hex pubkeys
//! ├── validator-1.key           — opaque Ed25519 secret (DO NOT SHARE)
//! └── validator-2.key
//! ```
//!
//! Authority ids are 1-based (`0x1`, `0x2`, …) like kanari-sdk; internally
//! they map to 0-based Mysticeti [`Authority`] indices. A validator loads
//! the committee, finds its own entry by matching the public key derived
//! from its secret file, and gets everything needed to join the DAG mesh
//! (no separate bootstrap flag: every validator dials every other one,
//! exactly like Mysticeti's `Network::load` full mesh).

use dag::{
    authority::Authority,
    committee::{AuthorityInfo, Committee},
    crypto::{PublicKey, Signer},
};
use rand::{SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

/// Default committee filename inside a keygen output directory.
pub const COMMITTEE_FILENAME: &str = "dag-committee.json";

/// One validator's public committee entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorEntry {
    /// 1-based authority id, e.g. `"0x1"` (kanari-sdk convention).
    pub id: String,
    /// TCP address of this validator's DAG mesh listener.
    pub dag_address: SocketAddr,
    /// Ed25519 public key, lowercase hex (32 bytes).
    pub public_key: String,
}

/// Shared committee file: every validator holds the same copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagCommittee {
    pub authorities: Vec<ValidatorEntry>,
}

/// A validator's resolved identity: committee position, address and keys.
pub struct LoadedValidator {
    /// 0-based DAG authority index.
    pub index: usize,
    /// 1-based authority id string (e.g. `"0x1"`).
    pub id: String,
    pub authority: Authority,
    pub signer: Signer,
    pub committee: Arc<Committee>,
    /// DAG addresses of all validators in authority-index order.
    pub dag_addresses: Vec<SocketAddr>,
    /// This validator's own DAG listen address.
    pub own_address: SocketAddr,
}

/// Committee file errors.
#[derive(Debug, thiserror::Error)]
pub enum CommitteeError {
    #[error("io error: {0}")]
    Io(String),
    #[error("invalid committee file: {0}")]
    Invalid(String),
}

impl DagCommittee {
    /// Number of validators.
    pub fn len(&self) -> usize {
        self.authorities.len()
    }

    /// True when no validators are listed.
    pub fn is_empty(&self) -> bool {
        self.authorities.is_empty()
    }

    /// Build the Mysticeti committee (equal stake per validator).
    pub fn to_mysticeti(&self) -> Result<Arc<Committee>, CommitteeError> {
        if self.authorities.len() < 4 {
            return Err(CommitteeError::Invalid(format!(
                "need at least 4 validators for quorum, got {}",
                self.authorities.len()
            )));
        }
        let mut infos = Vec::with_capacity(self.authorities.len());
        for entry in &self.authorities {
            let key = decode_pubkey(&entry.public_key).map_err(|e| {
                CommitteeError::Invalid(format!("bad public key for {}: {e}", entry.id))
            })?;
            infos.push(AuthorityInfo::new(1, key));
        }
        Ok(Committee::new(infos))
    }
}

/// Generate a fresh committee: `node_count` validators on `host` starting at
/// `base_dag_port` (10-port stride: 3500, 3510, …), writing
/// `dag-committee.json` plus one `validator-{i}.key` per validator into
/// `out_dir` (created when missing).
///
/// Ports must satisfy `(base + (count-1) * 10) * 10 <= 65535`: Mysticeti's
/// TCP mesh dials out from source port `listen * 10`, so listen ports above
/// ~6553 overflow and fail to bind.
pub fn generate_committee(
    node_count: usize,
    host: IpAddr,
    base_dag_port: u16,
    out_dir: &Path,
) -> Result<DagCommittee, CommitteeError> {
    if node_count < 4 {
        return Err(CommitteeError::Invalid(format!(
            "need at least 4 validators for quorum, got {node_count}"
        )));
    }
    let top = (base_dag_port as u32)
        .saturating_add((node_count as u32).saturating_sub(1).saturating_mul(10));
    if top.saturating_mul(10) > 65535 {
        return Err(CommitteeError::Invalid(format!(
            "dag ports {base_dag_port}..={top} overflow the mesh source-port range (listen * 10 must fit u16); use a base port below ~{}",
            6553u32.saturating_sub((node_count as u32).saturating_sub(1).saturating_mul(10)),
        )));
    }
    std::fs::create_dir_all(out_dir).map_err(|e| CommitteeError::Io(e.to_string()))?;
    let mut rng = StdRng::from_entropy();
    let mut authorities = Vec::with_capacity(node_count);
    for i in 0..node_count {
        let signer = Signer::new(&mut rng);
        let id = format!("0x{}", i + 1);
        let port = base_dag_port
            .checked_add((i as u16).saturating_mul(10))
            .ok_or_else(|| CommitteeError::Invalid("dag port range overflow".to_string()))?;
        authorities.push(ValidatorEntry {
            id: id.clone(),
            dag_address: SocketAddr::new(host, port),
            public_key: encode_pubkey(&signer.public_key())?,
        });
        let key_path = out_dir.join(format!("validator-{}.key", i + 1));
        let key_json =
            serde_json::to_string_pretty(&signer).map_err(|e| CommitteeError::Io(e.to_string()))?;
        std::fs::write(&key_path, key_json).map_err(|e| CommitteeError::Io(e.to_string()))?;
        // Re-load the secret immediately so a bad write fails fast here,
        // not when the validator boots.
        let _ = load_signer(&key_path)?;
    }
    let committee = DagCommittee { authorities };
    let committee_json = serde_json::to_string_pretty(&committee)
        .map_err(|e| CommitteeError::Io(e.to_string()))?;
    std::fs::write(out_dir.join(COMMITTEE_FILENAME), committee_json)
        .map_err(|e| CommitteeError::Io(e.to_string()))?;
    Ok(committee)
}

/// Load a validator identity: parse the committee, parse the secret key,
/// and find our own entry by public-key match.
pub fn load_validator(
    committee_path: &Path,
    key_path: &Path,
) -> Result<LoadedValidator, CommitteeError> {
    let raw =
        std::fs::read(committee_path).map_err(|e| CommitteeError::Io(e.to_string()))?;
    let committee: DagCommittee =
        serde_json::from_slice(&raw).map_err(|e| CommitteeError::Invalid(e.to_string()))?;
    let signer = load_signer(key_path)?;
    let own_pubkey = encode_pubkey(&signer.public_key())?;
    let index = committee
        .authorities
        .iter()
        .position(|entry| entry.public_key == own_pubkey)
        .ok_or_else(|| {
            CommitteeError::Invalid(format!(
                "key {} matches no validator in {}",
                key_path.display(),
                committee_path.display()
            ))
        })?;
    let entry = &committee.authorities[index];
    Ok(LoadedValidator {
        index,
        id: entry.id.clone(),
        authority: Authority::new(index as u64),
        signer,
        committee: committee.to_mysticeti()?,
        dag_addresses: committee.authorities.iter().map(|e| e.dag_address).collect(),
        own_address: entry.dag_address,
    })
}

/// Load an Ed25519 DAG signer from a `validator-{i}.key` file.
pub fn load_signer(path: &Path) -> Result<Signer, CommitteeError> {
    let raw = std::fs::read(path).map_err(|e| CommitteeError::Io(e.to_string()))?;
    serde_json::from_slice(&raw).map_err(|e| CommitteeError::Invalid(e.to_string()))
}

/// Compatibility helper: kanari-sdk-style public-keys map
/// (`{"0x1": "<hex>", …}`) for operators migrating existing key material.
pub fn public_keys_map(committee: &DagCommittee) -> BTreeMap<String, String> {
    committee
        .authorities
        .iter()
        .map(|entry| (entry.id.clone(), entry.public_key.clone()))
        .collect()
}

/// Default secret-key filename for a 1-based validator number.
pub fn key_filename(validator_number: usize) -> PathBuf {
    PathBuf::from(format!("validator-{validator_number}.key"))
}

fn encode_pubkey(key: &PublicKey) -> Result<String, CommitteeError> {
    let value =
        serde_json::to_value(key).map_err(|e| CommitteeError::Invalid(e.to_string()))?;
    let bytes: Vec<u8> =
        serde_json::from_value(value).map_err(|e| CommitteeError::Invalid(e.to_string()))?;
    if bytes.len() != 32 {
        return Err(CommitteeError::Invalid(format!(
            "public key must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    Ok(hex_of(&bytes))
}

fn decode_pubkey(hex: &str) -> Result<PublicKey, String> {
    let hex = hex.trim().trim_start_matches("0x");
    if hex.len() != 64 {
        return Err("want 32-byte hex".to_string());
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|_| "non-hex key".to_string())?;
        bytes[i] = u8::from_str_radix(s, 16).map_err(|_| "non-hex key".to_string())?;
    }
    PublicKey::from_bytes(bytes).map_err(|e| e.to_string())
}

fn hex_of(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(CHARS[(b >> 4) as usize] as char);
        out.push(CHARS[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_roundtrip_and_layout() {
        let dir = std::env::temp_dir().join(format!(
            "kanari-evm-committee-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let committee = generate_committee(4, IpAddr::from([127, 0, 0, 1]), 3500, &dir)
            .expect("keygen works");
        assert_eq!(committee.len(), 4);
        assert!(dir.join(COMMITTEE_FILENAME).is_file());
        // 10-port stride, 1-based ids.
        assert_eq!(committee.authorities[0].dag_address.port(), 3500);
        assert_eq!(committee.authorities[3].dag_address.port(), 3530);
        assert_eq!(committee.authorities[0].id, "0x1");
        // Every key file resolves to its own committee entry.
        for (i, entry) in committee.authorities.iter().enumerate() {
            let loaded = load_validator(
                &dir.join(COMMITTEE_FILENAME),
                &dir.join(key_filename(i + 1)),
            )
            .expect("key loads");
            assert_eq!(loaded.index, i);
            assert_eq!(loaded.id, entry.id);
            assert_eq!(loaded.own_address, entry.dag_address);
            assert_eq!(loaded.dag_addresses.len(), 4);
        }
        // Mysticeti committee builds with quorum for 4 equal-stake validators.
        let mysticeti = committee.to_mysticeti().expect("mysticeti committee");
        assert_eq!(mysticeti.len(), 4);
        assert_eq!(mysticeti.total_stake(), 4);
        assert!(mysticeti.known_authority(Authority::new(0)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keygen_rejects_small_committees() {
        let dir = std::env::temp_dir().join(format!(
            "kanari-evm-committee-small-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let err = generate_committee(3, IpAddr::from([127, 0, 0, 1]), 3500, &dir)
            .expect_err("3 validators must be rejected");
        assert!(err.to_string().contains("quorum"));
    }

    #[test]
    fn foreign_key_matches_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "kanari-evm-committee-foreign-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        generate_committee(4, IpAddr::from([127, 0, 0, 1]), 3500, &dir).expect("keygen");
        let mut rng = StdRng::from_entropy();
        let foreign = Signer::new(&mut rng);
        let foreign_path = dir.join("foreign.key");
        std::fs::write(&foreign_path, serde_json::to_string(&foreign).expect("json"))
            .expect("write");
        let err = match load_validator(&dir.join(COMMITTEE_FILENAME), &foreign_path) {
            Ok(_) => panic!("foreign key must not match"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("matches no validator"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
