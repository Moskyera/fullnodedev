use basis::interface::{ApiExecCtx, TransactionRead};
use field::{Address, BlockHeight, Hash, Hex};
use vm::ContractAddress;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedContractDeployment {
    pub main_address: Address,
    pub construct_argv: Vec<u8>,
}

pub(crate) fn verify_contract_deployment(
    ctx: &ApiExecCtx,
    expected_tx_hash: &Hash,
    deployment_height: u64,
    expected_contract: &ContractAddress,
    expected_code_sha3: &str,
) -> Option<VerifiedContractDeployment> {
    let (stored_block_hash, bytes) = ctx
        .engine
        .store()
        .block_data_by_height(&BlockHeight::from(deployment_height))?;
    let package = protocol::block::build_block_package(bytes).ok()?;
    if package.hash() != stored_block_hash || package.hein() != deployment_height {
        return None;
    }
    let matching_transactions = package
        .block_read()
        .transactions()
        .iter()
        .filter(|transaction| transaction.hash() == *expected_tx_hash)
        .collect::<Vec<_>>();
    if matching_transactions.len() != 1 {
        return None;
    }
    matching_deployment_in_transaction(
        matching_transactions[0].as_read(),
        expected_contract,
        expected_code_sha3,
    )
}

pub(crate) fn matching_deployment_in_transaction(
    transaction: &dyn TransactionRead,
    expected_contract: &ContractAddress,
    expected_code_sha3: &str,
) -> Option<VerifiedContractDeployment> {
    let matches = transaction
        .actions()
        .iter()
        .filter_map(|action| vm::action::ContractDeploy::downcast(action))
        .filter(|deploy| {
            ContractAddress::calculate(&transaction.main(), &deploy.nonce) == *expected_contract
                && deploy.contract.calc_edition().hash.to_hex() == expected_code_sha3
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return None;
    }
    Some(VerifiedContractDeployment {
        main_address: transaction.main(),
        construct_argv: matches[0].construct_argv.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis::interface::Transaction;
    use field::{Amount, Field, Uint4};
    use protocol::transaction::TransactionType3;
    use sys::Account;

    #[test]
    fn exact_deployment_returns_main_and_constructor_bytes() {
        let deployer = Account::create_by("hpay-shared-deployment-proof").unwrap();
        let main = Address::from(deployer.address().clone());
        let nonce = Uint4::from(7);
        let contract = ContractAddress::calculate(&main, &nonce);
        let source = include_str!("../../vm/contracts/hpay_channel_registry_v2.fitsh");
        let compiled = vm::fitshc::compile(source).unwrap().0.into_sto();
        let code_hash = compiled.calc_edition().hash.to_hex();
        let mut deploy = vm::action::ContractDeploy::new();
        deploy.nonce = nonce;
        deploy.construct_argv = field::BytesW2::from(vec![0x44; 32]).unwrap();
        deploy.contract = compiled;
        let mut transaction = TransactionType3::new_by(main, Amount::unit238(1), 1);
        transaction.push_action(Box::new(deploy.clone())).unwrap();

        assert_eq!(
            matching_deployment_in_transaction(&transaction, &contract, &code_hash),
            Some(VerifiedContractDeployment {
                main_address: main,
                construct_argv: vec![0x44; 32],
            })
        );
        assert!(
            matching_deployment_in_transaction(&transaction, &contract, &"ff".repeat(32)).is_none()
        );
        transaction.push_action(Box::new(deploy)).unwrap();
        assert!(
            matching_deployment_in_transaction(&transaction, &contract, &code_hash).is_none(),
            "ambiguous duplicate deployments must fail closed"
        );
    }
}
