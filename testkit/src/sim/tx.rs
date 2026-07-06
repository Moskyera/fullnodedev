use basis::interface::{TransactionRead, TxExec};
use field::{Address, Amount, Hash};

#[derive(Default, Clone, Debug)]
pub struct DummyTx;

impl field::Serialize for DummyTx {
    fn size(&self) -> usize {
        0
    }

    fn serialize(&self) -> Vec<u8> {
        vec![]
    }
}

impl TxExec for DummyTx {}

impl TransactionRead for DummyTx {
    fn ty(&self) -> u8 {
        3
    }

    fn hash(&self) -> Hash {
        Hash::default()
    }

    fn hash_with_fee(&self) -> Hash {
        Hash::default()
    }

    fn main(&self) -> Address {
        Address::default()
    }

    fn addrs(&self) -> Vec<Address> {
        vec![Address::default()]
    }

    fn fee(&self) -> &Amount {
        Amount::zero_ref()
    }

    fn fee_purity(&self) -> u64 {
        1
    }

    fn gas_max_byte(&self) -> Option<u8> {
        Some(1)
    }
}

#[derive(Clone, Debug)]
pub struct StubTx {
    pub ty: u8,
    pub hash: Hash,
    pub main: Address,
    pub addrs: Vec<Address>,
    pub fee: Amount,
    pub gas_max: u8,
    pub tx_size: usize,
    pub fee_purity: u64,
}

impl Default for StubTx {
    fn default() -> Self {
        Self {
            ty: 3,
            hash: Hash::default(),
            main: Address::default(),
            addrs: vec![Address::default()],
            fee: Amount::unit238(10_000_000),
            gas_max: 17,
            tx_size: 128,
            fee_purity: 3200,
        }
    }
}

impl field::Serialize for StubTx {
    fn size(&self) -> usize {
        self.tx_size
    }

    fn serialize(&self) -> Vec<u8> {
        vec![0u8; self.tx_size]
    }
}

impl TxExec for StubTx {}

impl TransactionRead for StubTx {
    fn ty(&self) -> u8 {
        self.ty
    }

    fn hash(&self) -> Hash {
        self.hash
    }

    fn hash_with_fee(&self) -> Hash {
        self.hash
    }

    fn main(&self) -> Address {
        self.main
    }

    fn addrs(&self) -> Vec<Address> {
        self.addrs.clone()
    }

    fn fee(&self) -> &Amount {
        &self.fee
    }

    fn fee_got(&self) -> Amount {
        self.fee.clone()
    }

    fn fee_purity(&self) -> u64 {
        self.fee_purity
    }

    fn gas_max_byte(&self) -> Option<u8> {
        if self.ty < 3 {
            return None;
        }
        Some(self.gas_max)
    }
}

#[derive(Clone, Debug, Default)]
pub struct StubTxBuilder {
    tx: StubTx,
}

impl StubTxBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ty(mut self, ty: u8) -> Self {
        self.tx.ty = ty;
        self
    }

    pub fn hash(mut self, hash: Hash) -> Self {
        self.tx.hash = hash;
        self
    }

    pub fn main(mut self, main: Address) -> Self {
        self.tx.main = main;
        self
    }

    pub fn addrs(mut self, addrs: Vec<Address>) -> Self {
        self.tx.addrs = addrs;
        self
    }

    pub fn fee(mut self, fee: Amount) -> Self {
        self.tx.fee = fee;
        self
    }

    pub fn gas_max(mut self, gas_max: u8) -> Self {
        self.tx.gas_max = gas_max;
        self
    }

    pub fn tx_size(mut self, tx_size: usize) -> Self {
        self.tx.tx_size = tx_size;
        self
    }

    pub fn fee_purity(mut self, fee_purity: u64) -> Self {
        self.tx.fee_purity = fee_purity;
        self
    }

    pub fn build(self) -> StubTx {
        self.tx
    }
}
