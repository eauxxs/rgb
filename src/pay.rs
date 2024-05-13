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
use std::marker::PhantomData;
use std::ops::DerefMut;

use bp::dbc::tapret::TapretProof;
use bp::seals::txout::ExplicitSeal;
use bp::{Outpoint, Sats, ScriptPubkey, Vout};
use bpstd::{psbt, Address};
use bpwallet::Wallet;
use psrgbt::{
    Beneficiary as BpBeneficiary, Psbt, PsbtConstructor, PsbtMeta, RgbPsbt, TapretKeyError,
    TxParams,
};
use rgbstd::containers::Transfer;
use rgbstd::interface::{OutpointFilter, WitnessFilter};
use rgbstd::invoice::{Amount, Beneficiary, InvoiceState, RgbInvoice};
use rgbstd::persistence::{IndexProvider, StashProvider, StateProvider, Stock};
use rgbstd::{ContractId, XChain, XOutpoint};

use crate::wallet::WalletWrapper;
use crate::{CompletionError, CompositionError, DescriptorRgb, PayError, RgbKeychain, Txid};

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

struct ContractOutpointsFilter<
    'stock,
    'wallet,
    W: WalletProvider<K> + ?Sized,
    K,
    S: StashProvider,
    H: StateProvider,
    P: IndexProvider,
> where W::Descr: DescriptorRgb<K>
{
    contract_id: ContractId,
    stock: &'stock Stock<S, H, P>,
    wallet: &'wallet W,
    _phantom: PhantomData<K>,
}

impl<
    'stock,
    'wallet,
    W: WalletProvider<K> + ?Sized,
    K,
    S: StashProvider,
    H: StateProvider,
    P: IndexProvider,
> OutpointFilter for ContractOutpointsFilter<'stock, 'wallet, W, K, S, H, P>
where W::Descr: DescriptorRgb<K>
{
    fn include_outpoint(&self, output: impl Into<XOutpoint>) -> bool {
        let output = output.into();
        if !self.wallet.filter().include_outpoint(output) {
            return false;
        }
        matches!(self.stock.contract_assignments_for(self.contract_id, [output]), Ok(list) if !list.is_empty())
    }
}

pub trait WalletProvider<K>: PsbtConstructor
where Self::Descr: DescriptorRgb<K>
{
    type Filter<'a>: Copy + WitnessFilter + OutpointFilter
    where Self: 'a;
    fn filter(&self) -> Self::Filter<'_>;
    fn descriptor_mut(&mut self) -> &mut Self::Descr;
    fn outpoints(&self) -> impl Iterator<Item = Outpoint>;
    fn txids(&self) -> impl Iterator<Item = Txid>;

    #[allow(clippy::result_large_err)]
    fn pay<S: StashProvider, H: StateProvider, P: IndexProvider>(
        &mut self,
        stock: &mut Stock<S, H, P>,
        invoice: &[RgbInvoice],
        params: TransferParams,
    ) -> Result<(Psbt, PsbtMeta, Vec<Transfer>), PayError> {
        let (mut psbt, meta) = self.construct_psbt_rgb(stock, invoice, params)?;
        // ... here we pass PSBT around signers, if necessary
        let transfer = self.transfer(stock, invoice, &mut psbt)?;
        Ok((psbt, meta, transfer))
    }

    #[allow(clippy::result_large_err)]
    fn construct_psbt_rgb<S: StashProvider, H: StateProvider, P: IndexProvider>(
        &mut self,
        stock: &Stock<S, H, P>,
        invoices: &[RgbInvoice],
        mut params: TransferParams,
    ) -> Result<(Psbt, PsbtMeta), CompositionError> {
        let mut beneficiaries = vec![];
        let mut prev_outputs = BTreeSet::new();
        let method = self.descriptor().seal_close_method();
        for invoice in invoices {
            let contract_id = invoice.contract.ok_or(CompositionError::NoContract)?;

            let iface_name = invoice.iface.clone().ok_or(CompositionError::NoIface)?;
            let iface = stock.iface(iface_name.clone()).map_err(|e| e.to_string())?;
            let contract = stock
                .contract_iface(contract_id, iface_name)
                .map_err(|e| e.to_string())?;
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
                        stock,
                        wallet: self,
                        _phantom: PhantomData,
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
            self.construct_psbt(prev_outpoints, &beneficiaries, params.tx)?;

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

        let batch = stock
            .compose(invoices, prev_outputs, method, beneficiary_vout, |_, _, _| meta.change_vout)
            .map_err(|e| e.to_string())?;

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
    fn transfer<S: StashProvider, H: StateProvider, P: IndexProvider>(
        &mut self,
        stock: &mut Stock<S, H, P>,
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
            self.descriptor_mut()
                .add_tapret_tweak(terminal, tapret_commitment)?;
        }
        stock.consume_fascia(fascia).map_err(|e| e.to_string())?;
        let witness_txid = psbt.txid();

        let mut invoice_map: HashMap<ContractId, Vec<&RgbInvoice>> = Default::default();
        for i in invoices {
            let contract_id = i.contract.ok_or(CompletionError::NoContract)?;
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
                        let method = self.descriptor().seal_close_method();
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

            let transfer = stock
                .transfer(contract_id, beneficiary2, beneficiary1)
                .map_err(|e| e.to_string())?;
            transfers.push(transfer);
        }

        Ok(transfers)
    }
}

impl<K, D: DescriptorRgb<K>> WalletProvider<K> for Wallet<K, D> {
    type Filter<'a> = WalletWrapper<'a, K, D> where Self: 'a;
    fn filter(&self) -> Self::Filter<'_> { WalletWrapper(self) }
    fn descriptor_mut(&mut self) -> &mut Self::Descr { self.deref_mut() }
    fn outpoints(&self) -> impl Iterator<Item = Outpoint> { self.coins().map(|coin| coin.outpoint) }
    fn txids(&self) -> impl Iterator<Item = Txid> { self.transactions().keys().copied() }
}
