/*
	Copyright 2021 Integritee AG and Supercomputing Systems AG

	Licensed under the Apache License, Version 2.0 (the "License");
	you may not use this file except in compliance with the License.
	You may obtain a copy of the License at

		http://www.apache.org/licenses/LICENSE-2.0

	Unless required by applicable law or agreed to in writing, software
	distributed under the License is distributed on an "AS IS" BASIS,
	WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
	See the License for the specific language governing permissions and
	limitations under the License.

*/

use crate::{error::Result, hex, io, utils::UnwrapOrSgxErrorUnexpected};
use base58::{FromBase58, ToBase58};
use codec::{Decode, Encode};
use ita_stf::{ShardIdentifier, State as StfState, StateType as StfStateType, Stf};
use itp_settings::files::{ENCRYPTED_STATE_FILE, SHARDS_PATH};
use itp_sgx_crypto::{AesSeal, StateCrypto};
use itp_sgx_io::SealedIO;
use log::*;
use sgx_tcrypto::rsgx_sha256_slice;
use sgx_types::*;
use sp_core::H256;
use std::{fs, io::Write, path::Path, vec::Vec};

/// Facade for handling STF state from file
pub trait HandleState {
	/// Load the STF state for a specific shard
	fn load(&self, shard: &ShardIdentifier) -> Result<StfState>;

	/// Writes the state (without the state diff) encrypted into the enclave
	///
	/// Returns the hash of the saved state (independent of the diff!)
	fn write(&mut self, state: StfState, shard: ShardIdentifier) -> Result<H256>;

	/// Query whether a given shard exists
	fn exists(&self, shard: &ShardIdentifier) -> bool;

	/// Initialize a shard with a given identifier
	fn init_shard(&mut self, shard: &ShardIdentifier) -> Result<()>;

	/// List all available shards
	fn list_shards(&self) -> Result<Vec<ShardIdentifier>>;
}

pub struct StateFacade;

impl HandleState for StateFacade {
	fn load(&self, shard: &ShardIdentifier) -> Result<StfState> {
		load(shard)
	}

	fn write(&mut self, state: StfState, shard: ShardIdentifier) -> Result<H256> {
		write(state, &shard)
	}

	fn exists(&self, shard: &ShardIdentifier) -> bool {
		exists(shard)
	}

	fn init_shard(&mut self, shard: &ShardIdentifier) -> Result<()> {
		init_shard(shard)
	}

	fn list_shards(&self) -> Result<Vec<ShardIdentifier>> {
		list_shards()
	}
}

pub fn load_initialized_state(shard: &H256) -> SgxResult<StfState> {
	trace!("Loading state from shard {:?}", shard);
	let state = if exists(&shard) {
		load(&shard)?
	} else {
		init_shard(&shard)?;
		Stf::init_state()
	};
	trace!("Sucessfully loaded or initialized state from shard {:?}", shard);
	Ok(state)
}

pub fn load(shard: &ShardIdentifier) -> Result<StfState> {
	// load last state
	let state_path =
		format!("{}/{}/{}", SHARDS_PATH, shard.encode().to_base58(), ENCRYPTED_STATE_FILE);
	trace!("loading state from: {}", state_path);
	let state_vec = read(&state_path)?;

	// state is now decrypted!
	let state: StfStateType = match state_vec.len() {
		0 => {
			debug!("state at {} is empty. will initialize it.", state_path);
			Stf::init_state().state
		},
		n => {
			debug!("State loaded from {} with size {}B, deserializing...", state_path, n);
			StfStateType::decode(&mut state_vec.as_slice())?
		},
	};
	trace!("state decoded successfully");
	// add empty state-diff
	let state_with_diff = StfState { state, state_diff: Default::default() };
	trace!("New state created: {:?}", state_with_diff);
	Ok(state_with_diff)
}

/// Writes the state (without the state diff) encrypted into the enclave storage
/// Returns the hash of the saved state (independent of the diff!)
pub fn write(state: StfState, shard: &ShardIdentifier) -> Result<H256> {
	let state_path =
		format!("{}/{}/{}", SHARDS_PATH, shard.encode().to_base58(), ENCRYPTED_STATE_FILE);
	trace!("writing state to: {}", state_path);

	// only save the state, the state diff is pruned
	let cyphertext = encrypt(state.state.encode())?;

	let state_hash = rsgx_sha256_slice(&cyphertext)?;

	debug!(
		"new encrypted state with hash=0x{} written to {}",
		hex::encode_hex(&state_hash),
		state_path
	);

	io::write(&cyphertext, &state_path)?;
	Ok(state_hash.into())
}

pub fn exists(shard: &ShardIdentifier) -> bool {
	Path::new(&format!("{}/{}/{}", SHARDS_PATH, shard.encode().to_base58(), ENCRYPTED_STATE_FILE))
		.exists()
}

pub fn init_shard(shard: &ShardIdentifier) -> Result<()> {
	let path = format!("{}/{}", SHARDS_PATH, shard.encode().to_base58());
	fs::create_dir_all(path.clone()).sgx_error()?;
	let mut file = fs::File::create(format!("{}/{}", path, ENCRYPTED_STATE_FILE)).sgx_error()?;
	Ok(file.write_all(b"")?)
}

fn read(path: &str) -> Result<Vec<u8>> {
	let mut bytes = io::read(path)?;

	if bytes.is_empty() {
		return Ok(bytes)
	}

	let state_hash = rsgx_sha256_slice(&bytes)?;
	debug!("read encrypted state with hash 0x{} from {}", hex::encode_hex(&state_hash), path);

	AesSeal::unseal().map(|key| key.decrypt(&mut bytes))??;
	trace!("buffer decrypted = {:?}", bytes);

	Ok(bytes)
}

#[allow(unused)]
fn write_encrypted(bytes: &mut Vec<u8>, path: &str) -> Result<sgx_status_t> {
	debug!("plaintext data to be written: {:?}", bytes);
	AesSeal::unseal().map(|key| key.encrypt(bytes))?;
	io::write(&bytes, path)?;
	Ok(sgx_status_t::SGX_SUCCESS)
}

fn encrypt(mut state: Vec<u8>) -> Result<Vec<u8>> {
	AesSeal::unseal().map(|key| key.encrypt(&mut state))??;
	Ok(state)
}

pub fn list_shards() -> Result<Vec<ShardIdentifier>> {
	let files = match fs::read_dir(SHARDS_PATH).sgx_error() {
		Ok(f) => f,
		Err(_) => return Ok(Vec::new()),
	};
	let mut shards = Vec::new();
	for file in files {
		let s = file
			.sgx_error()?
			.file_name()
			.into_string()
			.sgx_error()?
			.from_base58()
			.sgx_error()?;
		shards.push(ShardIdentifier::decode(&mut s.as_slice()).sgx_error()?);
	}
	Ok(shards)
}

//  tests
#[cfg(feature = "test")]
pub mod tests {
	use super::*;
	use crate::tests::ensure_no_empty_shard_directory_exists;
	use sgx_externalities::SgxExternalitiesTrait;

	// Fixme: Move this test to sgx-runtime:
	//
	// https://github.com/integritee-network/sgx-runtime/issues/23
	pub fn test_sgx_state_decode_encode_works() {
		// given
		let key: Vec<u8> = "hello".encode();
		let value: Vec<u8> = "world".encode();
		let mut state = StfState::new();
		state.insert(key, value);

		// when
		let encoded_state = state.state.encode();
		let state2 = StfStateType::decode(&mut encoded_state.as_slice()).unwrap();

		// then
		assert_eq!(state.state, state2);
	}

	pub fn test_encrypt_decrypt_state_type_works() {
		// given
		let key: Vec<u8> = "hello".encode();
		let value: Vec<u8> = "world".encode();
		let mut state = StfState::new();
		state.insert(key, value);

		// when
		let encrypted = encrypt(state.state.encode()).unwrap();

		let decrypted = encrypt(encrypted).unwrap();
		let decoded = StfStateType::decode(&mut decrypted.as_slice()).unwrap();

		// then
		assert_eq!(state.state, decoded);
	}

	pub fn test_write_and_load_state_works() {
		// given
		ensure_no_empty_shard_directory_exists();

		let key: Vec<u8> = "hello".encode();
		let value: Vec<u8> = "world".encode();
		let mut state = StfState::new();
		let shard: ShardIdentifier = [94u8; 32].into();
		state.insert(key, value);

		// when
		if !exists(&shard) {
			init_shard(&shard).unwrap();
		}
		let _hash = write(state.clone(), &shard).unwrap();
		let result = load(&shard).unwrap();

		// then
		assert_eq!(state.state, result.state);

		// clean up
		remove_shard_dir(&shard);
	}

	pub fn remove_shard_dir(shard: &ShardIdentifier) {
		std::fs::remove_dir_all(&format!("{}/{}", SHARDS_PATH, shard.encode().to_base58()))
			.unwrap();
	}
}