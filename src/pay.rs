// RGB wallet library for smart contracts on Bitcoin & Lightning network
//
// SPDX-License-Identifier: Apache-2.0
//
// Written in 2019-2023 by
//     Dr Maxim Orlovsky <orlovsky@lnp-bp.org>
//
// Copyright (C) 2019-2023 LNP/BP Standards Association. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use bp::dbc::tapret::TapretProof;
use bp::seals::txout::{CloseMethod, ExplicitSeal};
use bp::{Outpoint, Sats, ScriptPubkey, Vout};
use bpstd::Address;
use bpwallet::{Beneficiary as BpBeneficiary, ConstructionError, PsbtMeta, TxParams};
use psbt::{CommitError, EmbedError, Psbt, RgbPsbt, TapretKeyError};
use rgbstd::containers::Transfer;
use rgbstd::interface::ContractError;
use rgbstd::invoice::{Amount, Beneficiary, InvoiceState, RgbInvoice};
use rgbstd::persistence::{
    ComposeError, ConsignError, ContractIfaceError, FasciaError, StockError, StockErrorAll,
    StockErrorMem,
};
use rgbstd::{ContractId, XChain};

use crate::{
    ContractOutpointsFilter, DescriptorRgb, RgbKeychain, Runtime, TapTweakAlreadyAssigned,
};

#[derive(Debug, Display, Error, From)]
#[display(inner)]
pub enum PayError {
    #[from]
    Composition(CompositionError),

    #[from]
    Completion(CompletionError),
}

#[derive(Debug, Display, Error, From)]
#[display(doc_comments)]
pub enum CompositionError {
    /// unspecified contract.
    NoContract,

    /// unspecified interface.
    NoIface,

    /// invoice doesn't provide information about the operation, and the used
    /// interface do not define default operation.
    NoOperation,

    /// invoice doesn't provide information about the assignment type, and the
    /// used interface do not define default assignment type.
    NoAssignment,

    /// state provided via PSBT inputs is not sufficient to cover invoice state
    /// requirements.
    InsufficientState,

    /// the invoice has expired.
    InvoiceExpired,

    /// one of the RGB assignments spent require presence of tapret output -
    /// even this is not a taproot wallet. Unable to create a valid PSBT, manual
    /// work is needed.
    TapretRequired,

    /// non-fungible state is not yet supported by the invoices.
    Unsupported,

    #[from]
    #[display(inner)]
    Construction(ConstructionError),

    #[from]
    #[display(inner)]
    Interface(ContractError),

    #[from]
    #[display(inner)]
    Embed(EmbedError),

    #[from]
    #[from(StockError)]
    #[from(StockErrorMem<ComposeError>)]
    #[from(StockErrorMem<ContractIfaceError>)]
    #[display(inner)]
    Stock(StockErrorAll),
}

#[derive(Debug, Display, Error, From)]
#[display(doc_comments)]
pub enum CompletionError {
    /// unspecified contract.
    NoContract,

    /// the provided PSBT doesn't pay any sats to the RGB beneficiary address.
    NoBeneficiaryOutput,

    /// the provided PSBT has conflicting descriptor in the taptweak output.
    InconclusiveDerivation,

    #[from]
    #[display(inner)]
    MultipleTweaks(TapTweakAlreadyAssigned),

    #[from]
    #[display(inner)]
    TapretKey(TapretKeyError),

    #[from]
    #[display(inner)]
    Commit(CommitError),

    #[from(StockErrorMem<ConsignError>)]
    #[from(StockErrorMem<FasciaError>)]
    #[display(inner)]
    Stock(StockErrorAll),
}

#[derive(Clone, PartialEq, Debug)]
pub struct TransferParams {
    pub tx: TxParams,
    pub min_amount: Sats,
}

impl TransferParams {
    pub fn with(fee: Sats, min_amount: Sats) -> Self {
        TransferParams {
            tx: TxParams::with(fee),
            min_amount,
        }
    }
}

impl Runtime {
    #[allow(clippy::result_large_err)]
    pub fn pay(
        &mut self,
        invoice: &[RgbInvoice],
        method: CloseMethod,
        params: TransferParams,
    ) -> Result<(Psbt, PsbtMeta, Vec<Transfer>), PayError> {
        let (mut psbt, meta) = self.construct_psbt(invoice, method, params)?;
        // ... here we pass PSBT around signers, if necessary
        let transfer = self.transfer(invoice, &mut psbt)?;
        Ok((psbt, meta, transfer))
    }

    #[allow(clippy::result_large_err)]
    pub fn construct_psbt(
        &mut self,
        invoices: &[RgbInvoice],
        method: CloseMethod,
        mut params: TransferParams,
    ) -> Result<(Psbt, PsbtMeta), CompositionError> {
        let mut beneficiaries = vec![];
        let mut prev_outputs = BTreeSet::new();
        for invoice in invoices {
            let contract_id = invoice.contract.ok_or(CompositionError::NoContract)?;

            let iface_name = invoice.iface.clone().ok_or(CompositionError::NoIface)?;
            let iface = self.stock().iface(iface_name.clone())?;
            let contract = self.contract_iface(contract_id, iface_name)?;
            let operation = invoice
                .operation
                .as_ref()
                .or(iface.default_operation.as_ref())
                .ok_or(CompositionError::NoOperation)?;

            let assignment_name = invoice // assignment_name: assetowner
                .assignment
                .as_ref()
                .or_else(|| {
                    iface
                    .transitions
                    .get(operation) //operation: transfer
                    .and_then(|t| t.default_assignment.as_ref())
                })
                .cloned()
                .ok_or(CompositionError::NoAssignment)?;

            let prev_output = match invoice.owned_state {
                InvoiceState::Amount(amount) => {
                    let filter = ContractOutpointsFilter {
                        contract_id,
                        filter: self,
                    };
                    let state: BTreeMap<_, Vec<Amount>> = contract
                        .fungible(assignment_name, &filter)?
                        .fold(bmap![], |mut set, a| {
                            set.entry(a.seal).or_default().push(a.state);
                            set
                        });
                    let mut state: Vec<_> = state
                        .into_iter()
                        .map(|(seal, vals)| (vals.iter().copied().sum::<Amount>(), seal, vals))
                        .collect();
                    state.sort_by_key(|(sum, _, _)| *sum);
                    let mut sum = Amount::ZERO;
                    state
                        .iter()
                        .rev()
                        .take_while(|(val, _, _)| {
                            if sum >= amount {
                                false
                            } else {
                                sum += *val;
                                true
                            }
                        })
                        .map(|(_, seal, _)| *seal)
                        .collect::<BTreeSet<_>>()
                }
                _ => return Err(CompositionError::Unsupported),
            };
            prev_outputs.extend(prev_output);
            match invoice.beneficiary.into_inner() {
                Beneficiary::BlindedSeal(_) => {}
                Beneficiary::WitnessVout(payload) => beneficiaries.push(BpBeneficiary::new(
                    Address::new(payload, invoice.address_network()),
                    params.min_amount,
                )),
            };
        }
        let prev_outpoints = prev_outputs
            .iter()
            // TODO: Support liquid
            .map(|o| o.as_reduced_unsafe())
            .map(|o| Outpoint::new(o.txid, o.vout));

        params.tx.change_keychain = RgbKeychain::for_method(method).into();
        let (mut psbt, mut meta) =
            self.wallet_mut()
                .construct_psbt(prev_outpoints, &beneficiaries, params.tx)?;

        let beneficiary_script: HashSet<_> = invoices
            .into_iter()
            .filter_map(|invoice| {
                if let Beneficiary::WitnessVout(addr) = invoice.beneficiary.into_inner() {
                    Some(addr.script_pubkey())
                } else {
                    None
                }
            })
            .collect();

        psbt.outputs_mut()
            .find(|o| o.script.is_p2tr() && !beneficiary_script.contains(&o.script))
            .map(|o| o.set_tapret_host().expect("just created"));
        // TODO: Add descriptor id to the tapret host data

        let change_script = meta
            .change_vout
            .and_then(|vout| psbt.output(vout.to_usize()))
            .map(|output| output.script.clone());
        psbt.sort_outputs_by(|output| !output.is_tapret_host())
            .expect("PSBT must be modifiable at this stage");
        if let Some(change_script) = change_script {
            for output in psbt.outputs() {
                if output.script == change_script {
                    meta.change_vout = Some(output.vout());
                    break;
                }
            }
        }

        let beneficiary_vout = beneficiary_script
            .into_iter()
            .filter_map(|s| {
                let vout = psbt
                    .outputs()
                    .find(|output| output.script == s)
                    .map(psbt::Output::vout)
                    .expect("PSBT without beneficiary address");
                debug_assert_ne!(Some(vout), meta.change_vout);
                Some(vout)
            })
            .into_iter()
            .map(|v| Some(v))
            .collect::<HashSet<_>>();

        let batch = self.compose(invoices, prev_outputs, method, beneficiary_vout, |_, _, _| {
            meta.change_vout
        })?;

        let methods = batch.close_method_set();
        if methods.has_opret_first() {
            let output = psbt.construct_output_expect(ScriptPubkey::op_return(&[]), Sats::ZERO);
            output.set_opret_host().expect("just created");
        }

        psbt.complete_construction();
        psbt.rgb_embed(batch)?;
        Ok((psbt, meta))
    }

    #[allow(clippy::result_large_err)]
    pub fn transfer(
        &mut self,
        invoices: &[RgbInvoice],
        psbt: &mut Psbt,
    ) -> Result<Vec<Transfer>, CompletionError> {
        let fascia = psbt.rgb_commit()?;
        if fascia.anchor.has_tapret() {
            let output = psbt
                .dbc_output::<TapretProof>()
                .ok_or(TapretKeyError::NotTaprootOutput)?;
            let terminal = output
                .terminal_derivation()
                .ok_or(CompletionError::InconclusiveDerivation)?;
            let tapret_commitment = output.tapret_commitment()?;
            self.wallet_mut()
                .add_tapret_tweak(terminal, tapret_commitment)?;
        }
        self.stock_mut().consume_fascia(fascia)?;
        let witness_txid = psbt.txid();

        let mut invoice_map: HashMap<ContractId, Vec<&RgbInvoice>> = Default::default();
        for i in invoices {
            let contract_id = i.contract.ok_or(ComposeError::NoContract).unwrap();
            invoice_map.entry(contract_id).or_default().push(i);
        }

        let mut transfers = vec![];
        for (contract_id, invoices) in invoice_map {
            let mut beneficiary1 = vec![];
            let mut beneficiary2 = vec![];
            for i in invoices {
                match i.beneficiary.into_inner() {
                    Beneficiary::WitnessVout(addr) => {
                        let s = addr.script_pubkey();
                        let vout = psbt
                            .outputs()
                            .position(|output| output.script == s)
                            .ok_or(CompletionError::NoBeneficiaryOutput)?;
                        let vout = Vout::from_u32(vout as u32);
                        let method = self.wallet().seal_close_method();
                        let seal = XChain::Bitcoin(ExplicitSeal::new(
                            method,
                            Outpoint::new(witness_txid, vout),
                        ));
                        beneficiary2.push(seal);
                    }
                    Beneficiary::BlindedSeal(seal) => {
                        beneficiary1.push(XChain::Bitcoin(seal));
                    }
                }
            }

            let transfer = self
                .stock()
                .transfer(contract_id, beneficiary2, beneficiary1)?;
            transfers.push(transfer);
        }

        Ok(transfers)
    }
}
