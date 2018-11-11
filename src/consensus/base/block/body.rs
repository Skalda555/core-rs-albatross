use beserial::{Deserialize, Serialize};
use consensus::base::account::PrunedAccount;
use consensus::base::primitive::Address;
use consensus::base::primitive::hash::{Hash, HashOutput, SerializeContent};
use consensus::base::transaction::Transaction;
use consensus::networks::NetworkId;
use std::{cmp::Ordering, io};
use utils::merkle;

#[derive(Default, Clone, PartialEq, PartialOrd, Eq, Ord, Debug, Serialize, Deserialize)]
pub struct BlockBody {
    pub miner: Address,
    #[beserial(len_type(u8))]
    pub extra_data: Vec<u8>,
    #[beserial(len_type(u16))]
    pub transactions: Vec<Transaction>,
    #[beserial(len_type(u16))]
    pub pruned_accounts: Vec<PrunedAccount>,
}

impl SerializeContent for BlockBody {
    fn serialize_content<W: io::Write>(&self, writer: &mut W) -> io::Result<usize> { self.serialize(writer) }
}

impl Hash for BlockBody {
    fn hash<H: HashOutput>(&self) -> H {
        let mut vec: Vec<H> = Vec::with_capacity(2 + self.transactions.len() + self.pruned_accounts.len());
        vec.push(self.miner.hash());
        vec.push(self.extra_data.hash());
        for t in &self.transactions {
            vec.push(t.hash());
        }
        for p in &self.pruned_accounts {
            vec.push(p.hash());
        }
        return merkle::compute_root_from_hashes::<H>(&vec);
    }
}

#[allow(unreachable_code)]
impl BlockBody {
    pub(super) fn verify(&self, block_height: u32, network_id: NetworkId) -> bool {
        let mut previous_tx: Option<&Transaction> = None;
        for tx in &self.transactions {
            // Ensure transactions are ordered and unique.
            if let Some(previous) = previous_tx {
                match previous.cmp_block_order(tx) {
                    Ordering::Equal => {
                        warn!("Invalid block - duplicate transaction");
                        return false;
                    }
                    Ordering::Greater => {
                        warn!("Invalid block - transactions not ordered");
                        return false;
                    }
                    _ => (),
                }
            }
            previous_tx = Some(tx);

            // Check intrinsic transaction invariants.
            if !tx.verify(network_id) {
                warn!("Invalid block - invalid transaction");
                return false;
            }

            // Check that the transaction is within its validity window.
            if !tx.is_valid_at(block_height) {
                warn!("Invalid block - transaction outside validity window");
                return false;
            }
        }

        let mut previous_acc: Option<&PrunedAccount> = None;
        for acc in &self.pruned_accounts {
            // Ensure pruned accounts are ordered and unique.
            if let Some(previous) = previous_acc {
                match previous.cmp(acc) {
                    Ordering::Equal => {
                        warn!("Invalid block - duplicate pruned account");
                        return false;
                    }
                    Ordering::Greater => {
                        warn!("Invalid block - pruned accounts not ordered");
                        return false;
                    }
                    _ => (),
                }
            }
            previous_acc = Some(acc);

            // Check that the account is actually supposed to be pruned.
            if !acc.account.is_to_be_pruned() {
                warn!("Invalid block - invalid pruned account");
                return false;
            }
        }

        // Everything checks out.
        return true;
    }
}
