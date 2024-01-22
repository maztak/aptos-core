// Copyright © Aptos Foundation

use crate::{
    algebra::{lagrange::lagrange_coefficients, polynomials::get_powers_of_tau},
    pvss,
    pvss::{traits::HasEncryptionPublicParams, Player, WeightedConfig},
    utils::{
        g1_multi_exp, g2_multi_exp, multi_pairing,
        random::{random_nonzero_scalar, random_scalar},
    },
    weighted_vuf::traits::WeightedVUF,
};
use anyhow::{anyhow, bail};
use blstrs::{pairing, G1Projective, G2Projective, Gt, Scalar};
use ff::Field;
use group::{Curve, Group};
use rand::thread_rng;
use serde::{Deserialize, Serialize};
use std::ops::{Mul, Neg};

pub const PINKAS_WVUF_DST: &[u8; 21] = b"APTOS_PINKAS_WVUF_DST";

pub struct PinkasWUF;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RandomizedPKs {
    pi: G1Projective,       // \hat{g}^{r}
    rks: Vec<G1Projective>, // g^{r \sk_i}, for all shares i
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicParameters {
    g: G1Projective,
    g_neg: G1Projective,
    g_hat: G2Projective,
}

impl From<&pvss::das::PublicParameters> for PublicParameters {
    fn from(pp: &pvss::das::PublicParameters) -> Self {
        let g = pp.get_encryption_public_params().message_base().clone();
        PublicParameters {
            g,
            g_neg: g.neg(),
            g_hat: pp.get_commitment_base().clone(),
        }
    }
}

/// Implements the Pinkas weighted VUF scheme, compatible with *any* PVSS scheme with the right kind
/// of secret key and public key.
impl WeightedVUF for PinkasWUF {
    type AugmentedPubKeyShare = (RandomizedPKs, Self::PubKeyShare);
    type AugmentedSecretKeyShare = (Scalar, Self::SecretKeyShare);
    // /// Note: Our BLS PKs are currently in G_1.
    // type BlsPubKey = bls12381::PublicKey;
    // type BlsSecretKey = bls12381::PrivateKey;

    type Delta = RandomizedPKs;
    type Evaluation = Gt;
    /// Naive aggregation by concatenation. It is an open problem to get constant-sized aggregation.
    type Proof = Vec<(Player, Self::ProofShare)>;
    type ProofShare = G2Projective;
    type PubKey = pvss::dealt_pub_key::g2::DealtPubKey;
    type PubKeyShare = Vec<pvss::dealt_pub_key_share::g2::DealtPubKeyShare>;
    type PublicParameters = PublicParameters;
    type SecretKey = pvss::dealt_secret_key::g1::DealtSecretKey;
    type SecretKeyShare = Vec<pvss::dealt_secret_key_share::g1::DealtSecretKeyShare>;

    fn augment_key_pair<R: rand_core::RngCore + rand_core::CryptoRng>(
        pp: &Self::PublicParameters,
        sk: Self::SecretKeyShare,
        pk: Self::PubKeyShare,
        // lsk: &Self::BlsSecretKey,
        rng: &mut R,
    ) -> (Self::AugmentedSecretKeyShare, Self::AugmentedPubKeyShare) {
        let r = random_nonzero_scalar(rng);

        let rpks = RandomizedPKs {
            pi: pp.g.mul(&r),
            rks: sk
                .iter()
                .map(|sk| sk.as_group_element().mul(&r))
                .collect::<Vec<G1Projective>>(),
        };

        ((r.invert().unwrap(), sk), (rpks, pk))
    }

    fn get_public_delta(apk: &Self::AugmentedPubKeyShare) -> &Self::Delta {
        let (rpks, _) = apk;

        rpks
    }

    fn augment_pubkey(
        pp: &Self::PublicParameters,
        pk: Self::PubKeyShare,
        // lpk: &Self::BlsPubKey,
        delta: Self::Delta,
    ) -> anyhow::Result<Self::AugmentedPubKeyShare> {
        if delta.rks.len() != pk.len() {
            bail!(
                "Expected PKs and RKs to be of the same length. Got {} and {}, respectively.",
                delta.rks.len(),
                pk.len()
            );
        }

        // TODO: Fiat-Shamir transform instead of RNG
        let tau = random_scalar(&mut thread_rng());

        let pks = pk
            .iter()
            .map(|pk| *pk.as_group_element())
            .collect::<Vec<G2Projective>>();
        let taus = get_powers_of_tau(&tau, pks.len());

        let pks_combined = g2_multi_exp(&pks[..], &taus[..]);
        let rks_combined = g1_multi_exp(&delta.rks[..], &taus[..]);

        if multi_pairing(
            [&delta.pi, &rks_combined].into_iter(),
            [&pks_combined, &pp.g_hat.neg()].into_iter(),
        ) != Gt::identity()
        {
            bail!("RPKs were not correctly randomized.");
        }

        Ok((delta, pk))
    }

    fn create_share(ask: &Self::AugmentedSecretKeyShare, msg: &[u8]) -> Self::ProofShare {
        let (r_inv, _) = ask;

        let hash = Self::hash_to_curve(msg);

        hash.mul(r_inv)
    }

    fn verify_share(
        pp: &Self::PublicParameters,
        apk: &Self::AugmentedPubKeyShare,
        msg: &[u8],
        proof: &Self::ProofShare,
    ) -> anyhow::Result<()> {
        let delta = Self::get_public_delta(apk);

        let h = Self::hash_to_curve(msg);

        if multi_pairing([&delta.pi, &pp.g_neg].into_iter(), [proof, &h].into_iter())
            != Gt::identity()
        {
            bail!("PinkasWVUF ProofShare failed to verify.");
        }

        Ok(())
    }

    fn aggregate_shares(
        _wc: &WeightedConfig,
        apks_and_proofs: &[(Player, Self::AugmentedPubKeyShare, Self::ProofShare)],
    ) -> Self::Proof {
        let mut players_and_shares = Vec::with_capacity(apks_and_proofs.len());

        for (p, _, share) in apks_and_proofs {
            players_and_shares.push((p.clone(), share.clone()));
        }

        players_and_shares
    }

    fn eval(sk: &Self::SecretKey, msg: &[u8]) -> Self::Evaluation {
        let h = Self::hash_to_curve(msg).to_affine();

        pairing(&sk.as_group_element().to_affine(), &h)
    }

    // NOTE: This VUF has the same evaluation as its proof.
    fn derive_eval(
        wc: &WeightedConfig,
        _pp: &Self::PublicParameters,
        _msg: &[u8],
        apks: &[Option<Self::AugmentedPubKeyShare>],
        proof: &Self::Proof,
    ) -> anyhow::Result<Self::Evaluation> {
        // Collect all the evaluation points associated with each player's augmented pubkey sub shares.
        let mut sub_player_ids = Vec::with_capacity(wc.get_total_weight());

        for (player, _) in proof {
            for j in 0..wc.get_player_weight(player) {
                sub_player_ids.push(wc.get_virtual_player(player, j).id);
            }
        }

        // Compute the Lagrange coefficients associated with those evaluation points
        let batch_dom = wc.get_batch_evaluation_domain();
        let lagr = lagrange_coefficients(batch_dom, &sub_player_ids[..], &Scalar::ZERO);

        // Interpolate the WVUF Proof
        let mut k = 0;
        let mut lhs = Vec::with_capacity(proof.len());
        let mut rhs = Vec::with_capacity(proof.len());
        for (player, share) in proof {
            // println!(
            //     "Flattening {} share(s) for player {player}",
            //     sub_shares.len()
            // );
            let apk = apks[player.id]
                .as_ref()
                .ok_or(anyhow!("Missing APK for player {}", player.get_id()))?;
            let rks = &apk.0.rks;
            let num_shares = rks.len();

            rhs.push(share);
            lhs.push(g1_multi_exp(&rks[..], &lagr[k..k + num_shares]));

            k += num_shares;
        }

        Ok(multi_pairing(lhs.iter().map(|r| r), rhs.into_iter()))
    }

    /// Verifies the proof shares one by one
    fn verify_proof(
        pp: &Self::PublicParameters,
        _pk: &Self::PubKey,
        apks: &[Option<Self::AugmentedPubKeyShare>],
        msg: &[u8],
        proof: &Self::Proof,
    ) -> anyhow::Result<()> {
        if proof.len() >= apks.len() {
            bail!("Number of proof shares ({}) exceeds number of APKs ({}) when verifying aggregated WVUF proof", proof.len(), apks.len());
        }

        // TODO: Fiat-Shamir transform instead of RNG
        let tau = random_scalar(&mut thread_rng());
        let taus = get_powers_of_tau(&tau, proof.len());

        // [share_i^{\tau^i}]_{i \in [0, n)}
        let shares = proof
            .iter()
            .map(|(_, share)| share)
            .zip(taus.iter())
            .map(|(share, tau)| share.mul(tau))
            .collect::<Vec<G2Projective>>();

        let mut pis = Vec::with_capacity(proof.len());
        for (player, _) in proof {
            if player.id >= apks.len() {
                bail!(
                    "Player index {} falls outside APK vector of length {}",
                    player.id,
                    apks.len()
                );
            }

            pis.push(
                apks[player.id]
                    .as_ref()
                    .ok_or(anyhow!("Missing APK for player {}", player.get_id()))?
                    .0
                    .pi,
            );
        }

        let h = Self::hash_to_curve(msg);
        let sum_of_taus: Scalar = taus.iter().sum();

        if multi_pairing(
            pis.iter().chain([pp.g_neg].iter()),
            shares.iter().chain([h.mul(sum_of_taus)].iter()),
        ) != Gt::identity()
        {
            bail!("Multipairing check in batched aggregate verification failed");
        }

        Ok(())
    }
}

impl PinkasWUF {
    fn hash_to_curve(msg: &[u8]) -> G2Projective {
        G2Projective::hash_to_curve(msg, &PINKAS_WVUF_DST[..], b"H(m)")
    }
}