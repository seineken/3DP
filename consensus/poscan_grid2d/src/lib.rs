use std::sync::Arc;
use parity_scale_codec::{Decode, Encode};
use sc_consensus_poscan::{Error, PoscanData, PowAlgorithm};
use sha3::{Digest, Sha3_256};
use sp_blockchain::HeaderBackend;
use sp_api::ProvideRuntimeApi;
use sp_consensus_poscan::{Seal as RawSeal, SCALE_DIFF_SINCE, SCALE_DIFF_BY};
use sp_consensus_poscan::DifficultyApi;
use sp_core::{H256, U256, crypto::Pair, hashing::blake2_256, ByteArray};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::Block as BlockT;
// use frame_support::sp_runtime::print as prn;
// use frame_support::runtime_print;
use sc_consensus_poscan::app;
use sp_consensus_poscan::POSCAN_ALGO_GRID2D_V3_1;
use poscan_algo::get_obj_hashes;
use log::*;

/// Determine whether the given hash satisfies the given difficulty.
/// The test is done by multiplying the two together. If the product
/// overflows the bounds of U256, then the product (and thus the hash)
/// was too high.
pub fn hash_meets_difficulty(hash: &H256, difficulty: U256) -> bool {
	let num_hash = U256::from(&hash[..]);
	let (_, overflowed) = num_hash.overflowing_mul(difficulty);

	!overflowed
}

/// A Seal struct that will be encoded to a Vec<u8> as used as the
/// `RawSeal` type.
#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct Seal {
	pub difficulty: U256,
	pub work: H256,
	// pub nonce: U256,
	pub poscan_hash: H256,
	pub signature: app::Signature,
}

/// A not-yet-computed attempt to solve the proof of work. Calling the
/// compute method will compute the hash and return the seal.
#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct Compute {
	pub difficulty: U256,
	pub pre_hash: H256,
	// pub nonce: U256,
	pub poscan_hash: H256,
}

impl Compute {
	// pub fn compute(self, ) -> Seal {
	// 	let work = H256::from_slice(Sha3_256::digest(&self.encode()[..]).as_slice());
	//
	// 	Seal {
	// 		difficulty: self.difficulty,
	// 		work,
	// 		poscan_hash: self.poscan_hash,
	//
	// 	}
	// }

	pub fn seal(&self, signature: app::Signature) -> Seal {
		let work = H256::from_slice(Sha3_256::digest(&self.encode()[..]).as_slice());

		Seal {
			difficulty: self.difficulty,
			work,
			poscan_hash: self.poscan_hash,
			signature,
		}
	}

	pub fn get_work(&self) -> H256 {
		H256::from_slice(Sha3_256::digest(&self.encode()[..]).as_slice())
	}

	fn signing_message(&self) -> [u8; 32] {
		let calculation = Self {
			difficulty: self.difficulty,
			pre_hash: self.pre_hash,
			poscan_hash: self.poscan_hash,
		};

		blake2_256(&calculation.encode()[..])
	}

	pub fn sign(&self, pair: &app::Pair) -> app::Signature {
		let hash = self.signing_message();
		pair.sign(&hash[..])
	}

	pub fn verify(
		&self,
		signature: &app::Signature,
		public: &app::Public,
	) -> bool {
		let hash = self.signing_message();
		app::Pair::verify(
			signature,
			&hash[..],
			public,
		)
	}

}

#[derive(Clone, PartialEq, Eq, Encode, Decode, Debug)]
pub struct DoubleHash {
	pub pre_hash: H256,
	pub obj_hash: H256,
}

impl DoubleHash {
	pub fn calc_hash(self) -> H256 {
		H256::from_slice(Sha3_256::digest(&self.encode()[..]).as_slice())
	}
}

pub struct PoscanAlgorithm<C> {
	client: Arc<C>,
}

impl<C> PoscanAlgorithm<C> {
	pub fn new(client: Arc<C>) -> Self {
		Self { client }
	}
}

impl<C> Clone for PoscanAlgorithm<C> {
	fn clone(&self) -> Self {
		Self::new(self.client.clone())
	}
}

impl<B: BlockT<Hash = H256>, C> PowAlgorithm<B> for PoscanAlgorithm<C>
where
	C: ProvideRuntimeApi<B> + HeaderBackend<B>,
	C::Api: DifficultyApi<B, U256>,
{
	type Difficulty = U256;

	fn difficulty(&self, parent: B::Hash) -> Result<Self::Difficulty, Error<B>> {
		let parent_id = BlockId::<B>::hash(parent);
		let parent_num = self.client.block_number_from_id(&parent_id).unwrap().unwrap();
		self.client
			.runtime_api()
			.difficulty(&parent_id)
			.map(|d| if parent_num >= SCALE_DIFF_SINCE.into() { d / SCALE_DIFF_BY } else { d })
			.map_err(|err| {
				sc_consensus_poscan::Error::Environment(format!(
					"Fetching difficulty from runtime failed: {:?}",
					err
				))
			})
	}

	fn verify(
		&self,
		parent: &H256,
		pre_hash: &H256,
		pre_digest: Option<&[u8]>,
		seal: &RawSeal,
		difficulty: Self::Difficulty,
		poscan_data: &PoscanData,
	) -> Result<bool, Error<B>> {
		// Try to construct a seal object by decoding the raw seal given
		let seal = match Seal::decode(&mut &seal[..]) {
			Ok(seal) => seal,
			Err(_) => {
				info!(">>> verify: no seal");
				return Ok(false)
			},
		};

		// See whether the hash meets the difficulty requirement. If not, fail fast.
		if !hash_meets_difficulty(&seal.work, difficulty) {
			info!(">>> verify: hash_meets_difficulty - false");
			info!(">>> work:{} poscan_hash:{} difficulty: {}", &seal.work, &seal.poscan_hash, difficulty);
			return Ok(false);
		}

		// Make sure the provided work actually comes from the correct pre_hash
		let compute = Compute {
			difficulty,
			pre_hash: *pre_hash,
			poscan_hash: seal.poscan_hash,
		};

		if compute.seal(seal.signature.clone()) != seal {
			info!(">>> verify: compute.compute() != seal");
			return Ok(false);
		}

		let pre_digest = match pre_digest {
			Some(pre_digest) => pre_digest,
			None => {
				info!(">>> verify: no pre_digest");
				return Ok(false)
			},
		};

		let author = match app::Public::decode(&mut &pre_digest[..]) {
			Ok(author) => author,
			Err(_) => {
				info!(">>> verify: decode author failed");
				return Ok(false)
			},
		};


		if !compute.verify(&seal.signature, &author) {
			// use sp_core::Public;
			info!(">>> pre_hash: {:x?}", &compute.pre_hash);
			info!(">>> seal.difficulty: {}", &seal.difficulty);
			info!(">>> seal.work: {}", &seal.work);
			info!(">>> seal.poscan_hash: {}", &seal.poscan_hash);
			info!(">>> seal signature is {:x?}", &seal.signature.to_vec());

			info!(">>> verify: miner signature is invalid");
			info!(">>> verify: miner author is {:x?}", &author.to_raw_vec());
			return Ok(false)
		}

		let h = if poscan_data.alg_id == POSCAN_ALGO_GRID2D_V3_1 { pre_hash } else { parent };
		let hashes = get_obj_hashes(&poscan_data.alg_id, &poscan_data.obj, h);
		if hashes != poscan_data.hashes {
			info!(">>> verify: hashes != poscan_data.hashes");
			return Ok(false)
		}

		Ok(true)
	}
}
