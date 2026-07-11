/*
* Explicit extra required signers beyond intrinsic action req_sign.
* Type3 uses this as E in D = R0 ∪ E (exact SignW2 match).
* Type2 still uses at-least semantics via req_sign().
*/
action_define! { ReqSignList, 0x0414,
    ActScope::TOP_GUARD_UNIQUE, 2, false,
    self.signers.as_list(),
    {
        signers: AddrOrPtrW2
    },
    (self, format!("Require extra signers ({})", self.signers.length())),
    (self, ctx, _gas {
        // Declaration-only: validate list shape; no state mutation.
        self.validate_against(&ctx.env().tx.addrs)?;
        Ok(vec![])
    })
}

impl ReqSignList {
    pub fn create_by(signers: Vec<AddrOrPtr>) -> Ret<Self> {
        if signers.is_empty() {
            return errf!("ReqSignList cannot be empty");
        }
        Ok(Self {
            signers: AddrOrPtrW2::from_list(signers)?,
            ..Self::new()
        })
    }

    pub fn create_by_addrs(addrs: Vec<Address>) -> Ret<Self> {
        let ptrs = addrs.into_iter().map(AddrOrPtr::from_addr).collect();
        Self::create_by(ptrs)
    }

    /// Resolve and validate E: non-empty, unique, PRIVAKEY, not unknown system.
    pub fn validate_against(&self, addrs: &Vec<Address>) -> Ret<HashSet<Address>> {
        if self.signers.length() == 0 {
            return errf!("ReqSignList cannot be empty");
        }
        let mut e = HashSet::new();
        for ptr in self.signers.as_list() {
            let adr = ptr.real(addrs)?;
            if !adr.is_privakey() {
                return errf!(
                    "ReqSignList address {} must be PRIVAKEY type",
                    adr.to_readable()
                );
            }
            if adr.is_privakey_unknown() {
                return errf!(
                    "ReqSignList address {} is a system address with unknown private key",
                    adr.to_readable()
                );
            }
            if !e.insert(adr) {
                return errf!(
                    "ReqSignList address {} is duplicated",
                    adr.to_readable()
                );
            }
        }
        Ok(e)
    }
}
