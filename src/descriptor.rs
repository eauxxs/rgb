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

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;
use std::{iter, vec};

use amplify::Wrapper;
use bp::dbc::tapret::TapretCommitment;
use bp::dbc::Method;
use bp::seals::txout::CloseMethod;
use bpstd::{
    CompressedPk, Derive, DeriveCompr, DeriveSet, DeriveXOnly, DerivedScript, Idx, IdxBase,
    IndexError, IndexParseError, KeyOrigin, Keychain, NormalIndex, TapDerivation, TapScript,
    TapTree, Terminal, XOnlyPk, XpubDerivable, XpubSpec,
};
use commit_verify::CommitVerify;
use descriptors::{Descriptor, SpkClass, StdDescr, TrKey, Wpkh};
use indexmap::IndexMap;

#[derive(Copy, Clone, Eq, PartialEq, Debug, Display, Error)]
#[display("terminal derivation {0} already has a taptweak assigned")]
pub struct TapTweakAlreadyAssigned(pub Terminal);

pub trait DescriptorRgb<K = XpubDerivable, V = ()>: Descriptor<K, V> {
    fn seal_close_method(&self) -> CloseMethod;
    fn add_tapret_tweak(
        &mut self,
        terminal: Terminal,
        tweak: TapretCommitment,
    ) -> Result<(), TapTweakAlreadyAssigned>;
}

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug, Display)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate", rename_all = "camelCase")
)]
#[repr(u8)]
pub enum RgbKeychain {
    #[display("0", alt = "0")]
    External = 0,

    #[display("1", alt = "1")]
    Internal = 1,

    #[display("9", alt = "9")]
    Rgb = 9,

    #[display("10", alt = "10")]
    Tapret = 10,
}

impl RgbKeychain {
    pub const RGB_ALL: [RgbKeychain; 2] = [RgbKeychain::Rgb, RgbKeychain::Tapret];

    pub fn contains_rgb(keychain: impl Into<Keychain>) -> bool {
        let k = keychain.into().into_inner();
        k == Self::Rgb as u8 || k == Self::Tapret as u8
    }
    pub fn is_seal(self) -> bool { self == Self::Rgb || self == Self::Tapret }

    pub const fn for_method(method: Method) -> Self {
        match method {
            Method::OpretFirst => Self::Rgb,
            Method::TapretFirst => Self::Tapret,
        }
    }
}

impl FromStr for RgbKeychain {
    type Err = IndexParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match NormalIndex::from_str(s)? {
            NormalIndex::ZERO => Ok(RgbKeychain::External),
            NormalIndex::ONE => Ok(RgbKeychain::Internal),
            val => Err(IndexError {
                what: "non-standard keychain",
                invalid: val.index(),
                start: 0,
                end: 1,
            }
            .into()),
        }
    }
}

impl From<RgbKeychain> for Keychain {
    fn from(keychain: RgbKeychain) -> Self { Keychain::from(keychain as u8) }
}

#[cfg_attr(feature = "serde", serde_as)]
#[derive(Clone, Eq, PartialEq, Debug)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate", rename_all = "camelCase")
)]
pub struct TapretKey<K: DeriveXOnly = XpubDerivable> {
    pub internal_key: K,
    // TODO: Allow multiple tweaks per index by introducing derivation using new Terminal trait
    // TODO: Change serde implementation for both Terminal and TapretCommitment
    #[cfg_attr(
        feature = "serde",
        serde_as(as = "HashMap<serde_with::DisplayFromStr, serde_with::DisplayFromStr>")
    )]
    pub tweaks: HashMap<Terminal, TapretCommitment>,
}

impl<K: DeriveXOnly> TapretKey<K> {
    pub fn new_unfunded(internal_key: K) -> Self {
        TapretKey {
            internal_key,
            tweaks: empty!(),
        }
    }
}

impl<K: DeriveXOnly> Derive<DerivedScript> for TapretKey<K> {
    #[inline]
    fn default_keychain(&self) -> Keychain { RgbKeychain::Rgb.into() }

    fn keychains(&self) -> BTreeSet<Keychain> {
        bset![
            RgbKeychain::External.into(),
            RgbKeychain::Internal.into(),
            RgbKeychain::Rgb.into(),
            RgbKeychain::Tapret.into(),
        ]
    }

    fn derive(
        &self,
        keychain: impl Into<Keychain>,
        index: impl Into<NormalIndex>,
    ) -> DerivedScript {
        let keychain = keychain.into();
        let index = index.into();
        let terminal = Terminal::new(keychain, index);
        let internal_key = self.internal_key.derive(keychain, index);
        if keychain.into_inner() == RgbKeychain::Tapret as u8 {
            if let Some(tweak) = self.tweaks.get(&terminal) {
                let script_commitment = TapScript::commit(tweak);
                let tap_tree = TapTree::with_single_leaf(script_commitment);
                return DerivedScript::TaprootScript(internal_key.into(), tap_tree);
            }
        }
        DerivedScript::TaprootKeyOnly(internal_key.into())
    }
}

impl<K: DeriveXOnly> From<K> for TapretKey<K> {
    fn from(tr: K) -> Self {
        TapretKey {
            internal_key: tr,
            tweaks: none!(),
        }
    }
}

impl<K: DeriveXOnly> From<TrKey<K>> for TapretKey<K> {
    fn from(tr: TrKey<K>) -> Self {
        TapretKey {
            internal_key: tr.into_internal_key(),
            tweaks: none!(),
        }
    }
}

impl<K: DeriveXOnly> Descriptor<K> for TapretKey<K> {
    type KeyIter<'k> = iter::Once<&'k K> where Self: 'k, K: 'k;
    type VarIter<'v> = iter::Empty<&'v ()> where Self: 'v, (): 'v;
    type XpubIter<'x> = iter::Once<&'x XpubSpec> where Self: 'x;

    fn class(&self) -> SpkClass { SpkClass::P2tr }

    fn keys(&self) -> Self::KeyIter<'_> { iter::once(&self.internal_key) }
    fn vars(&self) -> Self::VarIter<'_> { iter::empty() }
    fn xpubs(&self) -> Self::XpubIter<'_> { iter::once(self.internal_key.xpub_spec()) }

    fn compr_keyset(&self, _terminal: Terminal) -> IndexMap<CompressedPk, KeyOrigin> {
        IndexMap::new()
    }

    fn xonly_keyset(&self, terminal: Terminal) -> IndexMap<XOnlyPk, TapDerivation> {
        let mut map = IndexMap::with_capacity(1);
        let key = self.internal_key.derive(terminal.keychain, terminal.index);
        map.insert(
            key,
            TapDerivation::with_internal_pk(
                self.internal_key.xpub_spec().origin().clone(),
                terminal,
            ),
        );
        map
    }
}

impl<K: DeriveXOnly> DescriptorRgb<K> for TapretKey<K> {
    fn seal_close_method(&self) -> CloseMethod { CloseMethod::TapretFirst }

    fn add_tapret_tweak(
        &mut self,
        terminal: Terminal,
        tweak: TapretCommitment,
    ) -> Result<(), TapTweakAlreadyAssigned> {
        if self.tweaks.contains_key(&terminal) {
            return Err(TapTweakAlreadyAssigned(terminal));
        }
        self.tweaks.insert(terminal, tweak);
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, Debug, From)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(
        crate = "serde_crate",
        rename_all = "camelCase",
        bound(
            serialize = "S::Compr: serde::Serialize, S::XOnly: serde::Serialize",
            deserialize = "S::Compr: serde::Deserialize<'de>, S::XOnly: serde::Deserialize<'de>"
        )
    )
)]
#[non_exhaustive]
pub enum RgbDescr<S: DeriveSet = XpubDerivable> {
    #[from]
    Wpkh(Wpkh<S::Compr>),
    #[from]
    TapretKey(TapretKey<S::XOnly>),
}

impl<S: DeriveSet> Derive<DerivedScript> for RgbDescr<S> {
    fn default_keychain(&self) -> Keychain {
        match self {
            RgbDescr::Wpkh(d) => d.default_keychain(),
            RgbDescr::TapretKey(d) => d.default_keychain(),
        }
    }

    fn keychains(&self) -> BTreeSet<Keychain> {
        match self {
            RgbDescr::Wpkh(d) => d.keychains(),
            RgbDescr::TapretKey(d) => d.keychains(),
        }
    }

    fn derive(&self, change: impl Into<Keychain>, index: impl Into<NormalIndex>) -> DerivedScript {
        match self {
            RgbDescr::Wpkh(d) => d.derive(change, index),
            RgbDescr::TapretKey(d) => d.derive(change, index),
        }
    }
}

impl<K: DeriveSet<Compr = K, XOnly = K> + DeriveCompr + DeriveXOnly> Descriptor<K> for RgbDescr<K>
where Self: Derive<DerivedScript>
{
    type KeyIter<'k> = vec::IntoIter<&'k K> where Self: 'k, K: 'k;
    type VarIter<'v> = iter::Empty<&'v ()> where Self: 'v, (): 'v;
    type XpubIter<'x> = vec::IntoIter<&'x XpubSpec> where Self: 'x;

    fn class(&self) -> SpkClass {
        match self {
            RgbDescr::Wpkh(d) => d.class(),
            RgbDescr::TapretKey(d) => d.class(),
        }
    }

    fn keys(&self) -> Self::KeyIter<'_> {
        match self {
            RgbDescr::Wpkh(d) => d.keys().collect::<Vec<_>>(),
            RgbDescr::TapretKey(d) => d.keys().collect::<Vec<_>>(),
        }
        .into_iter()
    }

    fn vars(&self) -> Self::VarIter<'_> {
        match self {
            RgbDescr::Wpkh(d) => d.vars(),
            RgbDescr::TapretKey(d) => d.vars(),
        }
    }

    fn xpubs(&self) -> Self::XpubIter<'_> {
        match self {
            RgbDescr::Wpkh(d) => d.xpubs().collect::<Vec<_>>(),
            RgbDescr::TapretKey(d) => d.xpubs().collect::<Vec<_>>(),
        }
        .into_iter()
    }

    fn compr_keyset(&self, terminal: Terminal) -> IndexMap<CompressedPk, KeyOrigin> {
        match self {
            RgbDescr::Wpkh(d) => d.compr_keyset(terminal),
            RgbDescr::TapretKey(d) => d.compr_keyset(terminal),
        }
    }

    fn xonly_keyset(&self, terminal: Terminal) -> IndexMap<XOnlyPk, TapDerivation> {
        match self {
            RgbDescr::Wpkh(d) => d.xonly_keyset(terminal),
            RgbDescr::TapretKey(d) => d.xonly_keyset(terminal),
        }
    }
}

impl<K: DeriveSet<Compr = K, XOnly = K> + DeriveCompr + DeriveXOnly> DescriptorRgb<K>
    for RgbDescr<K>
where Self: Derive<DerivedScript>
{
    fn seal_close_method(&self) -> CloseMethod {
        match self {
            RgbDescr::Wpkh(_) => CloseMethod::OpretFirst,
            RgbDescr::TapretKey(d) => d.seal_close_method(),
        }
    }

    fn add_tapret_tweak(
        &mut self,
        terminal: Terminal,
        tweak: TapretCommitment,
    ) -> Result<(), TapTweakAlreadyAssigned> {
        match self {
            RgbDescr::Wpkh(_) => panic!("adding tapret tweak to non-taproot descriptor"),
            RgbDescr::TapretKey(d) => d.add_tapret_tweak(terminal, tweak),
        }
    }
}

impl From<StdDescr> for RgbDescr {
    fn from(descr: StdDescr) -> Self {
        match descr {
            StdDescr::Wpkh(wpkh) => RgbDescr::Wpkh(wpkh),
            StdDescr::TrKey(tr) => RgbDescr::TapretKey(tr.into()),
            _ => todo!(),
        }
    }
}
