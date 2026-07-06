combi_struct! { PosiHash,
    posi: Uint1
    hash: Hash
}

combi_list! { MerkelStuffs,
    Uint1, PosiHash
}

pub struct UnlockScript {
    codeconf: u8,
    stuff: Vec<u8>,
    witness: Vec<u8>,
}

/// Result of `scriptmh` address derivation for a P2SH lock script.
///
/// This is intended to help wallet / tooling authors compute the correct `SCRIPTMH` address
/// deterministically, without re-implementing the hashing rules and accidentally diverging
/// from consensus.
///
/// Hashing rules (same as `P2SHScriptProve::get_merkel()`):
/// - Leaf: `sha3("p2sh_leaf_" || libs || codeconf || lockbox)`
/// - Branch i: `sha3("p2sh_branch_" || left || right)` where `(left,right)` is decided by `posi`.
/// - Address: `Address::create_scriptmh(ripemd160(root_sha3))`
#[derive(Debug, Clone)]
pub struct ScriptmhCalc {
    /// Final `SCRIPTMH` address (base58 leading symbol usually `3`).
    pub address: Address,
    /// `ripemd160(root_sha3)` that becomes the address payload (20 bytes).
    pub payload20: [u8; 20],
    /// SHA3-256 chain. `sha3_path[0]` is the leaf hash, `sha3_path.last()` is the root hash.
    pub sha3_path: Vec<Hash>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct P2shEntryPayload {
    pub state_addr: Address,
    pub libs: ContractAddressW1,
    pub codes: BytesW2,
}

impl P2shEntryPayload {
    pub fn parse(payload: &[u8]) -> Ret<Self> {
        let (state_addr, mv1) = Address::create(payload)?;
        state_addr.must_scriptmh()?;
        let (libs, mv2) = ContractAddressW1::create(&payload[mv1..])?;
        let mv = mv1 + mv2;
        let codes = BytesW2::from(payload[mv..].to_vec())?;
        Ok(Self {
            state_addr,
            libs,
            codes,
        })
    }

    pub fn build(state_addr: Address, libs: ContractAddressW1, codes: BytesW2) -> Ret<Vec<u8>> {
        state_addr.must_scriptmh()?;
        let mut out = state_addr.as_bytes().to_vec();
        out.extend_from_slice(&libs.serialize());
        out.extend_from_slice(codes.as_vec());
        Ok(out)
    }

    pub fn verify_unlock_inputs(
        &self,
        block_height: u64,
        gst: &GasExtra,
        cap: &SpaceCap,
        codeconf: CodeConf,
        witness: &BytesW2,
    ) -> Ret<()> {
        P2SHScriptProve::verify_unlock_inputs(
            block_height,
            gst,
            cap,
            &self.libs,
            codeconf,
            &self.codes,
            witness,
        )
    }
}

/* pay to script hash */
action_define! { P2SHScriptProve, 46,
    ActScope::TOP, 3, false, [],
    {
        // calc hash: script + calibs
        argvkey: BytesW2 // unlock witness bytes (not executed)
        adrlibs: ContractAddressW1 // lib address list for pure and codecall
        codeconf: Uint1 // low 2 bits: CodeType, high 6 bits: reserved (must be 0)
        lockbox: BytesW2 // verify bytecodes
        merkels: MerkelStuffs
        _marks_: Fixed2
    },
    (self, "Prove P2SH unlock script".to_owned()),
    (self, ctx, _gas {
        if ctx.exec_from() != ExecFrom::Top {
            return xerrf!("P2SHScriptProve can only run in TOP context, got {}", ctx.exec_from())
        }
        if self._marks_.not_zero() {
            return xerrf!("marks bytes format invalid")
        }
        let hei = ctx.env().block.height;
        let (_, cap) = peek_vm_runtime_limits(ctx, hei);
        let p2sh_count = ctx.p2sh_count();
        if p2sh_count >= cap.p2sh_set {
            return xerrf!("p2sh_set overflow ({}>={})", p2sh_count, cap.p2sh_set)
        }
        Self::verify_merkle_depth(&cap, self.merkels.as_list().len())?;
        let adr = self.get_merkel()?;
        let stuff = self.get_stuff_with_merkel(ctx, &adr)?;
        ctx.p2sh_set(adr, Box::new(stuff))?;
        // finish
        Ok(vec![])
    })
}

impl P2sh for UnlockScript {
    fn code_conf(&self) -> u8 {
        self.codeconf
    }
    fn code_stuff(&self) -> &[u8] {
        &self.stuff
    }
    fn witness(&self) -> &[u8] {
        &self.witness
    }
}

impl P2SHScriptProve {
    /// Compute the `SCRIPTMH` address from:
    /// - `adrlibs`: the contract library allowlist used by the P2SH lock script
    /// - `codeconf`: script code config byte
    /// - `lockbox`: the P2SH lock script bytecode (as it appears in this action field)
    /// - `merkels`: the Merkle proof path (siblings + left/right positions) used to commit
    ///   the lock script into a Merkle root.
    ///
    /// This helper intentionally returns intermediate hashes (`sha3_path`) so that tools can:
    /// - debug address derivation step-by-step
    /// - display/verify the Merkle proof path
    /// - avoid subtle encoding mistakes
    ///
    /// Notes for tooling authors:
    /// - `codeconf` is hashed as one raw byte.
    /// - lockbox is hashed as raw data bytes (BytesW2::to_vec(), without length prefix)
    ///   not as a custom encoding.
    /// - Each sibling `hash` is hashed as `hash.serialize()` (type `field::Hash`).
    pub fn calc_scriptmh_from_lockbox(
        adrlibs: &ContractAddressW1,
        codeconf: CodeConf,
        lockbox: &BytesW2,
        merkels: &MerkelStuffs,
    ) -> Ret<ScriptmhCalc> {
        let mut h = Hash::from(sha3(
            vec![
                "p2sh_leaf_".as_bytes().to_vec(), // domain separator for safety
                adrlibs.serialize(),
                vec![codeconf.raw()],
                lockbox.to_vec(),
            ]
            .concat(),
        ));
        let mut path = vec![h.clone()];
        for step in merkels.as_list().iter() {
            let posi = step.posi.uint();
            if posi > 1 {
                return errf!("p2sh Merkle position {} invalid, must be 0 or 1", posi);
            }
            let ch = h.clone();
            if step.hash == ch {
                return errf!("p2sh Merkle self pair is not allowed");
            }
            // left or right: posi==0 means sibling on the left, posi==1 means sibling on the right.
            let pair = maybe!(posi == 0, [step.hash, ch], [ch, step.hash]);
            let mut buf: Vec<_> = pair.iter().map(|a| a.serialize()).collect();
            buf.insert(0, "p2sh_branch_".as_bytes().to_vec()); // domain separator for safety
            h = Hash::from(sha3(buf.concat()));
            path.push(h.clone());
        }
        let payload20 = ripemd160(h);
        Ok(ScriptmhCalc {
            address: Address::create_scriptmh(payload20),
            payload20,
            sha3_path: path,
        })
    }

    fn verify_adrlibs(cap: &SpaceCap, adrlibs: &ContractAddressW1) -> Ret<()> {
        let libs = adrlibs.as_list();
        if libs.len() > cap.library {
            return errf!("p2sh libs overflow ({}>{})", libs.len(), cap.library);
        }
        if !libs.iter().all(|a| a.is_contract()) {
            return errf!("contract libs invalid");
        }
        let mut libset = std::collections::HashSet::with_capacity(libs.len());
        for a in libs.iter() {
            if !libset.insert(a) {
                return errf!("duplicate p2sh lib address '{}'", a.to_readable());
            }
        }
        Ok(())
    }

    pub fn verify_unlock_inputs(
        block_height: u64,
        gst: &GasExtra,
        cap: &SpaceCap,
        adrlibs: &ContractAddressW1,
        codeconf: CodeConf,
        lockbox: &BytesW2,
        witness: &BytesW2,
    ) -> Ret<()> {
        Self::verify_adrlibs(cap, adrlibs)?;
        Self::verify_lockbox_bytes(cap, lockbox.as_vec())?;
        convert_and_check(cap, gst, codeconf.code_type(), lockbox.as_vec(), block_height)?;
        Self::verify_witness_bytes(cap, witness.as_vec())?;
        Ok(())
    }

    fn verify_lockbox_bytes(cap: &SpaceCap, lockbox: &[u8]) -> Ret<()> {
        if lockbox.len() > cap.p2sh_lockbox_size_max {
            return errf!(
                "p2sh lockbox bytes too long ({}>{})",
                lockbox.len(),
                cap.p2sh_lockbox_size_max
            );
        }
        Ok(())
    }

    pub(crate) fn verify_merkle_depth(cap: &SpaceCap, merkle_depth: usize) -> Ret<()> {
        if merkle_depth > cap.p2sh_merkle_depth_max {
            return errf!(
                "p2sh merkle depth overflow ({}>{})",
                merkle_depth,
                cap.p2sh_merkle_depth_max
            );
        }
        Ok(())
    }

    fn verify_witness_bytes(cap: &SpaceCap, witness: &[u8]) -> Ret<()> {
        if witness.len() > cap.value_size {
            return errf!("p2sh witness bytes too long");
        }
        Ok(())
    }

    fn get_stuff_with_merkel(&self, ctx: &mut dyn Context, scriptmh: &Address) -> Ret<UnlockScript> {
        let hei = ctx.env().block.height;
        let (gst, cap) = peek_vm_runtime_limits(ctx, hei);
        let codeconf = CodeConf::parse(self.codeconf.uint())?;
        Self::verify_unlock_inputs(hei, &gst, &cap, &self.adrlibs, codeconf, &self.lockbox, &self.argvkey)?;
        let lockbox = self.lockbox.as_vec();
        let witness = self.argvkey.as_vec().clone();
        // ok
        let merkel = scriptmh.to_vec();
        let libs = self.adrlibs.serialize();
        let mut stuff = Vec::with_capacity(merkel.len() + libs.len() + lockbox.len());
        stuff.extend_from_slice(&merkel);
        stuff.extend_from_slice(&libs);
        stuff.extend_from_slice(&lockbox);
        Ok(UnlockScript {
            codeconf: codeconf.raw(),
            stuff,
            witness,
        })
    }

    fn get_merkel(&self) -> Ret<Address> {
        let codeconf = CodeConf::parse(self.codeconf.uint())?;
        Ok(
            Self::calc_scriptmh_from_lockbox(
                &self.adrlibs,
                codeconf,
                &self.lockbox,
                &self.merkels,
            )?
            .address,
        )
    }
}

#[cfg(test)]
mod p2sh_test {
    use super::*;

    fn dummy_lockbox(byte: u8) -> BytesW2 {
        BytesW2::from(vec![Bytecode::PU8 as u8, byte, Bytecode::END as u8]).unwrap()
    }

    #[test]
    fn reject_merkel_self_pair() {
        let libs = ContractAddressW1::from_list(vec![]).unwrap();
        let lockbox = dummy_lockbox(11);
        let empty_path = MerkelStuffs::from_list(vec![]).unwrap();
        let leaf = P2SHScriptProve::calc_scriptmh_from_lockbox(
            &libs,
            CodeConf::from_type(CodeType::Bytecode),
            &lockbox,
            &empty_path,
        )
        .unwrap();
        let bad_path = MerkelStuffs::from_list(vec![PosiHash {
            posi: Uint1::from(1u8),
            hash: leaf.sha3_path[0].clone(),
        }])
        .unwrap();
        assert!(
            P2SHScriptProve::calc_scriptmh_from_lockbox(
                &libs,
                CodeConf::from_type(CodeType::Bytecode),
                &lockbox,
                &bad_path
            )
            .is_err()
        );
    }

    #[test]
    fn codeconf_affects_leaf_hash() {
        let libs = ContractAddressW1::from_list(vec![]).unwrap();
        let lockbox = dummy_lockbox(12);
        let empty_path = MerkelStuffs::from_list(vec![]).unwrap();
        let c0 = P2SHScriptProve::calc_scriptmh_from_lockbox(
            &libs,
            CodeConf::from_type(CodeType::Bytecode),
            &lockbox,
            &empty_path,
        )
        .unwrap();
        let c1 = P2SHScriptProve::calc_scriptmh_from_lockbox(
            &libs,
            CodeConf::from_type(CodeType::IRNode),
            &lockbox,
            &empty_path,
        )
        .unwrap();
        assert_ne!(c0.address, c1.address);
    }

    #[test]
    fn verify_unlock_inputs_rejects_duplicate_libs() {
        let lib = ContractAddress::from_unchecked(Address::create_contract([7u8; 20]));
        let libs = ContractAddressW1::from_list(vec![lib.clone(), lib]).unwrap();
        let lockbox = dummy_lockbox(13);
        let witness = BytesW2::from(vec![]).unwrap();
        let gst = GasExtra::new(1);
        let cap = SpaceCap::new(1);
        let err = P2SHScriptProve::verify_unlock_inputs(
            1,
            &gst,
            &cap,
            &libs,
            CodeConf::from_type(CodeType::Bytecode),
            &lockbox,
            &witness,
        )
        .unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn verify_unlock_inputs_rejects_oversized_lockbox() {
        let libs = ContractAddressW1::from_list(vec![]).unwrap();
        let lockbox = BytesW2::from(vec![0u8; SpaceCap::new(1).p2sh_lockbox_size_max + 1]).unwrap();
        let witness = BytesW2::from(vec![]).unwrap();
        let gst = GasExtra::new(1);
        let cap = SpaceCap::new(1);
        let err = P2SHScriptProve::verify_unlock_inputs(
            1,
            &gst,
            &cap,
            &libs,
            CodeConf::from_type(CodeType::Bytecode),
            &lockbox,
            &witness,
        )
        .unwrap_err();
        assert!(err.contains("lockbox bytes too long"), "{err}");
    }

    #[test]
    fn verify_merkle_depth_rejects_overflow() {
        let cap = SpaceCap::new(1);
        let err = P2SHScriptProve::verify_merkle_depth(&cap, cap.p2sh_merkle_depth_max + 1).unwrap_err();
        assert!(err.contains("merkle depth overflow"), "{err}");
    }
}
